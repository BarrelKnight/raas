//! 源目录扫描
//!
//! tar 与 zip 归档共享同一套目录扫描逻辑,输出稳定的相对路径与元数据。
//! 扫描阶段只读取文件元数据(大小、类型),不读取文件内容,因此扫描成本与
//! 目录条目数相关,而与文件体积无关。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::ArchiveError;

/// 扫描得到的一个条目
#[derive(Debug, Clone)]
pub struct ScannedEntry {
    /// 归档内相对路径(使用 `/` 分隔,目录以 `/` 结尾)
    pub relative_path: String,
    /// 读取内容时使用的源路径
    pub source_path: PathBuf,
    /// 文件大小(目录为 0)
    pub size: u64,
    /// 是否为目录
    pub is_dir: bool,
    /// 权限位(非 Unix 平台使用 0644/0755 作为合理默认值)
    pub mode: u32,
    /// 修改时间(Unix 秒,无法获取时为 0)
    pub mtime: u64,
}

/// 提取权限位
#[cfg(unix)]
fn entry_mode(metadata: &fs::Metadata, _is_dir: bool) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn entry_mode(_metadata: &fs::Metadata, is_dir: bool) -> u32 {
    if is_dir { 0o755 } else { 0o644 }
}

/// 提取修改时间(Unix 秒)
fn entry_mtime(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 扫描源路径
///
/// 若 `source` 为单个文件,则产出以文件名命名的单条记录;否则递归扫描目录。
pub fn scan_source(source: &Path) -> Result<Vec<ScannedEntry>, ArchiveError> {
    let mut entries = Vec::new();

    if source.is_file() {
        let metadata = fs::metadata(source)?;
        let name = source
            .file_name()
            .ok_or_else(|| {
                ArchiveError::Io(io::Error::new(io::ErrorKind::InvalidData, "无法获取文件名"))
            })?
            .to_string_lossy()
            .to_string();

        entries.push(ScannedEntry {
            relative_path: name,
            source_path: source.to_path_buf(),
            size: metadata.len(),
            is_dir: false,
            mode: entry_mode(&metadata, false),
            mtime: entry_mtime(&metadata),
        });
    } else {
        scan_dir(source, source, &mut entries)?;
    }

    Ok(entries)
}

/// 递归扫描目录
fn scan_dir(
    base_path: &Path,
    current_path: &Path,
    entries: &mut Vec<ScannedEntry>,
) -> Result<(), ArchiveError> {
    for entry in fs::read_dir(current_path)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;

        // 计算相对于基础路径的相对路径,并统一为 tar/zip 规范使用的正斜杠
        let rel_path = path.strip_prefix(base_path).map_err(|_| {
            ArchiveError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "无法计算相对路径",
            ))
        })?;
        let rel_str = rel_path.to_string_lossy().replace('\\', "/");

        if metadata.is_file() {
            // 在 Windows 上 entry.metadata() 可能返回缓存的大小,重新获取确保准确
            let metadata = if cfg!(windows) {
                fs::metadata(&path)?
            } else {
                metadata
            };

            entries.push(ScannedEntry {
                relative_path: rel_str,
                source_path: path,
                size: metadata.len(),
                is_dir: false,
                mode: entry_mode(&metadata, false),
                mtime: entry_mtime(&metadata),
            });
        } else if metadata.is_dir() {
            entries.push(ScannedEntry {
                relative_path: format!("{}/", rel_str),
                source_path: path.clone(),
                size: 0,
                is_dir: true,
                mode: entry_mode(&metadata, true),
                mtime: entry_mtime(&metadata),
            });

            scan_dir(base_path, &path, entries)?;
        }
    }

    Ok(())
}
