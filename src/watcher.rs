//! 文件系统监听模块
//!
//! 使用 `notify` 递归监听数据根目录,在源目录内容发生变更时自动失效对应的存档缓存,
//! 确保缓存数据与源目录保持一致(见 README 开发计划 1)。
//!
//! 设计要点:
//! - 监听线程只负责收集与合并事件,真正的失效逻辑委托给 [`ArchiveCache`];
//! - 通过抖动窗口(debounce)合并连续事件,避免批量写入时反复触发失效;
//! - 监听器被释放时会停止监听并回收线程。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{debug, info, warn};

use crate::cache::ArchiveCache;

/// 停止信号的轮询间隔,决定监听线程退出的最大延迟
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 文件系统监听器
///
/// 监听 `root` 下的所有变更事件,并据此失效存档缓存中受影响的条目。
/// `Drop` 时会停止底层监听并回收事件处理线程。
pub struct FileSystemWatcher {
    /// 底层 watcher,仅用于保持监听存活
    watcher: Option<RecommendedWatcher>,
    /// 停止信号发送端
    stop_tx: Option<Sender<()>>,
    /// 事件处理线程
    worker: Option<JoinHandle<()>>,
}

impl FileSystemWatcher {
    /// 启动文件系统监听
    ///
    /// `root` 为要监听的数据根目录,`cache` 为需要失效的存档缓存,
    /// `debounce` 为事件抖动合并窗口。
    pub fn start(
        root: &Path,
        cache: Arc<ArchiveCache>,
        debounce: Duration,
    ) -> anyhow::Result<Self> {
        // 规范化根目录,避免相对路径或符号链接带来的差异
        let watch_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

        let (event_tx, event_rx) = mpsc::channel::<notify::Result<Event>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();

        let mut watcher = notify::recommended_watcher(move |res| {
            // 接收端已关闭时忽略发送失败
            let _ = event_tx.send(res);
        })?;
        watcher.watch(&watch_root, RecursiveMode::Recursive)?;

        let worker_root = watch_root.clone();
        let worker = thread::Builder::new()
            .name("raas-fs-watcher".to_string())
            .spawn(move || {
                run_worker(event_rx, stop_rx, cache, worker_root, debounce);
            })?;

        info!("已启动文件系统监听: {:?}", watch_root);

        Ok(Self {
            watcher: Some(watcher),
            stop_tx: Some(stop_tx),
            worker: Some(worker),
        })
    }
}

impl Drop for FileSystemWatcher {
    fn drop(&mut self) {
        // 通知线程退出
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        // 停止底层监听(回调随之释放,事件通道随之断开)
        self.watcher.take();
        // 等待线程回收,确保不会在缓存被销毁后继续访问
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// 监听线程主循环
fn run_worker(
    event_rx: Receiver<notify::Result<Event>>,
    stop_rx: Receiver<()>,
    cache: Arc<ArchiveCache>,
    root: PathBuf,
    debounce: Duration,
) {
    let mut pending: HashSet<PathBuf> = HashSet::new();

    loop {
        // 带超时地等待事件,同时周期性检查停止信号
        match event_rx.recv_timeout(POLL_INTERVAL) {
            Ok(event) => {
                collect_event(&mut pending, event);

                // 在抖动窗口内继续合并后续事件,避免同一批变更被反复处理
                loop {
                    match event_rx.recv_timeout(debounce) {
                        Ok(event) => collect_event(&mut pending, event),
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => {
                            flush(&cache, &mut pending);
                            return;
                        }
                    }
                }

                flush(&cache, &mut pending);
            }
            Err(RecvTimeoutError::Timeout) => match stop_rx.try_recv() {
                Ok(()) | Err(TryRecvError::Disconnected) => break,
                Err(TryRecvError::Empty) => {}
            },
            Err(RecvTimeoutError::Disconnected) => {
                flush(&cache, &mut pending);
                return;
            }
        }
    }

    // 退出前处理剩余事件
    flush(&cache, &mut pending);
    debug!("文件系统监听线程已退出: {:?}", root);
}

/// 从单个事件中提取受影响的路径并加入待处理集合
fn collect_event(pending: &mut HashSet<PathBuf>, event: notify::Result<Event>) {
    let event = match event {
        Ok(event) => event,
        Err(err) => {
            warn!("文件系统监听事件出错: {}", err);
            return;
        }
    };

    // 仅关心内容/元数据变更,忽略纯访问事件,减少无谓的失效
    if matches!(event.kind, EventKind::Access(_)) {
        return;
    }

    for path in event.paths {
        pending.insert(path);
    }
}

/// 将待处理路径批量失效
fn flush(cache: &ArchiveCache, pending: &mut HashSet<PathBuf>) {
    if pending.is_empty() {
        return;
    }

    let mut total_invalidated = 0usize;
    for path in pending.drain() {
        let invalidated = cache.invalidate(&path);
        if !invalidated.is_empty() {
            debug!("源路径变更 {:?},失效缓存: {:?}", path, invalidated);
            total_invalidated += invalidated.len();
        }
    }

    if total_invalidated > 0 {
        info!("文件系统变更导致 {} 个存档缓存条目失效", total_invalidated);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// 轮询等待条件成立,超时返回 false
    fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        cond()
    }

    #[test]
    fn test_watcher_invalidates_cache_on_file_change() {
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let root = temp_dir.path().to_path_buf();
        let file_path = root.join("data.txt");
        fs::write(&file_path, "initial").unwrap();

        let cache = Arc::new(ArchiveCache::new(10));
        cache.get_or_create(&file_path).unwrap();
        assert!(cache.is_cached(&file_path));

        let watcher = FileSystemWatcher::start(&root, cache.clone(), Duration::from_millis(50))
            .expect("启动监听失败");

        // 修改文件应触发缓存失效
        fs::write(&file_path, "changed").unwrap();

        let invalidated = wait_until(Duration::from_secs(5), || !cache.is_cached(&file_path));
        assert!(invalidated, "文件变更后缓存应被自动失效");

        // 重新缓存后再删除文件,同样应触发失效
        cache.get_or_create(&file_path).unwrap();
        assert!(cache.is_cached(&file_path));
        fs::remove_file(&file_path).unwrap();

        let invalidated = wait_until(Duration::from_secs(5), || !cache.is_cached(&file_path));
        assert!(invalidated, "文件删除后缓存应被自动失效");

        drop(watcher);
    }

    #[test]
    fn test_watcher_start_on_missing_root_is_recoverable() {
        let cache = Arc::new(ArchiveCache::new(10));
        let missing = PathBuf::from("this-directory-should-not-exist-xyz");

        // 监听不存在的路径应返回错误,调用方据此降级为不启用监听
        let result = FileSystemWatcher::start(&missing, cache, Duration::from_millis(50));
        assert!(result.is_err());
    }

    #[test]
    fn test_normalize_ignores_access_events() {
        // Access 事件不应进入待处理集合
        let mut pending = HashSet::new();
        let event = Event {
            kind: EventKind::Access(notify::event::AccessKind::Any),
            paths: vec![PathBuf::from("/tmp/x")],
            attrs: Default::default(),
        };
        collect_event(&mut pending, Ok(event));
        assert!(pending.is_empty());
    }
}
