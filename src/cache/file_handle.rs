use std::cell::RefCell;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// 文件句柄缓存
/// 
/// 使用 RefCell 实现内部可变性,只缓存一个文件句柄。
/// 适用于流式顺序读取场景,请求结束后自动 drop。
pub struct FileHandleCache {
    current_file: RefCell<Option<(PathBuf, File)>>,
}

impl FileHandleCache {
    /// 创建新的文件句柄缓存
    pub fn new() -> Self {
        Self {
            current_file: RefCell::new(None),
        }
    }

    /// 读取文件内容到 buffer
    /// 
    /// 如果请求的是同一个文件,使用已缓存的句柄
    /// 如果是不同的文件,关闭旧句柄并打开新文件
    pub fn read_at<P: AsRef<Path>>(&self, path: P, position: u64, buf: &mut [u8]) -> Result<usize, io::Error> {
        let path = path.as_ref();
        
        // 检查是否命中缓存
        let need_open = {
            let cache = self.current_file.borrow();
            match cache.as_ref() {
                Some((cached_path, _)) => cached_path != path,
                None => true,
            }
        };
        
        // 未命中缓存,打开新文件
        if need_open {
            let file = File::open(path)?;
            *self.current_file.borrow_mut() = Some((path.to_path_buf(), file));
        }
        
        // 使用缓存的文件句柄进行读取
        let mut cache = self.current_file.borrow_mut();
        let file = &mut cache.as_mut().unwrap().1;
        file.seek(SeekFrom::Start(position))?;
        file.read(buf)
    }
}

impl Default for FileHandleCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::FileHandleCache;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_file_handle_cache_read() {
        let cache = FileHandleCache::new();
        
        // 创建临时目录和文件
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let test_file_path = temp_dir.path().join("test_file.txt");
        {
            let mut file = File::create(&test_file_path).unwrap();
            writeln!(file, "测试内容").unwrap();
        }
        
        // 读取文件内容
        let mut buffer = vec![0u8; 100];
        let bytes_read = cache.read_at(&test_file_path, 0, &mut buffer).unwrap();
        assert!(bytes_read > 0);
        
        let content = String::from_utf8_lossy(&buffer[..bytes_read]);
        assert!(content.contains("测试内容"));
    }

    #[tokio::test]
    async fn test_file_handle_cache_same_file() {
        let cache = FileHandleCache::new();

        // 创建临时文件
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let test_file_path = temp_dir.path().join("test.txt");
        {
            let mut file = File::create(&test_file_path).unwrap();
            writeln!(file, "内容").unwrap();
        }
        
        // 多次读取同一个文件,应该复用句柄
        let mut buffer1 = vec![0u8; 100];
        let mut buffer2 = vec![0u8; 100];
        
        let bytes1 = cache.read_at(&test_file_path, 0, &mut buffer1).unwrap();
        let bytes2 = cache.read_at(&test_file_path, 0, &mut buffer2).unwrap();
        
        assert_eq!(bytes1, bytes2);
        assert_eq!(&buffer1[..bytes1], &buffer2[..bytes2]);
    }

    #[tokio::test]
    async fn test_file_handle_cache_different_files() {
        let cache = FileHandleCache::new();

        // 创建多个临时文件
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let file1 = temp_dir.path().join("file1.txt");
        let file2 = temp_dir.path().join("file2.txt");
        
        {
            let mut f1 = File::create(&file1).unwrap();
            writeln!(f1, "文件1").unwrap();
            
            let mut f2 = File::create(&file2).unwrap();
            writeln!(f2, "文件2").unwrap();
        }
        
        // 读取不同文件
        let mut buffer1 = vec![0u8; 100];
        let mut buffer2 = vec![0u8; 100];
        
        let bytes1 = cache.read_at(&file1, 0, &mut buffer1).unwrap();
        let bytes2 = cache.read_at(&file2, 0, &mut buffer2).unwrap();
        
        assert!(bytes1 > 0);
        assert!(bytes2 > 0);
        
        let content1 = String::from_utf8_lossy(&buffer1[..bytes1]);
        let content2 = String::from_utf8_lossy(&buffer2[..bytes2]);
        
        assert!(content1.contains("文件1"));
        assert!(content2.contains("文件2"));
    }

    #[tokio::test]
    async fn test_file_handle_cache_nonexistent_file() {
        let cache = FileHandleCache::new();
        let nonexistent_path = "/nonexistent/path/file.txt";
        
        let mut buffer = vec![0u8; 100];
        let result = cache.read_at(nonexistent_path, 0, &mut buffer);
        
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_file_handle_cache_read_at_position() {
        let cache = FileHandleCache::new();
        
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let test_file_path = temp_dir.path().join("test.txt");
        {
            let mut file = File::create(&test_file_path).unwrap();
            write!(file, "Hello, World!").unwrap();
        }
        
        // 从位置 7 开始读取
        let mut buffer = vec![0u8; 5];
        let bytes_read = cache.read_at(&test_file_path, 7, &mut buffer).unwrap();
        
        assert_eq!(bytes_read, 5);
        let content = String::from_utf8_lossy(&buffer);
        assert_eq!(content, "World");
    }

    #[tokio::test]
    async fn test_file_handle_cache_read_beyond_eof() {
        let cache = FileHandleCache::new();
        
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let test_file_path = temp_dir.path().join("test.txt");
        {
            let mut file = File::create(&test_file_path).unwrap();
            write!(file, "Short").unwrap();
        }
        
        // 尝试从超出文件末尾的位置读取
        let mut buffer = vec![0u8; 100];
        let bytes_read = cache.read_at(&test_file_path, 1000, &mut buffer).unwrap();
        
        // 应该读取 0 字节
        assert_eq!(bytes_read, 0);
    }
}
