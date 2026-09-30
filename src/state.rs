use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

use crate::cache::ArchiveCache;
use crate::config::AppConfig;
use crate::watcher::FileSystemWatcher;

/// 应用状态
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    /// 已规范化的数据根目录(启动时 canonicalize 一次,避免每请求重复解析)
    pub data_root: PathBuf,
    pub archive_cache: Arc<ArchiveCache>,
    /// 文件系统监听器（保持存活即可，Drop 时会自动停止监听）
    pub file_watcher: Option<Arc<FileSystemWatcher>>,
}

impl AppState {
    pub fn new(config: AppConfig) -> Self {
        let config_arc = Arc::new(config);
        let archive_cache = Arc::new(ArchiveCache::new(
            config_arc.server_performance.archive_cache_max_capacity,
        ));

        // 提前规范化根目录:路径校验依赖它与目标路径同为规范化形式
        let data_root = config_arc
            .data_root
            .canonicalize()
            .unwrap_or_else(|_| config_arc.data_root.clone());

        let file_watcher = Self::start_file_watcher(&config_arc, &archive_cache);

        Self {
            config: config_arc,
            data_root,
            archive_cache,
            file_watcher,
        }
    }

    /// 启动文件系统监听,失败时降级为不启用(缓存不再自动失效)
    fn start_file_watcher(
        config: &AppConfig,
        archive_cache: &Arc<ArchiveCache>,
    ) -> Option<Arc<FileSystemWatcher>> {
        if !config.server_performance.enable_file_watcher {
            return None;
        }

        let debounce = Duration::from_millis(config.server_performance.file_watcher_debounce_ms);

        match FileSystemWatcher::start(&config.data_root, archive_cache.clone(), debounce) {
            Ok(watcher) => Some(Arc::new(watcher)),
            Err(err) => {
                warn!(
                    "启动文件系统监听失败，缓存将不会随源目录变更自动失效: {}",
                    err
                );
                None
            }
        }
    }
}
