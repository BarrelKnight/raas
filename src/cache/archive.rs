use moka::sync::Cache;
use std::path::PathBuf;
use std::sync::Arc;
use crate::archive::random_access::RandomAccessArchive;

/// 存档缓存管理器
pub struct ArchiveCache {
    cache: Cache<PathBuf, Result<Arc<RandomAccessArchive>, String>>,
}

impl ArchiveCache {
    /// 创建新的存档缓存
    pub fn new(max_capacity: u64) -> Self {
        let cache = Cache::builder()
            .max_capacity(max_capacity)
            .build();

        Self { cache }
    }

    /// 获取存档，如果不存在则创建并缓存
    pub fn get_or_create(
        &self,
        path: &PathBuf,
    ) -> Result<Arc<RandomAccessArchive>, anyhow::Error> {
        // 使用 get_with 实现原子性加载，避免并发时的重复创建
        let result = self.cache.get_with(path.clone(), || {
            RandomAccessArchive::create(path)
                .map(Arc::new)
                .map_err(|e| e.to_string())
        });

        // 解包 Result
        result.map_err(|e| anyhow::anyhow!("创建随机访问存档失败: {}", e))
    }

    #[cfg(test)]
    /// 获取存档（仅从缓存）
    pub fn get(&self, path: &PathBuf) -> Option<Arc<RandomAccessArchive>> {
        self.cache.get(path).and_then(|result| result.ok())
    }

    #[cfg(test)]
    /// 插入存档到缓存
    pub fn insert(&self, path: PathBuf, archive: Arc<RandomAccessArchive>) {
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

impl Default for ArchiveCache {
    fn default() -> Self {
        Self::new(crate::config::ServerPerformanceConfig::default().archive_cache_max_capacity)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use std::fs::File;
    use std::io::Write;

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
}
