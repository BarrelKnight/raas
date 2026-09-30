use axum::{
    Json, Router,
    extract::Request,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::json;
use std::time::Instant;
use tracing::info;

use crate::server::handler;
use crate::state::AppState;

/// 创建 archive 路由
pub fn archive_router() -> Router<AppState> {
    handler::archive_router()
}

/// 健康检查端点
async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// 请求日志中间件:记录方法、路径、状态码与耗时
async fn log_requests(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let started = Instant::now();

    let response = next.run(request).await;

    info!(
        method = %method,
        path = %uri.path(),
        status = response.status().as_u16(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "request"
    );

    response
}

/// 创建完整的应用路由
pub fn create_app_routes(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .nest("/api/archive", archive_router())
        .layer(middleware::from_fn(log_requests))
        .with_state(state)
}
