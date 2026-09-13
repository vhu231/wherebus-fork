//! 管理控制台（网页端）：首次进入设置口令，之后用口令登录。
//!
//! 与 Telegram 身份无关：口令的 PBKDF2 哈希存在 SQLite 的 meta 表里，会话令牌只存在内存中。
//! 控制台会暴露全站用户数据，务必只在 HTTPS 或受信网络下对外提供。
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::bot::app::App;
use crate::bot::auth::{AdminSessions, hash_password, verify_password};
use crate::bot::miniapp::{ApiError, ApiResult, alerts_json};
use crate::bot::store::{AlertField, Store, now_secs};

/// 口令哈希在 meta 表里的键。
const PASSWORD_KEY: &str = "admin_password";
/// 口令最短长度。
const MIN_PASSWORD_LEN: usize = 8;

#[derive(Clone)]
pub struct Console {
    store: Arc<Store>,
    /// 机器人没启动时仍可进控制台，只是没有机器人相关的数据
    bot: Option<Arc<App>>,
    sessions: Arc<AdminSessions>,
}

pub fn router(store: Arc<Store>, bot: Option<Arc<App>>) -> Router {
    let console = Console {
        store,
        bot,
        sessions: Arc::new(AdminSessions::default()),
    };
    Router::new()
        .route("/admin", get(page))
        .route("/admin.js", get(script))
        .route("/api/console/status", get(status))
        .route("/api/console/setup", post(setup))
        .route("/api/console/login", post(login))
        .route("/api/console/logout", post(logout))
        .route("/api/console/password", post(change_password))
        .route("/api/console/overview", get(overview))
        .route("/api/console/users", get(list_users))
        .route("/api/console/users/action", post(user_action))
        .route("/api/console/settings", post(save_settings))
        .with_state(console)
}

async fn page() -> Html<&'static str> {
    Html(include_str!("../../../web/admin.html"))
}

async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../../../web/admin.js"),
    )
}

// ─── 口令与会话 ───

impl Console {
    fn password_hash(&self) -> Option<String> {
        self.store.meta_get(PASSWORD_KEY)
    }

    fn authorize(&self, headers: &HeaderMap) -> Result<(), ApiError> {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
            .unwrap_or_default();
        if token.is_empty() || !self.sessions.valid(token) {
            return Err(ApiError(
                StatusCode::UNAUTHORIZED,
                "请先登录管理控制台".into(),
            ));
        }
        Ok(())
    }
}

async fn status(State(console): State<Console>) -> ApiResult {
    Ok(Json(json!({
        "initialized": console.password_hash().is_some(),
        "bot_running": console.bot.is_some(),
        "min_password_len": MIN_PASSWORD_LEN,
    })))
}

#[derive(Deserialize)]
struct PasswordBody {
    password: String,
}

/// 首次进入时设置口令；已经设置过就拒绝（改口令要走 /api/console/password）。
async fn setup(State(console): State<Console>, Json(body): Json<PasswordBody>) -> ApiResult {
    if console.password_hash().is_some() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "管理口令已经设置过了，请直接登录".into(),
        ));
    }
    check_strength(&body.password)?;
    console
        .store
        .meta_set(PASSWORD_KEY, &hash_password(&body.password))
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    Ok(Json(json!({ "token": console.sessions.issue() })))
}

async fn login(State(console): State<Console>, Json(body): Json<PasswordBody>) -> ApiResult {
    let Some(stored) = console.password_hash() else {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "还没有设置管理口令，请先完成首次设置".into(),
        ));
    };
    if !verify_password(&body.password, &stored) {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "口令不正确".into()));
    }
    Ok(Json(json!({ "token": console.sessions.issue() })))
}

async fn logout(State(console): State<Console>, headers: HeaderMap) -> ApiResult {
    if let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    {
        console.sessions.revoke(token.trim());
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct ChangePasswordBody {
    current: String,
    new_password: String,
}

async fn change_password(
    State(console): State<Console>,
    headers: HeaderMap,
    Json(body): Json<ChangePasswordBody>,
) -> ApiResult {
    console.authorize(&headers)?;
    let stored = console
        .password_hash()
        .ok_or_else(|| ApiError(StatusCode::CONFLICT, "还没有设置管理口令".into()))?;
    if !verify_password(&body.current, &stored) {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "当前口令不正确".into()));
    }
    check_strength(&body.new_password)?;
    console
        .store
        .meta_set(PASSWORD_KEY, &hash_password(&body.new_password))
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    // 改完口令，其他地方登录的会话一律失效
    console.sessions.revoke_all();
    Ok(Json(json!({ "token": console.sessions.issue() })))
}

fn check_strength(password: &str) -> Result<(), ApiError> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("口令至少 {MIN_PASSWORD_LEN} 位"),
        ));
    }
    Ok(())
}

// ─── 站点数据 ───

async fn overview(State(console): State<Console>, headers: HeaderMap) -> ApiResult {
    console.authorize(&headers)?;
    let users = console.store.all_users();
    let favorites: usize = users.iter().map(|(_, user)| user.favorites.len()).sum();
    let queries: u64 = users.iter().map(|(_, user)| user.queries as u64).sum();
    let banned = users.iter().filter(|(_, user)| user.banned).count();
    let watching = users.iter().filter(|(_, user)| user.watch.is_some()).count();
    let settings = console.store.settings();

    Ok(Json(json!({
        "bot": console.bot.as_ref().map(|bot| bot.bot_username()),
        "bot_running": console.bot.is_some(),
        "started_at": console.bot.as_ref().map(|bot| bot.started_at()),
        "uptime_secs": console.bot.as_ref().map(|bot| now_secs().saturating_sub(bot.started_at())),
        "tz_offset": console.bot.as_ref().map(|bot| bot.tz_offset()),
        "database": console.store.path(),
        "users": users.len(),
        "banned": banned,
        "favorites": favorites,
        "queries": queries,
        "watches_active": console.bot.as_ref().map_or(watching, |bot| bot.active_watch_count()),
        "console_sessions": console.sessions.active(),
        "settings": {
            "allow_new_users": settings.allow_new_users,
            "defaults": alerts_json(&settings.defaults, false),
        },
    })))
}

async fn list_users(State(console): State<Console>, headers: HeaderMap) -> ApiResult {
    console.authorize(&headers)?;
    let defaults = console.store.settings().defaults;
    let users: Vec<Value> = console
        .store
        .all_users()
        .into_iter()
        .map(|(id, user)| {
            let histogram = user.hour_histogram();
            let peak = histogram
                .iter()
                .enumerate()
                .max_by_key(|(_, count)| **count)
                .filter(|(_, count)| **count > 0)
                .map(|(hour, _)| hour as u32);
            let alerts = user.alerts.unwrap_or(defaults).clamped();

            json!({
                "id": id,
                "name": user.display_name,
                "username": user.username,
                "city": user.city_label,
                "service": user.service,
                "queries": user.queries,
                "last_seen": user.last_seen,
                "first_seen": user.first_seen,
                "banned": user.banned,
                // 只说明有没有定位记录，不把坐标摆到管理界面上
                "has_location": user.last_location.is_some(),
                "favorites": user.favorites.len(),
                "favorites_detail": user.favorites.iter().map(|favorite| json!({
                    "line_name": favorite.line_name,
                    "station_name": favorite.station_name,
                    "city_label": favorite.city_label,
                    "order": favorite.order,
                    "hits": favorite.hits,
                    "added_at": favorite.added_at,
                })).collect::<Vec<_>>(),
                "habits": user.top_habits(5).iter().map(|habit| json!({
                    "label": habit.label,
                    "total": habit.total,
                    "last_at": habit.last_at,
                })).collect::<Vec<_>>(),
                "habit_lines": user.habits.len(),
                "peak_hour": peak,
                "hour_histogram": histogram,
                "alerts": alerts_json(&alerts, user.alerts.is_none()),
                "watch": user.watch.as_ref().map(|watch| json!({
                    "label": watch.label(),
                    "line_name": watch.line_name,
                    "station_name": watch.station_name,
                    "order": watch.order,
                    "target_bus": watch.target_bus,
                    "city_label": watch.city_label,
                    "started_at": watch.started_at,
                })),
            })
        })
        .collect();
    Ok(Json(json!({ "users": users })))
}

#[derive(Deserialize)]
struct UserActionBody {
    user_id: i64,
    /// ban | unban | stop_watch | delete
    action: String,
}

async fn user_action(
    State(console): State<Console>,
    headers: HeaderMap,
    Json(body): Json<UserActionBody>,
) -> ApiResult {
    console.authorize(&headers)?;
    if !console.store.exists(body.user_id) {
        return Err(ApiError(StatusCode::NOT_FOUND, "没有这个用户".into()));
    }

    match body.action.as_str() {
        "ban" => {
            console
                .store
                .update(body.user_id, |user| user.banned = true);
            if let Some(bot) = &console.bot {
                bot.sessions().revoke_user(body.user_id);
                bot.stop_watch(body.user_id, "账号已被管理员停用。").await;
            }
        }
        "unban" => {
            console
                .store
                .update(body.user_id, |user| user.banned = false);
        }
        "stop_watch" => {
            let stopped = match &console.bot {
                Some(bot) => bot.stop_watch(body.user_id, "管理员停止了这次盯车。").await,
                // 机器人没启动时只清掉库里的残留状态
                None => console
                    .store
                    .update(body.user_id, |user| user.watch.take().is_some()),
            };
            return Ok(Json(json!({"ok": true, "stopped": stopped})));
        }
        "delete" => {
            if let Some(bot) = &console.bot {
                bot.stop_watch(body.user_id, "账号数据已删除，盯车结束。")
                    .await;
                bot.sessions().revoke_user(body.user_id);
            }
            console.store.remove(body.user_id);
        }
        other => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("不支持的操作：{other}"),
            ));
        }
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct SettingsBody {
    #[serde(default)]
    allow_new_users: Option<bool>,
    #[serde(default)]
    values: std::collections::HashMap<String, u64>,
}

async fn save_settings(
    State(console): State<Console>,
    headers: HeaderMap,
    Json(body): Json<SettingsBody>,
) -> ApiResult {
    console.authorize(&headers)?;
    let mut unknown: Vec<String> = Vec::new();
    console.store.update_settings(|settings| {
        if let Some(allow) = body.allow_new_users {
            settings.allow_new_users = allow;
        }
        for (key, value) in &body.values {
            match AlertField::parse(key) {
                Some(field) => {
                    settings.defaults.set(field, *value);
                }
                None => unknown.push(key.clone()),
            }
        }
    });
    if !unknown.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("未知的设置项：{}", unknown.join("、")),
        ));
    }
    let settings = console.store.settings();
    Ok(Json(json!({
        "allow_new_users": settings.allow_new_users,
        "defaults": alerts_json(&settings.defaults, false),
    })))
}
