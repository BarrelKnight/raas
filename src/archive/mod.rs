//! 归档模块
//!
//! 提供对不同归档格式(tar / zip)的统一随机访问抽象,使上层(缓存、HTTP 处理器)
//! 无需关心具体格式即可实现流式下载与 HTTP Range 请求。

pub mod scanner;
pub mod tar;
pub mod zip;

use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;

pub use tar::TarArchive;
pub use zip::ZipArchive;

/// 支持的归档格式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ArchiveFormat {
    /// POSIX tar(GNU 扩展)
    #[default]
    Tar,
    /// Zip(STORED 存储,不压缩)
    Zip,
}

impl ArchiveFormat {
    /// 从查询参数解析格式名,无法识别时返回 `None`
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "" | "tar" => Some(ArchiveFormat::Tar),
            "zip" => Some(ArchiveFormat::Zip),
            _ => None,
        }
    }

    /// 下载文件扩展名(不含点)
    pub fn extension(self) -> &'static str {
        match self {
            ArchiveFormat::Tar => "tar",
            ArchiveFormat::Zip => "zip",
        }
    }

    /// 对应的 HTTP `Content-Type`
    pub fn content_type(self) -> &'static str {
        match self {
            ArchiveFormat::Tar => "application/x-tar",
            ArchiveFormat::Zip => "application/zip",
        }
    }
}

/// 归档错误
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("IO 错误: {0}")]
    Io(#[from] io::Error),

    #[error("序列化错误: {0}")]
    Serialization(#[from] anyhow::Error),

    #[error("意料之外的错误: {0}")]
    UnexpectedError(String),
}

/// 随机访问归档的统一抽象
///
/// 实现者需在索引构建阶段完成偏移量计算,从而支持对归档流的任意位置访问,
/// 这是实现 HTTP Range、断点续传与多线程下载的基础。
pub trait Archive: Send + Sync {
    /// 归档格式
    fn format(&self) -> ArchiveFormat;

    /// 归档总大小(字节)
    fn total_size(&self) -> u64;

    /// 创建一个按字节范围读取的只读流,区间为半开区间 `[start, end)`
    fn stream_range(&self, start: u64, end: u64) -> Box<dyn Read + Send + '_>;
}

/// 按格式创建归档
pub fn create_archive(
    source: &Path,
    format: ArchiveFormat,
) -> Result<Arc<dyn Archive>, ArchiveError> {
    match format {
        ArchiveFormat::Tar => Ok(Arc::new(TarArchive::create(source)?)),
        ArchiveFormat::Zip => Ok(Arc::new(ZipArchive::create(source)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::ArchiveFormat;

    #[test]
    fn test_format_from_name() {
        assert_eq!(ArchiveFormat::from_name(""), Some(ArchiveFormat::Tar));
        assert_eq!(ArchiveFormat::from_name("tar"), Some(ArchiveFormat::Tar));
        assert_eq!(ArchiveFormat::from_name("TAR"), Some(ArchiveFormat::Tar));
        assert_eq!(ArchiveFormat::from_name(" zip "), Some(ArchiveFormat::Zip));
        assert_eq!(ArchiveFormat::from_name("rar"), None);
    }

    #[test]
    fn test_format_default_metadata() {
        assert_eq!(ArchiveFormat::default(), ArchiveFormat::Tar);
        assert_eq!(ArchiveFormat::Tar.extension(), "tar");
        assert_eq!(ArchiveFormat::Zip.extension(), "zip");
        assert_eq!(ArchiveFormat::Tar.content_type(), "application/x-tar");
        assert_eq!(ArchiveFormat::Zip.content_type(), "application/zip");
    }
}
