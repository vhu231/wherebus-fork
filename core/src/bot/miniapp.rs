//! 用户面板（Telegram Mini App）：管理自己的偏好、收藏与车次监控。
//!
//! 身份只来自 Mini App 的 initData 签名（见 [`crate::bot::auth`]），前端传来的
//! 用户 ID 一律不信任。这里的每个接口都只能读写登录者本人的数据；
//! 站点级别的管理功能在网页端控制台（见 [`crate::bot::console`]）。
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
use crate::provider;

pub(crate) struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

pub(crate) type ApiResult = Result<Json<Value>, ApiError>;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/miniapp", get(page))
        .route("/miniapp.js", get(script))
        .route("/api/auth/telegram", post(login))
        .route("/api/me", get(me).delete(forget_me))
        .route("/api/me/settings", post(save_settings))
        .route("/api/me/city", post(set_city))
        .route("/api/me/favorites/delete", post(delete_favorite))
        .route("/api/me/watch/start", post(start_watch))
        .route("/api/me/watch/stop", post(stop_watch))
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
        "bot": app.bot_username(),
    })))
}

// ─── 个人资料与偏好 ───

pub(crate) fn alerts_json(alerts: &AlertSettings, using_defaults: bool) -> Value {
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

fn user_payload(app: &App, user_id: i64) -> Value {
    let state = app.store().get(user_id);
    let alerts = app.store().alerts_for(user_id);
    json!({
        "id": user_id,
        "name": state.display_name,
        "username": state.username,
        "service": state.service,
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
            "service": habit.service,
            "direction": habit.direction,
            "order": habit.order,
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
    Ok(Json(user_payload(&app, session.user.id)))
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
        return Ok(Json(user_payload(&app, session.user.id)));
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
    Ok(Json(user_payload(&app, session.user.id)))
}

#[derive(Deserialize)]
struct CityBody {
    service: String,
}

async fn set_city(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<CityBody>,
) -> ApiResult {
    let session = session(&app, &headers)?;
    let label = service_label(&body.service).ok_or_else(|| {
        ApiError(StatusCode::BAD_REQUEST, "请选择有效的城市与数据源".into())
    })?;
    app.store().update(session.user.id, |user| {
        user.service = Some(body.service.clone());
        user.city_label = Some(label.clone());
    });
    Ok(Json(user_payload(&app, session.user.id)))
}

/// 数据源 id → 「城市 · 数据源」；Debug 数据源不对外开放。
fn service_label(service: &str) -> Option<String> {
    provider::available_services()
        .into_iter()
        .find(|entry| entry.id == service && entry.provider != "Debug")
        .map(|entry| format!("{} · {}", entry.city.name(), entry.provider))
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
    Ok(Json(user_payload(&app, session.user.id)))
}

// ─── 车次监控 ───

#[derive(Deserialize)]
struct WatchBody {
    /// 省略时用用户当前选择的城市
    #[serde(default)]
    service: Option<String>,
    direction: String,
    order: u32,
    /// 指定车辆编号；省略表示盯「最近的一班」
    #[serde(default)]
    bus: Option<String>,
}

async fn start_watch(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<WatchBody>,
) -> ApiResult {
    let session = session(&app, &headers)?;
    let user_id = session.user.id;
    let service = body
        .service
        .or_else(|| app.store().get(user_id).service)
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "还没有选择城市".into()))?;
    if service_label(&service).is_none() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "请选择有效的城市与数据源".into(),
        ));
    }
    if body.order == 0 || body.order > 10_000 {
        return Err(ApiError(StatusCode::BAD_REQUEST, "站序无效".into()));
    }

    // 私聊的 chat_id 就是用户 id：卡片与提醒都发到用户与机器人的私聊里
    app.start_watch(
        user_id,
        user_id,
        &service,
        &body.direction,
        body.order,
        body.bus,
    )
    .await;

    let started = app.store().get(user_id).watch.is_some();
    if !started {
        return Err(ApiError(
            StatusCode::BAD_GATEWAY,
            "盯车没能启动，请回到机器人里查看提示（可能是线路站点已调整或数据源不可用）".into(),
        ));
    }
    Ok(Json(user_payload(&app, user_id)))
}

async fn stop_watch(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    let session = session(&app, &headers)?;
    let stopped = app
        .stop_watch(session.user.id, "你在用户面板里停止了盯车。")
        .await;
    Ok(Json(json!({
        "stopped": stopped,
        "me": user_payload(&app, session.user.id),
    })))
}

async fn forget_me(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    let session = session(&app, &headers)?;
    app.stop_watch(session.user.id, "账号数据已删除，盯车结束。")
        .await;
    let removed = app.store().remove(session.user.id);
    app.sessions().revoke_user(session.user.id);
    Ok(Json(json!({ "removed": removed })))
}
