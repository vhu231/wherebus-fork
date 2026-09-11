//! Stateless HTTP adapter: no Android bridge, device storage, or shared city selection.
use crate::provider::{self, BusDataProvider};
use axum::{
    Json, Router,
    extract::Query,
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

pub fn router() -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../../web/index.html")) }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../../web/app.js"),
                )
            }),
        )
        .route(
            "/bus-view.js",
            get(|| async {
                ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../../web/bus-view.js"))
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../../web/style.css"),
                )
            }),
        )
        .route(
            "/api/health",
            get(|| async { Json(json!({"status":"ok"})) }),
        )
        .route("/api/services", get(services))
        .route("/api/lines", get(lines))
        .route("/api/nearby", get(nearby))
        .route("/api/station-lines", get(station_lines))
        .route("/api/line", get(line))
        .route("/api/realtime", get(realtime))
        .fallback(|| async { ApiError(StatusCode::NOT_FOUND, "接口不存在".into()) })
}

struct ApiError(StatusCode, String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}
type ApiResult = Result<Json<Value>, ApiError>;

#[derive(Deserialize)]
struct Params {
    service: String,
    q: Option<String>,
    lat: Option<f64>,
    lng: Option<f64>,
    station: Option<String>,
    direction: Option<String>,
    order: Option<u32>,
}
impl Params {
    fn provider(&self) -> Result<Arc<dyn BusDataProvider>, ApiError> {
        if !provider::available_services()
            .iter()
            .any(|s| s.id == self.service && s.provider != "Debug")
        {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "请选择有效的城市与数据源".into(),
            ));
        }
        Ok(provider::create_provider(&self.service))
    }
    fn coords(&self) -> Result<(f64, f64), ApiError> {
        match (self.lat, self.lng) {
            (Some(a), Some(b))
                if a.is_finite()
                    && b.is_finite()
                    && (-90.0..=90.0).contains(&a)
                    && (-180.0..=180.0).contains(&b) =>
            {
                Ok(crate::support::coord::wgs84_to_gcj02(a, b))
            }
            _ => Err(ApiError(
                StatusCode::BAD_REQUEST,
                "请传入有效的 WGS84 经纬度".into(),
            )),
        }
    }
}
fn required(value: &Option<String>) -> Result<&str, ApiError> {
    value
        .as_deref()
        .filter(|v| !v.trim().is_empty() && v.len() <= 256)
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "缺少有效的站点或线路参数".into()))
}
async fn upstream<T: serde::Serialize>(
    future: impl std::future::Future<Output = Result<T, provider::ProviderError>>,
) -> ApiResult {
    match tokio::time::timeout(Duration::from_secs(20), future).await {
        Ok(Ok(data)) => Ok(Json(json!({"data":data}))),
        Ok(Err(error)) => {
            eprintln!("upstream: {error}");
            Err(ApiError(
                StatusCode::BAD_GATEWAY,
                "公交数据源暂时不可用，请稍后重试".into(),
            ))
        }
        Err(_) => Err(ApiError(
            StatusCode::GATEWAY_TIMEOUT,
            "公交数据源响应超时，请重试".into(),
        )),
    }
}
async fn services() -> Json<Value> {
    Json(
        json!({"data": provider::available_services().iter().filter(|s| s.provider != "Debug").map(|s| json!({"id":s.id,"city":s.city.name(),"province":s.city.province(),"provider":s.provider})).collect::<Vec<_>>()}),
    )
}
async fn lines(Query(p): Query<Params>) -> ApiResult {
    let provider = p.provider()?;
    upstream(async {
        let mut rows = provider.all_lines().await?;
        if let Some(q) = p.q.as_ref().filter(|q| !q.trim().is_empty()) {
            let q = q.trim().to_lowercase();
            rows.retain(|r| {
                r.name.to_lowercase().contains(&q)
                    || r.endpoints.origin.contains(&q)
                    || r.endpoints.terminus.contains(&q)
            });
        }
        Ok(rows)
    })
    .await
}
async fn nearby(Query(p): Query<Params>) -> ApiResult {
    let provider = p.provider()?;
    let (lat, lng) = p.coords()?;
    upstream(provider.nearby_stations(lat, lng)).await
}
async fn station_lines(Query(p): Query<Params>) -> ApiResult {
    let provider = p.provider()?;
    let (lat, lng) = p.coords()?;
    upstream(provider.station_lines(required(&p.station)?, lat, lng)).await
}
async fn line(Query(p): Query<Params>) -> ApiResult {
    let provider = p.provider()?;
    upstream(provider.line_detail(required(&p.direction)?)).await
}
async fn realtime(Query(p): Query<Params>) -> ApiResult {
    let provider = p.provider()?;
    let order = p
        .order
        .filter(|n| *n > 0 && *n <= 10000)
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "order 必须是有效站序".into()))?;
    upstream(provider.realtime(required(&p.direction)?, order)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    fn params() -> Params {
        Params {
            service: "invalid".into(),
            q: None,
            lat: None,
            lng: None,
            station: None,
            direction: None,
            order: None,
        }
    }
    #[test]
    fn invalid_inputs() {
        let mut p = params();
        assert!(p.provider().is_err());
        assert!(p.coords().is_err());
        p.lat = Some(f64::NAN);
        p.lng = Some(120.0);
        assert!(p.coords().is_err());
        assert!(required(&Some(" ".into())).is_err());
    }
    #[tokio::test]
    async fn errors_are_not_empty_successes() {
        let error =
            upstream::<Value>(async { Err(provider::ProviderError::Network("test".into())) })
                .await
                .err()
                .unwrap();
        assert_eq!(error.0, StatusCode::BAD_GATEWAY);
    }
}
