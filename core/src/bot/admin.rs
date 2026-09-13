//! 管理后台：Telegram Mini App 页面 + 用户管理 / Bot 管理的 HTTP 接口。
//!
//! 身份只来自 Mini App 的 initData 签名（见 [`crate::bot::auth`]），前端传来的
//! 用户 ID 一律不信任。普通用户只能读写自己的数据；管理员（环境变量
//! `WHEREBUS_BOT_ADMINS` 列出的 Telegram ID）才能看到全局状态与用户列表。
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::bot::app::App;
use crate::bot::auth::{Session, verify_init_data};
use crate::bot::store::{AlertField, AlertSettings, now_secs};

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

/// 启动管理后台 HTTP 服务；同时挂上网页版，方便单进程部署。
pub async fn serve(app: Arc<App>) -> anyhow::Result<()> {
    let bind = std::env::var("WHEREBUS_BOT_WEB_BIND")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "127.0.0.1:8081".into());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    let address = listener.local_addr()?;
    println!("[bot] 管理后台：http://{address}/miniapp（网页版：http://{address}/）");

    let router = crate::web::router().merge(router(app));
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            eprintln!("[bot] 管理后台已退出：{error}");
        }
    });
    Ok(())
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/miniapp", get(page))
        .route("/miniapp.js", get(script))
        .route("/api/auth/telegram", post(login))
        .route("/api/me", get(me).delete(forget_me))
        .route("/api/me/settings", post(save_settings))
        .route("/api/me/favorites/delete", post(delete_favorite))
        .route("/api/me/watch/stop", post(stop_my_watch))
        .route("/api/admin/overview", get(overview))
        .route("/api/admin/users", get(list_users))
        .route("/api/admin/users/action", post(user_action))
        .route("/api/admin/settings", post(save_global_settings))
        .with_state(app)
}

async fn page() -> Html<&'static str> {
    Html(include_str!("../../../web/miniapp.html"))
}

async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../../../web/miniapp.js"),
    )
}

// ─── 鉴权 ───

fn session(app: &App, headers: &HeaderMap) -> Result<Session, ApiError> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "缺少会话令牌，请重新登录".into()))?;
    let session = app
        .sessions()
        .lookup(token)
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "会话已过期，请重新登录".into()))?;
    if app.store().get(session.user.id).banned {
        return Err(ApiError(StatusCode::FORBIDDEN, "账号已被停用".into()));
    }
    Ok(session)
}

fn admin_session(app: &App, headers: &HeaderMap) -> Result<Session, ApiError> {
    let session = session(app, headers)?;
    if !session.is_admin {
        return Err(ApiError(StatusCode::FORBIDDEN, "需要管理员权限".into()));
    }
    Ok(session)
}

// ─── 登录 ───

#[derive(Deserialize)]
struct LoginBody {
    /// Telegram Mini App 的 window.Telegram.WebApp.initData 原文
    init_data: String,
}

async fn login(State(app): State<Arc<App>>, Json(body): Json<LoginBody>) -> ApiResult {
    let user = verify_init_data(&body.init_data, app.bot_token(), now_secs())
        .map_err(|error| ApiError(StatusCode::UNAUTHORIZED, error.to_string()))?;

    if app.store().get(user.id).banned {
        return Err(ApiError(StatusCode::FORBIDDEN, "账号已被停用".into()));
    }
    if !app.store().settings().allow_new_users && !app.store().exists(user.id) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "机器人当前不接受新用户".into(),
        ));
    }

    let name = user.name.clone();
    let username = user.username.clone();
    app.store().update(user.id, |state| {
        state.display_name = name;
        state.username = username;
    });

    let (token, session) = app.sessions().issue(user);
    Ok(Json(json!({
        "token": token,
        "user": session.user,
        "is_admin": session.is_admin,
        "bot": app.bot_username(),
    })))
}

// ─── 用户自己的数据 ───

fn alerts_json(alerts: &AlertSettings, using_defaults: bool) -> Value {
    json!({
        "using_defaults": using_defaults,
        "values": {
            "poll_secs": alerts.poll_secs,
            "alert_stations": alerts.alert_stations,
            "alert_distance_m": alerts.alert_distance_m,
            "repeat_secs": alerts.repeat_secs,
            "max_minutes": alerts.max_minutes,
        },
        "fields": AlertField::ALL.map(|field| {
            let (min, max) = field.range();
            json!({
                "key": field.key(),
                "label": field.label(),
                "unit": field.unit(),
                "step": field.step(),
                "min": min,
                "max": max,
            })
        }).to_vec(),
    })
}

fn user_payload(app: &App, user_id: i64, is_admin: bool) -> Value {
    let state = app.store().get(user_id);
    let alerts = app.store().alerts_for(user_id);
    json!({
        "id": user_id,
        "name": state.display_name,
        "username": state.username,
        "is_admin": is_admin,
        "city": state.city_label,
        "queries": state.queries,
        "first_seen": state.first_seen,
        "last_seen": state.last_seen,
        "alerts": alerts_json(&alerts, state.alerts.is_none()),
        "favorites": state.favorites.iter().map(|favorite| json!({
            "service": favorite.service,
            "direction": favorite.direction,
            "order": favorite.order,
            "line_name": favorite.line_name,
            "station_name": favorite.station_name,
            "city_label": favorite.city_label,
            "hits": favorite.hits,
        })).collect::<Vec<_>>(),
        "habits": state.top_habits(10).iter().map(|habit| json!({
            "label": habit.label,
            "total": habit.total,
            "hours": habit.hours,
        })).collect::<Vec<_>>(),
        "hour_histogram": state.hour_histogram(),
        "watch": state.watch.as_ref().map(|watch| json!({
            "label": watch.label(),
            "line_name": watch.line_name,
            "station_name": watch.station_name,
            "order": watch.order,
            "target_bus": watch.target_bus,
            "started_at": watch.started_at,
        })),
    })
}

async fn me(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    let session = session(&app, &headers)?;
    Ok(Json(user_payload(&app, session.user.id, session.is_admin)))
}

#[derive(Deserialize)]
struct SettingsBody {
    #[serde(default)]
    reset: bool,
    #[serde(default)]
    values: std::collections::HashMap<String, u64>,
}

async fn save_settings(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<SettingsBody>,
) -> ApiResult {
    let session = session(&app, &headers)?;
    if body.reset {
        app.store()
            .update(session.user.id, |user| user.alerts = None);
        return Ok(Json(user_payload(&app, session.user.id, session.is_admin)));
    }

    let defaults = app.store().settings().defaults;
    let mut unknown: Vec<String> = Vec::new();
    app.store().update(session.user.id, |user| {
        let mut alerts = user.alerts.unwrap_or(defaults);
        for (key, value) in &body.values {
            match AlertField::parse(key) {
                // set 内部会夹到合法区间，越界的值不会被写入
                Some(field) => {
                    alerts.set(field, *value);
                }
                None => unknown.push(key.clone()),
            }
        }
        user.alerts = Some(alerts);
    });
    if !unknown.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("未知的设置项：{}", unknown.join("、")),
        ));
    }
    Ok(Json(user_payload(&app, session.user.id, session.is_admin)))
}

#[derive(Deserialize)]
struct FavoriteBody {
    service: String,
    direction: String,
    order: u32,
}

async fn delete_favorite(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<FavoriteBody>,
) -> ApiResult {
    let session = session(&app, &headers)?;
    let removed = app.store().update(session.user.id, |user| {
        match user.favorite_index(&body.service, &body.direction, body.order) {
            Some(index) => {
                user.favorites.remove(index);
                true
            }
            None => false,
        }
    });
    if !removed {
        return Err(ApiError(StatusCode::NOT_FOUND, "没有这条收藏".into()));
    }
    Ok(Json(user_payload(&app, session.user.id, session.is_admin)))
}

async fn stop_my_watch(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    let session = session(&app, &headers)?;
    let stopped = app
        .stop_watch(session.user.id, "你在管理面板里停止了盯车。")
        .await;
    Ok(Json(json!({"stopped": stopped})))
}

async fn forget_me(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    let session = session(&app, &headers)?;
    app.stop_watch(session.user.id, "账号数据已删除，盯车结束。")
        .await;
    let removed = app.store().remove(session.user.id);
    app.sessions().revoke_user(session.user.id);
    Ok(Json(json!({"removed": removed})))
}

// ─── Bot 管理（仅管理员） ───

async fn overview(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    admin_session(&app, &headers)?;
    let users = app.store().all_users();
    let favorites: usize = users.iter().map(|(_, user)| user.favorites.len()).sum();
    let queries: u64 = users.iter().map(|(_, user)| user.queries as u64).sum();
    let banned = users.iter().filter(|(_, user)| user.banned).count();
    let settings = app.store().settings();

    Ok(Json(json!({
        "bot": app.bot_username(),
        "started_at": app.started_at(),
        "uptime_secs": now_secs().saturating_sub(app.started_at()),
        "tz_offset": app.tz_offset(),
        "data_file": app.store().path().display().to_string(),
        "users": users.len(),
        "banned": banned,
        "favorites": favorites,
        "queries": queries,
        "watches_active": app.active_watch_count(),
        "sessions_active": app.sessions().active(),
        "admins": app.sessions().admin_count(),
        "settings": {
            "allow_new_users": settings.allow_new_users,
            "defaults": alerts_json(&settings.defaults, false),
        },
    })))
}

async fn list_users(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    admin_session(&app, &headers)?;
    let users: Vec<Value> = app
        .store()
        .all_users()
        .into_iter()
        .map(|(id, user)| {
            json!({
                "id": id,
                "name": user.display_name,
                "username": user.username,
                "city": user.city_label,
                "favorites": user.favorites.len(),
                "queries": user.queries,
                "last_seen": user.last_seen,
                "first_seen": user.first_seen,
                "banned": user.banned,
                "is_admin": app.sessions().is_admin(id),
                "watch": user.watch.as_ref().map(|watch| watch.label()),
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
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<UserActionBody>,
) -> ApiResult {
    let session = admin_session(&app, &headers)?;
    if !app.store().exists(body.user_id) {
        return Err(ApiError(StatusCode::NOT_FOUND, "没有这个用户".into()));
    }
    // 管理员不能把自己或其他管理员停用/删除，避免误操作锁死后台
    let protected = app.sessions().is_admin(body.user_id);

    match body.action.as_str() {
        "ban" => {
            if protected {
                return Err(ApiError(StatusCode::BAD_REQUEST, "不能停用管理员".into()));
            }
            app.store().update(body.user_id, |user| user.banned = true);
            app.sessions().revoke_user(body.user_id);
            app.stop_watch(body.user_id, "账号已被管理员停用。").await;
        }
        "unban" => {
            app.store().update(body.user_id, |user| user.banned = false);
        }
        "stop_watch" => {
            let stopped = app
                .stop_watch(body.user_id, "管理员停止了这次盯车。")
                .await;
            return Ok(Json(json!({"ok": true, "stopped": stopped})));
        }
        "delete" => {
            if protected && body.user_id != session.user.id {
                return Err(ApiError(StatusCode::BAD_REQUEST, "不能删除其他管理员".into()));
            }
            app.stop_watch(body.user_id, "账号数据已删除，盯车结束。")
                .await;
            app.store().remove(body.user_id);
            app.sessions().revoke_user(body.user_id);
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
struct GlobalSettingsBody {
    #[serde(default)]
    allow_new_users: Option<bool>,
    #[serde(default)]
    values: std::collections::HashMap<String, u64>,
}

async fn save_global_settings(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<GlobalSettingsBody>,
) -> ApiResult {
    admin_session(&app, &headers)?;
    let mut unknown: Vec<String> = Vec::new();
    app.store().update_settings(|settings| {
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
    let settings = app.store().settings();
    Ok(Json(json!({
        "allow_new_users": settings.allow_new_users,
        "defaults": alerts_json(&settings.defaults, false),
    })))
}
