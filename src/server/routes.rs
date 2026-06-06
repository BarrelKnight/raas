use axum::Router;
use crate::state::AppState;
use crate::server::handler;

/// 创建 archive 路由
pub fn archive_router() -> Router<AppState> {
    handler::archive_router()
}

/// 创建完整的应用路由
pub fn create_app_routes(state: AppState) -> Router {
    Router::new()
        .nest("/api/archive", archive_router())
        .with_state(state.clone())
}
