use std::sync::Arc;
use crate::config::AppConfig;
use crate::cache::ArchiveCache;

/// 应用状态
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub archive_cache: Arc<ArchiveCache>,
}

impl AppState {
    pub fn new(config: AppConfig) -> Self {
        let config_arc = Arc::new(config);
        let archive_cache = Arc::new(ArchiveCache::new(
            config_arc.server_performance.archive_cache_max_capacity
        ));
        
        Self {
            config: config_arc,
            archive_cache,
        }
    }
}
