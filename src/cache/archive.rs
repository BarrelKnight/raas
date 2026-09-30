use crate::archive::random_access::RandomAccessArchive;
use moka::sync::Cache;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

/// 存档缓存管理器
///
/// 除了基于 `moka` 的容量淘汰外,还会记录所有已缓存的键,以便在源目录发生变更时
/// 精确失效受影响的存档(见 README 开发计划 1)。
pub struct ArchiveCache {
    cache: Cache<PathBuf, Result<Arc<RandomAccessArchive>, String>>,
    /// 已缓存键的注册表,用于按路径前缀定位需要失效的条目
    cached_keys: RwLock<HashSet<PathBuf>>,
    /// 缓存最大容量,用于注册表裁剪阈值
    max_capacity: u64,
}

impl ArchiveCache {
    /// 创建新的存档缓存
    pub fn new(max_capacity: u64) -> Self {
        let cache = Cache::builder().max_capacity(max_capacity).build();

        Self {
            cache,
            cached_keys: RwLock::new(HashSet::new()),
            max_capacity,
        }
    }

    /// 获取存档，如果不存在则创建并缓存
    pub fn get_or_create(&self, path: &PathBuf) -> Result<Arc<RandomAccessArchive>, anyhow::Error> {
        // 使用 get_with 实现原子性加载，避免并发时的重复创建
        let result = self.cache.get_with(path.clone(), || {
            RandomAccessArchive::create(path)
                .map(Arc::new)
                .map_err(|e| e.to_string())
        });

        // 无论成功与否都记录缓存键:失败结果同样被 moka 缓存,
        // 待源目录修复后需要靠文件系统事件将其失效以便重试
        self.register_key(path);

        // 解包 Result
        result.map_err(|e| anyhow::anyhow!("创建随机访问存档失败: {}", e))
    }

    /// 失效所有受 `changed_path` 变更影响的缓存条目
    ///
    /// 只要 `changed_path` 自身或其任一祖先路径被缓存为存档,该存档就必须失效,
    /// 因为其目录结构、文件大小和偏移量索引已与源目录不一致。
    ///
    /// 返回被失效的缓存键列表。
    pub fn invalidate(&self, changed_path: &Path) -> Vec<PathBuf> {
        let changed = normalize_for_match(changed_path);

        let victims: Vec<PathBuf> = {
            let keys = self.cached_keys.read().unwrap();
            keys.iter()
                .filter(|key| changed.starts_with(normalize_for_match(key)))
                .cloned()
                .collect()
        };

        if victims.is_empty() {
            return victims;
        }

        for key in &victims {
            self.cache.invalidate(key);
        }

        let mut keys = self.cached_keys.write().unwrap();
        for key in &victims {
            keys.remove(key);
        }

        victims
    }

    /// 清空所有缓存条目
    #[cfg(test)]
    pub fn invalidate_all(&self) {
        self.cache.invalidate_all();
        self.cached_keys.write().unwrap().clear();
    }

    /// 判断指定路径对应的存档当前是否仍在缓存中
    ///
    /// 主要供集成测试与外部调用者观察缓存状态；`raas` 二进制会在 lib/bin
    /// 双份编译下将该方法视为未使用，因此显式允许 dead_code。
    #[allow(dead_code)]
    pub fn is_cached(&self, path: &Path) -> bool {
        self.cache.contains_key(path)
    }

    /// 返回当前已缓存键的快照
    #[cfg(test)]
    pub fn cached_paths(&self) -> Vec<PathBuf> {
        self.cached_keys.read().unwrap().iter().cloned().collect()
    }

    /// 记录缓存键,并在注册表过度膨胀时按实际缓存内容裁剪
    fn register_key(&self, key: &Path) {
        let mut keys = self.cached_keys.write().unwrap();
        let prune_threshold = self.max_capacity.saturating_mul(4).max(8);
        if keys.len() as u64 > prune_threshold {
            keys.retain(|k| self.cache.contains_key(k));
        }
        keys.insert(key.to_path_buf());
    }

    #[cfg(test)]
    /// 获取存档（仅从缓存）
    pub fn get(&self, path: &PathBuf) -> Option<Arc<RandomAccessArchive>> {
        self.cache.get(path).and_then(|result| result.ok())
    }

    #[cfg(test)]
    /// 插入存档到缓存
    pub fn insert(&self, path: PathBuf, archive: Arc<RandomAccessArchive>) {
        self.register_key(&path);
        self.cache.insert(path, Ok(archive));
    }

    #[cfg(test)]
    /// 获取缓存统计信息
    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.cache.entry_count(),
            self.cache.policy().max_capacity().unwrap_or(0),
            self.cache.weighted_size(),
        )
    }
}

/// 去除 Windows 扩展长度路径前缀（`\\?\`），使不同来源的路径可以一致比较
fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            // UNC 路径: \\?\UNC\server\share -> \\server\share
            if let Some(unc) = rest.strip_prefix("UNC\\") {
                return PathBuf::from(format!(r"\\{}", unc));
            }
            return PathBuf::from(rest);
        }
        path.to_path_buf()
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

/// 规范化路径以便进行前缀匹配:
/// 去除 Windows 前缀、消解 `.`/`..`,并在 Windows 上忽略大小写。
fn normalize_for_match(path: &Path) -> PathBuf {
    let stripped = strip_verbatim_prefix(path);
    let mut normalized = PathBuf::new();
    for component in stripped.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }

    #[cfg(windows)]
    let normalized = PathBuf::from(normalized.to_string_lossy().to_lowercase());

    normalized
}

impl Default for ArchiveCache {
    fn default() -> Self {
        Self::new(crate::config::ServerPerformanceConfig::default().archive_cache_max_capacity)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;

    use super::ArchiveCache;
    use crate::archive::random_access::RandomAccessArchive;

    #[test]
    fn test_archive_cache_basic_operations() {
        let cache = ArchiveCache::new(5); // 最大容量为5

        // 创建临时目录和文件
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let source_dir = temp_dir.path();

        // 创建测试文件
        let test_file_path = source_dir.join("test_file.txt");
        let mut file = File::create(&test_file_path).unwrap();
        writeln!(file, "测试内容").unwrap();

        let path = source_dir.to_path_buf();

        // 第一次获取 - 应该创建新的存档
        let archive1_result = cache.get_or_create(&path);
        assert!(archive1_result.is_ok());
        let archive1 = archive1_result.unwrap();

        // 第二次获取 - 应该从缓存获取相同的存档
        let archive2_result = cache.get_or_create(&path);
        assert!(archive2_result.is_ok());
        let archive2 = archive2_result.unwrap();

        // 验证两次获取的是同一个存档（通过地址比较）
        assert_eq!(Arc::as_ptr(&archive1), Arc::as_ptr(&archive2));

        // 验证存档基本信息
        assert_eq!(archive1.total_size(), archive2.total_size());
        assert!(!archive1.list_files().is_empty());
    }

    #[test]
    fn test_archive_cache_capacity_limit() {
        let cache = ArchiveCache::new(2); // 最大容量为2

        // 创建多个临时目录
        let temp_dirs: Vec<TempDir> = (0..5)
            .map(|i| {
                let temp_dir = TempDir::new().expect("创建临时目录失败");
                let file_path = temp_dir.path().join(format!("file_{}.txt", i));
                std::fs::write(&file_path, format!("内容 {}", i)).unwrap();
                temp_dir
            })
            .collect();

        // 添加超过容量限制的存档
        let paths: Vec<PathBuf> = temp_dirs.iter().map(|d| d.path().to_path_buf()).collect();

        for path in &paths {
            let result = cache.get_or_create(path);
            assert!(result.is_ok());
        }

        // 验证当前缓存大小不超过限制
        let (entry_count, max_capacity, _) = cache.stats();
        assert!(entry_count <= max_capacity);
        assert_eq!(max_capacity, 2);
    }

    #[test]
    fn test_archive_cache_get_method() {
        let cache = ArchiveCache::new(5);

        // 创建临时目录和文件
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let source_dir = temp_dir.path();
        let test_file_path = source_dir.join("test.txt");
        std::fs::write(&test_file_path, "测试内容").unwrap();

        let path = source_dir.to_path_buf();

        // 先插入一个存档
        let archive = RandomAccessArchive::create(&path).unwrap();
        let archive_arc = Arc::new(archive);
        cache.insert(path.clone(), archive_arc.clone());

        // 尝试获取
        let retrieved = cache.get(&path);
        assert!(retrieved.is_some());

        // 验证获取到的存档是正确的
        let retrieved_archive = retrieved.unwrap();
        assert_eq!(Arc::as_ptr(&archive_arc), Arc::as_ptr(&retrieved_archive));
    }

    #[test]
    fn test_archive_cache_invalidate_ancestors() {
        let cache = ArchiveCache::new(10);

        // 构造 root/sub/f.txt 与 root/g.txt 的目录结构
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let root = temp_dir.path().to_path_buf();
        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("f.txt"), "x").unwrap();
        std::fs::write(root.join("g.txt"), "y").unwrap();

        // 缓存 root 与 sub 两个存档
        cache.get_or_create(&root).unwrap();
        cache.get_or_create(&sub).unwrap();

        // 修改 sub 下的文件:root 与 sub 都应失效
        let invalidated = cache.invalidate(&sub.join("f.txt"));
        assert_eq!(invalidated.len(), 2, "祖先路径的缓存都应被失效");
        assert!(!cache.is_cached(&root));
        assert!(!cache.is_cached(&sub));
        assert!(cache.cached_paths().is_empty());

        // 重新缓存:修改 root 下的其他文件只应失效 root,不影响 sub
        cache.get_or_create(&root).unwrap();
        cache.get_or_create(&sub).unwrap();
        let invalidated = cache.invalidate(&root.join("g.txt"));
        assert_eq!(invalidated.len(), 1);
        assert!(!cache.is_cached(&root));
        assert!(cache.is_cached(&sub));

        // 无关路径不应触发任何失效
        assert!(cache.invalidate(&temp_dir.path().join("other")).is_empty());
        assert!(cache.is_cached(&sub));
    }

    #[test]
    fn test_archive_cache_invalidate_self() {
        let cache = ArchiveCache::new(10);

        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let file_path = temp_dir.path().join("single.txt");
        std::fs::write(&file_path, "content").unwrap();

        // 缓存单个文件为存档
        cache.get_or_create(&file_path).unwrap();
        assert!(cache.is_cached(&file_path));

        // 该文件自身发生变更应使其失效
        let invalidated = cache.invalidate(&file_path);
        assert_eq!(invalidated.len(), 1);
        assert!(!cache.is_cached(&file_path));
    }

    #[test]
    fn test_archive_cache_invalidate_all() {
        let cache = ArchiveCache::new(10);

        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let a = temp_dir.path().join("a");
        let b = temp_dir.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        std::fs::write(a.join("1.txt"), "1").unwrap();
        std::fs::write(b.join("2.txt"), "2").unwrap();

        cache.get_or_create(&a).unwrap();
        cache.get_or_create(&b).unwrap();
        assert_eq!(cache.cached_paths().len(), 2);

        cache.invalidate_all();
        assert!(cache.cached_paths().is_empty());
        assert!(!cache.is_cached(&a));
        assert!(!cache.is_cached(&b));
    }
}
