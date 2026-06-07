use std::path::PathBuf;

/// 默认配置常量
mod defaults {
    pub const BIND_ADDR: &str = "0.0.0.0:8080";
    pub const MAX_CONCURRENT_REQUESTS: usize = 100;
    pub const THREAD_POOL_SIZE: usize = 4;
    pub const BLOCKING_QUEUE_SIZE: usize = 1024;
    pub const SEND_BUFFER_SIZE: usize = 262144; // 256KB - 优化大文件流式传输性能
    pub const RECV_BUFFER_SIZE: usize = 16384; // 16KB - 仅接收GET请求头，无需太大
    pub const STREAM_READ_BUFFER_SIZE: usize = 16384; // 16KB - 平衡内存和性能，与文件系统块对齐
    pub const ARCHIVE_CACHE_MAX_CAPACITY: u64 = 100;
}

/// 服务器性能配置
#[derive(Clone, Debug)]
pub struct ServerPerformanceConfig {
    /// 最大并发请求数
    pub max_concurrent_requests: usize,
    /// 线程池大小
    pub thread_pool_size: usize,
    /// 阻塞队列大小
    pub blocking_queue_size: usize,
    /// 发送缓冲区大小（字节）
    pub send_buffer_size: usize,
    /// 接收缓冲区大小（字节）
    pub recv_buffer_size: usize,
    /// 流式读取缓冲区大小（字节）
    pub stream_read_buffer_size: usize,
    /// 存档缓存最大容量
    pub archive_cache_max_capacity: u64,
}

impl Default for ServerPerformanceConfig {
    fn default() -> Self {
        Self {
            max_concurrent_requests: defaults::MAX_CONCURRENT_REQUESTS,
            thread_pool_size: defaults::THREAD_POOL_SIZE,
            blocking_queue_size: defaults::BLOCKING_QUEUE_SIZE,
            send_buffer_size: defaults::SEND_BUFFER_SIZE,
            recv_buffer_size: defaults::RECV_BUFFER_SIZE,
            stream_read_buffer_size: defaults::STREAM_READ_BUFFER_SIZE,
            archive_cache_max_capacity: defaults::ARCHIVE_CACHE_MAX_CAPACITY,
        }
    }
}

impl ServerPerformanceConfig {
    pub fn from_env() -> Self {
        Self {
            max_concurrent_requests: std::env::var("MAX_CONCURRENT_REQUESTS")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(defaults::MAX_CONCURRENT_REQUESTS),
            thread_pool_size: std::env::var("THREAD_POOL_SIZE")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(defaults::THREAD_POOL_SIZE),
            blocking_queue_size: std::env::var("BLOCKING_QUEUE_SIZE")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(defaults::BLOCKING_QUEUE_SIZE),
            send_buffer_size: std::env::var("SEND_BUFFER_SIZE")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(defaults::SEND_BUFFER_SIZE),
            recv_buffer_size: std::env::var("RECV_BUFFER_SIZE")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(defaults::RECV_BUFFER_SIZE),
            stream_read_buffer_size: std::env::var("STREAM_READ_BUFFER_SIZE")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(defaults::STREAM_READ_BUFFER_SIZE),
            archive_cache_max_capacity: std::env::var("ARCHIVE_CACHE_MAX_CAPACITY")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(defaults::ARCHIVE_CACHE_MAX_CAPACITY),
        }
    }
}

/// 应用配置
#[derive(Clone, Debug)]
pub struct AppConfig {
    pub bind_addr: String,
    pub data_root: PathBuf,
    pub server_performance: ServerPerformanceConfig,
}

impl AppConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| defaults::BIND_ADDR.to_string());

        let data_root = std::env::var("DATA_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));

        let server_performance = ServerPerformanceConfig::from_env();

        Ok(Self {
            bind_addr,
            data_root,
            server_performance,
        })
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            bind_addr: defaults::BIND_ADDR.to_string(),
            data_root: PathBuf::from("."),
            server_performance: ServerPerformanceConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn test_server_performance_config_default() {
        let config = ServerPerformanceConfig::default();
        
        assert_eq!(config.max_concurrent_requests, defaults::MAX_CONCURRENT_REQUESTS);
        assert_eq!(config.thread_pool_size, defaults::THREAD_POOL_SIZE);
        assert_eq!(config.blocking_queue_size, defaults::BLOCKING_QUEUE_SIZE);
        assert_eq!(config.send_buffer_size, defaults::SEND_BUFFER_SIZE);
        assert_eq!(config.recv_buffer_size, defaults::RECV_BUFFER_SIZE);
        assert_eq!(config.stream_read_buffer_size, defaults::STREAM_READ_BUFFER_SIZE);
        assert_eq!(config.archive_cache_max_capacity, defaults::ARCHIVE_CACHE_MAX_CAPACITY);
    }

    #[test]
    #[serial]
    fn test_server_performance_config_from_env() {
        // 首先清理所有相关环境变量
        unsafe {
            std::env::remove_var("MAX_CONCURRENT_REQUESTS");
            std::env::remove_var("THREAD_POOL_SIZE");
            std::env::remove_var("BLOCKING_QUEUE_SIZE");
            std::env::remove_var("SEND_BUFFER_SIZE");
            std::env::remove_var("RECV_BUFFER_SIZE");
            std::env::remove_var("STREAM_READ_BUFFER_SIZE");
            std::env::remove_var("ARCHIVE_CACHE_MAX_CAPACITY");
        }
        
        // 设置自定义环境变量
        unsafe {
            std::env::set_var("MAX_CONCURRENT_REQUESTS", "200");
            std::env::set_var("THREAD_POOL_SIZE", "8");
            std::env::set_var("BLOCKING_QUEUE_SIZE", "2048");
            std::env::set_var("SEND_BUFFER_SIZE", "524288");  // 512KB
            std::env::set_var("RECV_BUFFER_SIZE", "32768");  // 32KB
            std::env::set_var("STREAM_READ_BUFFER_SIZE", "32768");  // 32KB
            std::env::set_var("ARCHIVE_CACHE_MAX_CAPACITY", "50");
        }

        let config = ServerPerformanceConfig::from_env();

        assert_eq!(config.max_concurrent_requests, 200);
        assert_eq!(config.thread_pool_size, 8);
        assert_eq!(config.blocking_queue_size, 2048);
        assert_eq!(config.send_buffer_size, 524288);
        assert_eq!(config.recv_buffer_size, 32768);
        assert_eq!(config.stream_read_buffer_size, 32768);
        assert_eq!(config.archive_cache_max_capacity, 50);

        // 清理环境变量
        unsafe {
            std::env::remove_var("MAX_CONCURRENT_REQUESTS");
            std::env::remove_var("THREAD_POOL_SIZE");
            std::env::remove_var("BLOCKING_QUEUE_SIZE");
            std::env::remove_var("SEND_BUFFER_SIZE");
            std::env::remove_var("RECV_BUFFER_SIZE");
            std::env::remove_var("STREAM_READ_BUFFER_SIZE");
            std::env::remove_var("ARCHIVE_CACHE_MAX_CAPACITY");
        }
    }

    #[test]
    #[serial]
    fn test_server_performance_config_invalid_env() {
        // 首先清理所有相关环境变量
        unsafe {
            std::env::remove_var("MAX_CONCURRENT_REQUESTS");
            std::env::remove_var("THREAD_POOL_SIZE");
        }
        
        // 设置无效的环境变量（非数字）
        unsafe {
            std::env::set_var("MAX_CONCURRENT_REQUESTS", "invalid");
            std::env::set_var("THREAD_POOL_SIZE", "abc");
        }

        let config = ServerPerformanceConfig::from_env();

        // 应该使用默认值
        assert_eq!(config.max_concurrent_requests, defaults::MAX_CONCURRENT_REQUESTS);
        assert_eq!(config.thread_pool_size, defaults::THREAD_POOL_SIZE);

        // 清理环境变量
        unsafe {
            std::env::remove_var("MAX_CONCURRENT_REQUESTS");
            std::env::remove_var("THREAD_POOL_SIZE");
        }
    }

    #[test]
    fn test_app_config_default() {
        let config = AppConfig::default();
        
        assert_eq!(config.bind_addr, defaults::BIND_ADDR);
        assert_eq!(config.data_root, PathBuf::from("."));
        assert_eq!(config.server_performance.max_concurrent_requests, defaults::MAX_CONCURRENT_REQUESTS);
    }

    #[test]
    #[serial]
    fn test_app_config_from_env_with_defaults() {
        // 确保清除可能影响测试的环境变量
        unsafe {
            std::env::remove_var("BIND_ADDR");
            std::env::remove_var("DATA_ROOT");
            std::env::remove_var("MAX_CONCURRENT_REQUESTS");
            std::env::remove_var("THREAD_POOL_SIZE");
            std::env::remove_var("BLOCKING_QUEUE_SIZE");
            std::env::remove_var("SEND_BUFFER_SIZE");
            std::env::remove_var("RECV_BUFFER_SIZE");
            std::env::remove_var("STREAM_READ_BUFFER_SIZE");
            std::env::remove_var("ARCHIVE_CACHE_MAX_CAPACITY");
        }
        
        // 不设置任何环境变量，使用默认值
        let config = AppConfig::from_env().unwrap();
        
        assert_eq!(config.bind_addr, defaults::BIND_ADDR);
        assert_eq!(config.data_root, PathBuf::from("."));
    }

    #[test]
    #[serial]
    fn test_app_config_from_env_with_custom_values() {
        // 首先清理所有相关环境变量
        unsafe {
            std::env::remove_var("BIND_ADDR");
            std::env::remove_var("DATA_ROOT");
            std::env::remove_var("MAX_CONCURRENT_REQUESTS");
            std::env::remove_var("THREAD_POOL_SIZE");
            std::env::remove_var("BLOCKING_QUEUE_SIZE");
            std::env::remove_var("SEND_BUFFER_SIZE");
            std::env::remove_var("RECV_BUFFER_SIZE");
            std::env::remove_var("STREAM_READ_BUFFER_SIZE");
            std::env::remove_var("ARCHIVE_CACHE_MAX_CAPACITY");
        }
        
        // 设置自定义环境变量
        unsafe {
            std::env::set_var("BIND_ADDR", "0.0.0.0:3000");
            std::env::set_var("DATA_ROOT", "/tmp/test_data");
            std::env::set_var("MAX_CONCURRENT_REQUESTS", "50");
        }

        let config = AppConfig::from_env().unwrap();

        assert_eq!(config.bind_addr, "0.0.0.0:3000");
        assert_eq!(config.data_root, PathBuf::from("/tmp/test_data"));
        assert_eq!(config.server_performance.max_concurrent_requests, 50);

        // 清理环境变量
        unsafe {
            std::env::remove_var("BIND_ADDR");
            std::env::remove_var("DATA_ROOT");
            std::env::remove_var("MAX_CONCURRENT_REQUESTS");
        }
    }
}
