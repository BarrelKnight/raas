use anyhow::Result;
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use tar::{EntryType, Header};
use tracing::error;

use super::scanner;
use super::{Archive, ArchiveError, ArchiveFormat};
use crate::cache::file_handle::FileHandleCache;

/// tar 块大小
const BLOCK_SIZE: u64 = 512;

/// 归档结尾的零块数量(tar 规范要求两个 512 字节零块作为结束标记)
const END_OF_ARCHIVE_BLOCKS: u64 = 2;

/// 文件元数据信息
#[derive(Debug, Clone)]
pub struct FileInfo {
    /// 文件路径
    pub path: String,
    /// 在归档中的偏移量
    pub offset: u64,
    /// 文件大小
    pub size: u64,
    /// 是否是目录
    pub is_dir: bool,
    /// 权限位
    pub mode: u32,
    /// 修改时间(Unix 秒)
    pub mtime: u64,
}

/// 归档条目:元信息 + 预生成的头部字节
struct TarEntry {
    info: FileInfo,
    header: Vec<u8>,
}

/// 随机访问存档
pub struct TarArchive {
    /// 源路径
    source_path: PathBuf,
    /// 按 offset 升序排列的条目(条目间连续、无空隙)
    entries: Vec<TarEntry>,
    /// 归档内路径到下标的索引
    #[allow(dead_code)]
    path_index: HashMap<String, usize>,
    /// 文件总大小(含结尾零块)
    total_size: u64,
}

impl TarArchive {
    /// 创建新的随机访问存档
    pub fn create(source_path: &Path) -> Result<Self, ArchiveError> {
        // 扫描源路径,收集文件信息(元数据阶段,不读取文件内容)
        let scanned = scanner::scan_source(source_path)?;

        let mut entries: Vec<TarEntry> = Vec::with_capacity(scanned.len());
        let mut path_index: HashMap<String, usize> = HashMap::with_capacity(scanned.len());
        let mut current_pos = 0u64;

        for item in scanned {
            let mut info = FileInfo {
                path: item.relative_path,
                offset: 0, // 将在后续计算
                size: if item.is_dir { 0 } else { item.size },
                is_dir: item.is_dir,
                mode: item.mode,
                mtime: item.mtime,
            };

            // 创建头部,以确定是否需要额外的 LongLink 头部
            let header = Self::create_header(&info)?;

            // 对齐到 512 字节边界(条目本就按块对齐,此处作为兜底)
            info.offset = Self::align_to_boundary(current_pos, BLOCK_SIZE);
            let padding_size = Self::calculate_padding(info.size);
            current_pos = info.offset + header.len() as u64 + info.size + padding_size;

            path_index.insert(info.path.clone(), entries.len());
            entries.push(TarEntry { info, header });
        }

        // tar 规范要求归档以两个 512 字节零块结束
        let total_size = current_pos + END_OF_ARCHIVE_BLOCKS * BLOCK_SIZE;

        Ok(TarArchive {
            source_path: source_path.to_path_buf(),
            entries,
            path_index,
            total_size,
        })
    }

    /// 创建tar头部，返回主头部和可能的额外头部
    fn create_header(file_info: &FileInfo) -> Result<Vec<u8>, ArchiveError> {
        let mut header = tar::Header::new_gnu();

        if file_info.is_dir {
            header.set_size(0);
            header.set_entry_type(tar::EntryType::dir());
        } else {
            header.set_size(file_info.size);
            header.set_entry_type(tar::EntryType::file());
        }

        // 写入权限与修改时间,避免解包后得到 mode 000、时间恒为 1970 的文件
        header.set_mode(file_info.mode);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(file_info.mtime);

        // 使用GNU长路径扩展，而不是简单的截断
        let header_data = Self::set_path_with_gnu_extension(&mut header, &file_info.path)?;

        Ok(header_data)
    }

    fn align_to_512_bytes(data: &[u8]) -> Vec<u8> {
        let block_size = BLOCK_SIZE as usize;
        let current_len = data.len();
        let target_len = current_len.div_ceil(block_size) * block_size;

        let mut result = Vec::with_capacity(target_len);
        result.extend_from_slice(data);
        result.resize(target_len, 0);

        result
    }

    /// 使用GNU长路径扩展设置路径，返回拼接的字节数组
    fn set_path_with_gnu_extension(
        header: &mut Header,
        path: &str,
    ) -> Result<Vec<u8>, std::io::Error> {
        use std::str;

        // 尝试直接设置路径，如果失败则使用GNU长路径扩展
        if let Err(e) = header.set_path(path) {
            let path_bytes = Self::path2bytes(Path::new(path))?;
            let max = header.as_old().name.len();

            // 验证路径确实太长才使用扩展
            if path_bytes.len() <= max {
                return Err(e);
            }

            // 创建GNU LongLink头部
            let long_path_header = Self::prepare_header(path_bytes.len() as u64, b'L');

            // Calculate padding needed for 512-byte alignment
            let current_len = (path_bytes.len() + 1) as u64; // +1 for null terminator
            let padding_needed = Self::calculate_padding(current_len);

            // 设置截断路径到主头部，以确保头部有效
            let truncated =
                match str::from_utf8(&path_bytes[..std::cmp::min(max, path_bytes.len())]) {
                    Ok(s) => s,
                    Err(e) => str::from_utf8(&path_bytes[..e.valid_up_to()]).unwrap(),
                };
            // 使用标准set_path方法设置截断路径
            header.set_path(truncated)?;
            header.set_cksum();

            // 预计算所有部分的总大小
            let long_path_header_bytes = Self::align_to_512_bytes(long_path_header.as_bytes());
            let header_bytes = Self::align_to_512_bytes(header.as_bytes());
            let long_path_data_size = path_bytes.len() + 1 + padding_needed as usize; // null-terminated + padding

            // 一次性预分配所需空间
            let mut result = Vec::with_capacity(
                long_path_header_bytes.len() + long_path_data_size + header_bytes.len(),
            );

            // 按顺序拼接: long_path_header_bytes + long_path_data + header_bytes
            result.extend_from_slice(&long_path_header_bytes);

            // 直接写入路径数据 + null终止符 + 填充
            result.extend_from_slice(&path_bytes);
            result.push(0); // null terminator
            result.resize(result.len() + padding_needed as usize, 0); // padding

            result.extend_from_slice(&header_bytes);

            return Ok(result);
        }
        header.set_cksum();
        Ok(header.as_bytes().to_vec())
    }

    fn prepare_header(size: u64, entry_type: u8) -> Header {
        let mut header = Header::new_gnu();
        let name = b"././@LongLink";
        header.as_gnu_mut().unwrap().name[..name.len()].clone_from_slice(&name[..]);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        // + 1 to be compliant with GNU tar
        header.set_size(size + 1);
        header.set_entry_type(EntryType::new(entry_type));
        header.set_cksum();
        header
    }

    #[cfg(any(windows, target_arch = "wasm32"))]
    pub fn path2bytes(p: &Path) -> io::Result<Cow<'_, [u8]>> {
        p.as_os_str()
            .to_str()
            .map(|s| s.as_bytes())
            .ok_or_else(|| io::Error::other("path was not valid Unicode"))
            .map(|bytes| {
                if bytes.contains(&b'\\') {
                    // Normalize to Unix-style path separators
                    let mut bytes = bytes.to_owned();
                    for b in &mut bytes {
                        if *b == b'\\' {
                            *b = b'/';
                        }
                    }
                    Cow::Owned(bytes)
                } else {
                    Cow::Borrowed(bytes)
                }
            })
    }

    #[cfg(all(unix, not(target_arch = "wasm32")))]
    /// On unix this will never fail
    pub fn path2bytes(p: &Path) -> io::Result<Cow<'_, [u8]>> {
        use std::os::unix::ffi::OsStrExt;
        Ok(p.as_os_str().as_bytes()).map(Cow::Borrowed)
    }

    /// 计算填充大小
    fn calculate_padding(content_size: u64) -> u64 {
        (BLOCK_SIZE - (content_size % BLOCK_SIZE)) % BLOCK_SIZE
    }

    /// 对齐到指定边界
    fn align_to_boundary(offset: u64, boundary: u64) -> u64 {
        let remainder = offset % boundary;
        if remainder == 0 {
            offset
        } else {
            offset + (boundary - remainder)
        }
    }

    /// 创建一个按字节范围读取的只读流
    pub fn stream_range_writer<'a>(&'a self, start: u64, end: u64) -> RangeStreamWriter<'a> {
        RangeStreamWriter::new(self, start, end, FileHandleCache::new())
    }

    /// 获取所有文件列表
    #[cfg(test)]
    pub fn list_files(&self) -> Vec<&String> {
        self.entries.iter().map(|e| &e.info.path).collect()
    }

    /// 按归档内路径查询条目元信息
    #[allow(dead_code)]
    pub fn file_info(&self, path: &str) -> Option<&FileInfo> {
        self.path_index
            .get(path)
            .map(|&index| &self.entries[index].info)
    }
}

impl Archive for TarArchive {
    fn format(&self) -> ArchiveFormat {
        ArchiveFormat::Tar
    }

    fn total_size(&self) -> u64 {
        self.total_size
    }

    fn stream_range(&self, start: u64, end: u64) -> Box<dyn Read + Send + '_> {
        Box::new(self.stream_range_writer(start, end))
    }
}

/// 范围流读取器 - 实现 `Read` trait
pub struct RangeStreamWriter<'a> {
    archive: &'a TarArchive,
    end: u64,
    current_pos: u64,
    /// 当前定位到的条目下标,顺序读取时作为快速路径,避免每个数据块都做二分查找
    cursor: usize,
    file_handle_cache: FileHandleCache,
}

impl<'a> RangeStreamWriter<'a> {
    pub fn new(
        archive: &'a TarArchive,
        start: u64,
        end: u64,
        file_handle_cache: FileHandleCache,
    ) -> Self {
        RangeStreamWriter {
            archive,
            end,
            current_pos: start,
            cursor: 0,
            file_handle_cache,
        }
    }

    /// 条目 `index` 在归档中的结束偏移(头部 + 内容,不含填充)
    fn item_end(&self, index: usize) -> u64 {
        let entry = &self.archive.entries[index];
        entry.info.offset + entry.header.len() as u64 + entry.info.size
    }

    /// 定位包含偏移 `pos` 的条目下标;`None` 表示落在填充区或归档结尾零块
    fn locate(&mut self, pos: u64) -> Option<usize> {
        let count = self.archive.entries.len();
        if count == 0 {
            return None;
        }

        // 快速路径:仍位于游标所指条目内
        if self.cursor < count {
            let entry = &self.archive.entries[self.cursor];
            if pos >= entry.info.offset && pos < self.item_end(self.cursor) {
                return Some(self.cursor);
            }
        }

        // 二分查找最后一个 offset <= pos 的条目
        let candidate = self
            .archive
            .entries
            .partition_point(|entry| entry.info.offset <= pos);
        if candidate == 0 {
            return None;
        }

        let index = candidate - 1;
        if pos < self.item_end(index) {
            self.cursor = index;
            Some(index)
        } else {
            None
        }
    }

    // 内部方法,用于读取数据到目标缓冲区
    // 返回实际读取的字节数
    fn read_into_buffer(&mut self, buf: &mut [u8]) -> Result<usize, ArchiveError> {
        if self.current_pos >= self.end || self.current_pos >= self.archive.total_size {
            return Ok(0);
        }

        let mut bytes_written = 0;
        let mut pos = self.current_pos;

        // 读取数据直到填满 buf 或到达 end
        while bytes_written < buf.len() && pos < self.end && pos < self.archive.total_size {
            let Some(index) = self.locate(pos) else {
                // 填充区或归档结尾零块:写入零以满足 512 字节对齐 / 结束标记
                let remaining_space = (buf.len() - bytes_written) as u64;
                let limit = remaining_space
                    .min(self.end - pos)
                    .min(self.archive.total_size - pos);
                let padding_needed = TarArchive::calculate_padding(pos);
                // pos 已对齐且不在任何条目内时必定位于归档结尾零块,
                // 此时 padding 为 0,需退化为按剩余空间填充,避免死循环
                let bytes_to_add = if padding_needed == 0 {
                    limit
                } else {
                    padding_needed.min(limit)
                } as usize;

                buf[bytes_written..bytes_written + bytes_to_add].fill(0);
                bytes_written += bytes_to_add;
                pos += bytes_to_add as u64;
                continue;
            };

            let entry = &self.archive.entries[index];
            let header = &entry.header;
            let header_start = entry.info.offset;
            let header_end = header_start + header.len() as u64;
            let content_start = header_end;
            let content_end = content_start + entry.info.size;

            if pos >= header_start && pos < header_end {
                // 当前位置在头部区域 - 直接拷贝到 buf
                let pos_in_header = (pos - header_start) as usize;
                let max_bytes_from_header = header.len() - pos_in_header;
                let remaining_space = buf.len() - bytes_written;
                let bytes_to_read = std::cmp::min(
                    std::cmp::min(max_bytes_from_header, remaining_space),
                    (self.end - pos) as usize,
                );
                buf[bytes_written..bytes_written + bytes_to_read]
                    .copy_from_slice(&header[pos_in_header..pos_in_header + bytes_to_read]);
                bytes_written += bytes_to_read;
                pos += bytes_to_read as u64;
            } else if pos >= content_start && pos < content_end {
                // 当前位置在内容区域 - 直接读取到 buf
                let source_file_path = if self.archive.source_path.is_file() {
                    self.archive.source_path.clone()
                } else {
                    let normalized_path: Cow<str> =
                        Cow::Borrowed(entry.info.path.trim_end_matches('/'));
                    self.archive.source_path.join(normalized_path.as_ref())
                };

                let pos_in_content = pos - content_start;
                let remaining_space = buf.len() - bytes_written;
                let bytes_available_in_content = (content_end - pos) as usize;
                let bytes_to_read = std::cmp::min(
                    std::cmp::min(remaining_space, bytes_available_in_content),
                    (self.end - pos) as usize,
                );

                let bytes_read = self
                    .file_handle_cache
                    .read_at(
                        &source_file_path,
                        pos_in_content,
                        &mut buf[bytes_written..bytes_written + bytes_to_read],
                    )
                    .map_err(ArchiveError::Io)?;

                bytes_written += bytes_read;
                pos += bytes_read as u64;
            } else {
                error!(
                    "Unexpected position {} outside of file content range for {}",
                    pos, entry.info.path
                );
                return Err(ArchiveError::UnexpectedError(format!(
                    "Unexpected position {} outside of file content range for {}",
                    pos, entry.info.path
                )));
            }
        }

        self.current_pos = pos;
        Ok(bytes_written)
    }
}

impl<'a> std::io::Read for RangeStreamWriter<'a> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // 直接读取到 buf,消除中间 Vec 分配和拷贝
        self.read_into_buffer(buf)
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Archive;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;
    use tracing::info;

    #[test]
    fn test_random_access_archive_creation_and_metadata() {
        // 创建临时目录和文件
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let source_dir = temp_dir.path();

        // 创建一些测试文件
        let file1_path = source_dir.join("file1.txt");
        let mut file1 = File::create(&file1_path).unwrap();
        writeln!(file1, "这是第一个测试文件的内容").unwrap();
        file1.flush().unwrap(); // 确保写入磁盘

        let subdir = source_dir.join("subdirectory");
        std::fs::create_dir(&subdir).unwrap();

        let file2_path = subdir.join("file2.txt");
        let mut file2 = File::create(&file2_path).unwrap();
        writeln!(file2, "这是第二个测试文件的内容").unwrap();
        file2.flush().unwrap(); // 确保写入磁盘

        // 创建随机访问存档
        let archive = TarArchive::create(source_dir).expect("创建存档失败");

        // 测试元信息的正确性
        assert_eq!(archive.total_size(), archive.total_size);
        assert!(!archive.list_files().is_empty());

        // 验证文件列表包含正确的文件
        let files: Vec<&String> = archive.list_files();
        assert!(files.iter().any(|&f| f.contains("file1.txt")));
        assert!(files.iter().any(|&f| f.contains("subdirectory/file2.txt")));

        // 验证文件信息的正确性
        for file_path in files {
            let file_info = archive.file_info(file_path).unwrap();
            if file_path.ends_with('/') {
                assert!(file_info.is_dir);
                assert_eq!(file_info.size, 0);
            } else {
                assert!(!file_info.is_dir);
                assert!(file_info.size > 0);
            }
        }
    }

    #[test]
    fn test_random_access_archive_long_path_support() {
        // 创建临时目录和具有长路径的文件
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let source_dir = temp_dir.path();

        // 创建一个长路径
        let mut long_path = source_dir.to_path_buf();
        for i in 0..10 {
            long_path.push(format!(
                "very_long_directory_name_that_exceeds_standard_tar_limit_{}",
                i
            ));
        }
        long_path.push("very_long_filename.txt");

        // 确保路径存在
        if let Some(parent) = long_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
            info!("Created directory: {}", parent.display());
        }

        let mut file = File::create(&long_path).unwrap();
        writeln!(file, "这是一个具有超长路径的测试文件").unwrap();
        info!("Created file: {}", long_path.display());

        // 验证文件确实被创建
        assert!(long_path.exists(), "测试文件应存在: {:?}", long_path);
        let file_size = std::fs::metadata(&long_path).unwrap().len();
        info!("File size: {}", file_size);

        // 创建随机访问存档
        let archive = TarArchive::create(source_dir).expect("创建存档失败");

        // 验证长路径文件被正确处理
        let files: Vec<&String> = archive.list_files();
        info!("Total files in archive: {}", files.len());
        for f in &files {
            info!("Archive contains: {}", f);
        }

        let long_path_file_option = files.iter().find(|&f| f.contains("very_long_filename"));
        if let Some(long_path_file) = long_path_file_option {
            let file_info = archive.file_info(long_path_file).unwrap();
            info!(
                "Found file in archive: {} with size {} at offset {}",
                long_path_file, file_info.size, file_info.offset
            );
            assert!(!file_info.is_dir);
            assert!(file_info.size > 0);

            // 验证存档可以正确访问长路径文件
            assert!(archive.file_info(long_path_file).is_some());
        } else {
            panic!("未找到预期的长路径文件");
        }

        // 输出存档总大小
        info!("Archive total size: {}", archive.total_size());

        let mut stream_range_writer = archive.stream_range_writer(0, archive.total_size());
        let mut output_file = File::create("test_output.tar").unwrap();
        let bytes_copied = io::copy(&mut stream_range_writer, &mut output_file).unwrap();
        info!("Bytes copied to output file: {}", bytes_copied);
    }

    #[test]
    fn test_random_access_archive_random_access_capability() {
        // 创建临时目录和文件
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let source_dir = temp_dir.path();

        // 创建一个较大的测试文件
        let file_path = source_dir.join("large_test_file.txt");
        let mut file = File::create(&file_path).unwrap();
        let content = "这是一个用于测试随机访问功能的大文件内容。".repeat(100);
        write!(file, "{}", content).unwrap();

        // 创建另一个小文件
        let small_file_path = source_dir.join("small_file.txt");
        let mut small_file = File::create(&small_file_path).unwrap();
        write!(small_file, "小文件").unwrap();

        // 创建随机访问存档
        let archive = TarArchive::create(source_dir).expect("创建存档失败");

        // 测试随机访问功能
        // 创建一个范围读取器，模拟从存档中读取特定范围的数据
        let total_size = archive.total_size();
        assert!(total_size > 0);

        // 测试不同的读取范围
        if total_size > 10 {
            let mut reader = archive.stream_range_writer(0, 10);
            let mut buffer = [0u8; 10];
            let bytes_read = reader.read(&mut buffer).unwrap();

            // 至少应该能读取一些数据
            assert!(bytes_read > 0);
        }

        // 测试从中间位置开始读取
        if total_size > 20 {
            let middle = total_size / 2;
            let mut reader = archive.stream_range_writer(middle, middle + 10);
            let mut buffer = [0u8; 10];
            let bytes_read = reader.read(&mut buffer).unwrap();

            // 应该能够读取数据而不报错
            assert!(bytes_read > 0);
        }
    }

    #[test]
    fn test_random_access_archive_with_nested_directories() {
        // 创建临时目录和嵌套结构
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let source_dir = temp_dir.path();

        // 创建多层嵌套目录
        let nested_dir = source_dir.join("level1").join("level2").join("level3");
        std::fs::create_dir_all(&nested_dir).unwrap();

        // 在不同层级创建文件
        let file1 = source_dir.join("root_file.txt");
        File::create(&file1).unwrap();

        let file2 = source_dir.join("level1").join("first_level_file.txt");
        File::create(&file2).unwrap();

        let file3 = nested_dir.join("deep_file.txt");
        let mut deep_file = File::create(&file3).unwrap();
        write!(deep_file, "深层嵌套文件内容").unwrap();

        // 创建随机访问存档
        let archive = TarArchive::create(source_dir).expect("创建存档失败");

        // 验证所有层级的文件都被正确索引
        let files: Vec<&String> = archive.list_files();
        assert!(files.iter().any(|&f| f.contains("root_file.txt")));
        assert!(
            files
                .iter()
                .any(|&f| f.contains("level1/first_level_file.txt"))
        );
        assert!(
            files
                .iter()
                .any(|&f| f.contains("level1/level2/level3/deep_file.txt"))
        );

        // 验证目录也被正确索引
        assert!(files.iter().any(|&f| f.contains("level1/")));
        assert!(files.iter().any(|&f| f.contains("level1/level2/")));
        assert!(files.iter().any(|&f| f.contains("level1/level2/level3/")));
    }

    #[test]
    fn test_random_access_archive_empty_file() {
        // 测试 0 字节文件
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let source_dir = temp_dir.path();

        let empty_file = source_dir.join("empty.txt");
        File::create(&empty_file).unwrap();

        let archive = TarArchive::create(source_dir).expect("创建存档失败");

        let files: Vec<&String> = archive.list_files();
        assert_eq!(files.len(), 1);

        let file_info = archive.file_info("empty.txt").unwrap();
        assert!(!file_info.is_dir);
        assert_eq!(file_info.size, 0);
    }

    #[test]
    fn test_random_access_archive_single_file() {
        // 测试单个文件（非目录）
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let single_file = temp_dir.path().join("test.txt");

        {
            let mut file = File::create(&single_file).unwrap();
            write!(file, "Single file content").unwrap();
        }

        let archive = TarArchive::create(&single_file).expect("创建存档失败");

        let files: Vec<&String> = archive.list_files();
        assert_eq!(files.len(), 1);
        assert!(files.iter().any(|&f| f == "test.txt"));
    }

    #[test]
    fn test_random_access_archive_special_characters() {
        // 测试特殊字符文件名
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let source_dir = temp_dir.path();

        let special_file = source_dir.join("file with spaces & (parens).txt");
        {
            let mut file = File::create(&special_file).unwrap();
            write!(file, "Special content").unwrap();
        }

        let archive = TarArchive::create(source_dir).expect("创建存档失败");

        let files: Vec<&String> = archive.list_files();
        assert!(files.iter().any(|&f| f.contains("file with spaces")));
    }

    #[test]
    fn test_range_stream_writer_read_empty_range() {
        // 测试空范围读取
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let source_dir = temp_dir.path();

        let test_file = source_dir.join("test.txt");
        {
            let mut file = File::create(&test_file).unwrap();
            write!(file, "Test content").unwrap();
        }

        let archive = TarArchive::create(source_dir).expect("创建存档失败");

        // 尝试读取 0 字节范围
        let mut reader = archive.stream_range_writer(0, 0);
        let mut buffer = [0u8; 10];
        let bytes_read = reader.read(&mut buffer).unwrap();

        assert_eq!(bytes_read, 0);
    }

    #[test]
    fn test_range_stream_writer_read_beyond_total_size() {
        // 测试超出总大小的范围读取
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let source_dir = temp_dir.path();

        let test_file = source_dir.join("test.txt");
        {
            let mut file = File::create(&test_file).unwrap();
            write!(file, "Test").unwrap();
        }

        let archive = TarArchive::create(source_dir).expect("创建存档失败");
        let total_size = archive.total_size();

        // 尝试读取超出范围
        let mut reader = archive.stream_range_writer(total_size, total_size + 100);
        let mut buffer = [0u8; 100];
        let bytes_read = reader.read(&mut buffer).unwrap();

        assert_eq!(bytes_read, 0);
    }
}
