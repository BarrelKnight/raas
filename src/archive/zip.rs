//! Zip 归档的随机访问实现
//!
//! 设计要点:
//! - 采用 **STORED(不压缩)** 方式存储,避免为压缩额外占用一份存储,契合项目
//!   「不预创建完整归档、不占用双倍空间」的目标;
//! - 索引构建阶段只读取目录元数据(大小、类型),**不读取文件内容**,因此与 tar
//!   一样保持低内存、低首包延迟;归档总大小仅由元数据与文件名长度决定;
//! - Zip 规范要求每个条目记录 CRC-32,该值由 `zip-crc32` feature 控制:
//!   - **默认关闭**:CRC 字段直接写 0,**完全不读取文件内容**,与 tar 一致;
//!   - **启用后**:首次需要生成条目头部时读取文件计算 CRC 并缓存(见 [`ZipEntry::crc`]),
//!     因此仅访问文件内容区间或仅做 Range 下载不会触发 CRC 计算;
//! - 支持 Zip64 扩展:当条目大小、偏移量或条目数超过 Zip32 上限时自动写入 Zip64
//!   本地/中央目录扩展字段与 Zip64 EOCD 记录,从而支持超过 4GiB 的归档。

#[cfg(feature = "zip-crc32")]
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
#[cfg(feature = "zip-crc32")]
use std::sync::OnceLock;

use tracing::debug;
#[cfg(feature = "zip-crc32")]
use tracing::trace;

use super::scanner;
use super::{Archive, ArchiveError, ArchiveFormat};
use crate::cache::file_handle::FileHandleCache;

/// 本地文件头固定部分长度
const LOCAL_HEADER_FIXED: u64 = 30;
/// 中央目录项固定部分长度
const CENTRAL_ENTRY_FIXED: u64 = 46;
/// EOCD(中央目录结束记录)长度
const EOCD_FIXED: u64 = 22;
/// Zip64 EOCD 记录长度
const ZIP64_EOCD_LEN: u64 = 56;
/// Zip64 EOCD 定位器长度
const ZIP64_LOCATOR_LEN: u64 = 20;
/// Zip64 扩展字段头部长度(ID + 数据长度)
const ZIP64_EXTRA_HEADER: u64 = 4;
/// 本地头 Zip64 扩展字段数据长度(未压缩大小 + 压缩大小)
const ZIP64_LOCAL_DATA: u64 = 16;

/// Zip32 中表示「值见 Zip64 扩展」的哨兵值
const U32_MAX: u64 = 0xFFFF_FFFF;
const U16_MAX: u64 = 0xFFFF;

const SIGNATURE_LOCAL: u32 = 0x0403_4b50;
const SIGNATURE_CENTRAL: u32 = 0x0201_4b50;
const SIGNATURE_EOCD: u32 = 0x0605_4b50;
const SIGNATURE_ZIP64_EOCD: u32 = 0x0606_4b50;
const SIGNATURE_ZIP64_LOCATOR: u32 = 0x0706_4b50;

/// 通用标志位:文件名使用 UTF-8 编码
const FLAG_UTF8: u16 = 0x0800;
/// 压缩方式:存储(不压缩)
const METHOD_STORE: u16 = 0;
/// 解压所需版本:2.0(普通)/ 4.5(支持 Zip64)
const VERSION_DEFAULT: u16 = 20;
const VERSION_ZIP64: u16 = 45;
/// 固定的 DOS 时间戳(1980-01-01 00:00:00)
const DOS_TIME: u16 = 0;
const DOS_DATE: u16 = 0x0021;

/// 单个 Zip 条目
struct ZipEntry {
    /// 归档内路径(目录以 `/` 结尾)
    name: String,
    /// UTF-8 编码后的名称
    name_bytes: Vec<u8>,
    /// 文件源路径(目录为 `None`)
    source_path: Option<PathBuf>,
    /// 文件大小(目录为 0)
    size: u64,
    /// 是否为目录
    is_dir: bool,
    /// 本地头起始偏移
    local_offset: u64,
    /// 本地头字节长度
    local_header_len: u64,
    /// 本地头是否需要 Zip64 扩展(大小达到 Zip32 上限)
    local_zip64: bool,
    /// 中央目录项的大小字段是否需要 Zip64
    central_size_zip64: bool,
    /// 中央目录项的偏移字段是否需要 Zip64
    central_offset_zip64: bool,
    /// 中央目录项起始偏移
    central_offset: u64,
    /// 中央目录项字节长度
    central_len: u64,
    /// 按需计算并缓存的 CRC-32(仅在启用 `zip-crc32` feature 时存在)
    #[cfg(feature = "zip-crc32")]
    crc: OnceLock<Result<u32, String>>,
}

/// 归档内的一个连续区间
#[derive(Debug, Clone, Copy)]
enum SegmentKind {
    LocalHeader(usize),
    Data(usize),
    CentralEntry(usize),
    Zip64Eocd,
    Zip64Locator,
    Eocd,
}

/// 归档内按偏移排序的区间描述
#[derive(Debug, Clone, Copy)]
struct Segment {
    start: u64,
    len: u64,
    kind: SegmentKind,
}

/// 随机访问 Zip 归档
pub struct ZipArchive {
    entries: Vec<ZipEntry>,
    segments: Vec<Segment>,
    total_size: u64,
    cd_offset: u64,
    cd_size: u64,
    zip64_eocd_offset: u64,
}

impl ZipArchive {
    /// 创建 Zip 归档索引
    pub fn create(source_path: &Path) -> Result<Self, ArchiveError> {
        let scanned = scanner::scan_source(source_path)?;

        let mut entries: Vec<ZipEntry> = scanned
            .into_iter()
            .map(|item| ZipEntry {
                name_bytes: item.relative_path.as_bytes().to_vec(),
                name: item.relative_path,
                source_path: if item.is_dir {
                    None
                } else {
                    Some(item.source_path)
                },
                size: item.size,
                is_dir: item.is_dir,
                local_offset: 0,
                local_header_len: 0,
                local_zip64: false,
                central_size_zip64: false,
                central_offset_zip64: false,
                central_offset: 0,
                central_len: 0,
                #[cfg(feature = "zip-crc32")]
                crc: OnceLock::new(),
            })
            .collect();

        // 第一趟:布局本地头与文件数据,得到每个文件的偏移量
        let mut pos = 0u64;
        for entry in &mut entries {
            entry.local_zip64 = !entry.is_dir && entry.size >= U32_MAX;
            let extra = if entry.local_zip64 {
                ZIP64_EXTRA_HEADER + ZIP64_LOCAL_DATA
            } else {
                0
            };
            entry.local_offset = pos;
            entry.local_header_len = LOCAL_HEADER_FIXED + entry.name_bytes.len() as u64 + extra;
            pos += entry.local_header_len + entry.size;
        }

        // 第二趟:布局中央目录。大小与偏移字段是否使用 Zip64 仅依赖上面已确定的
        // 本地偏移量,不存在循环依赖。
        let cd_offset = pos;
        let mut cpos = pos;
        for entry in &mut entries {
            entry.central_size_zip64 = !entry.is_dir && entry.size >= U32_MAX;
            entry.central_offset_zip64 = entry.local_offset >= U32_MAX;

            let mut extra_data = 0u64;
            if entry.central_size_zip64 {
                extra_data += 16;
            }
            if entry.central_offset_zip64 {
                extra_data += 8;
            }
            let extra = if extra_data > 0 {
                ZIP64_EXTRA_HEADER + extra_data
            } else {
                0
            };

            entry.central_offset = cpos;
            entry.central_len = CENTRAL_ENTRY_FIXED + entry.name_bytes.len() as u64 + extra;
            cpos += entry.central_len;
        }

        let cd_size = cpos - cd_offset;
        let needs_zip64_eocd =
            entries.len() as u64 > U16_MAX || cd_size >= U32_MAX || cd_offset >= U32_MAX;
        let zip64_eocd_offset = cpos;

        let mut total = cpos;
        if needs_zip64_eocd {
            total += ZIP64_EOCD_LEN + ZIP64_LOCATOR_LEN;
        }
        total += EOCD_FIXED;

        // 构建区间表(按偏移升序且连续)
        let mut segments = Vec::with_capacity(entries.len() * 2 + 3);
        for (index, entry) in entries.iter().enumerate() {
            segments.push(Segment {
                start: entry.local_offset,
                len: entry.local_header_len,
                kind: SegmentKind::LocalHeader(index),
            });
            if entry.size > 0 {
                segments.push(Segment {
                    start: entry.local_offset + entry.local_header_len,
                    len: entry.size,
                    kind: SegmentKind::Data(index),
                });
            }
        }
        for (index, entry) in entries.iter().enumerate() {
            segments.push(Segment {
                start: entry.central_offset,
                len: entry.central_len,
                kind: SegmentKind::CentralEntry(index),
            });
        }
        if needs_zip64_eocd {
            segments.push(Segment {
                start: zip64_eocd_offset,
                len: ZIP64_EOCD_LEN,
                kind: SegmentKind::Zip64Eocd,
            });
            segments.push(Segment {
                start: zip64_eocd_offset + ZIP64_EOCD_LEN,
                len: ZIP64_LOCATOR_LEN,
                kind: SegmentKind::Zip64Locator,
            });
        }
        segments.push(Segment {
            start: total - EOCD_FIXED,
            len: EOCD_FIXED,
            kind: SegmentKind::Eocd,
        });

        debug!(
            source = %source_path.display(),
            entries = entries.len(),
            total_size = total,
            zip64 = needs_zip64_eocd,
            "zip 归档索引构建完成"
        );

        Ok(ZipArchive {
            entries,
            segments,
            total_size: total,
            cd_offset,
            cd_size,
            zip64_eocd_offset,
        })
    }

    /// 获取条目的 CRC-32
    ///
    /// 启用 `zip-crc32` 时,首次访问会读取对应文件计算 CRC 并缓存;
    /// 默认关闭时直接返回 0(CRC 字段写 0,不读取文件)。
    fn entry_crc(&self, entry: &ZipEntry) -> io::Result<u32> {
        if entry.is_dir {
            return Ok(0);
        }

        #[cfg(feature = "zip-crc32")]
        {
            let source = entry.source_path.as_ref().expect("文件条目必须包含源路径");

            match entry.crc.get_or_init(|| {
                trace!(path = %source.display(), size = entry.size, "计算 Zip 条目 CRC-32");
                compute_crc32(source, entry.size)
            }) {
                Ok(crc) => Ok(*crc),
                Err(message) => Err(io::Error::other(message.clone())),
            }
        }

        #[cfg(not(feature = "zip-crc32"))]
        {
            Ok(0)
        }
    }

    /// 生成本地文件头字节(仅供测试)
    #[cfg(test)]
    #[allow(dead_code)]
    fn local_header_bytes(&self, entry: &ZipEntry) -> io::Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(entry.local_header_len as usize);
        self.write_local_header(entry, &mut buf)?;
        Ok(buf)
    }

    /// 将本地文件头写入 `buf`(调用前 `buf` 应为空)
    fn write_local_header(&self, entry: &ZipEntry, buf: &mut Vec<u8>) -> io::Result<()> {
        let crc = self.entry_crc(entry)?;
        let version = if entry.local_zip64 {
            VERSION_ZIP64
        } else {
            VERSION_DEFAULT
        };

        buf.extend_from_slice(&SIGNATURE_LOCAL.to_le_bytes());
        buf.extend_from_slice(&version.to_le_bytes());
        buf.extend_from_slice(&FLAG_UTF8.to_le_bytes());
        buf.extend_from_slice(&METHOD_STORE.to_le_bytes());
        buf.extend_from_slice(&DOS_TIME.to_le_bytes());
        buf.extend_from_slice(&DOS_DATE.to_le_bytes());
        buf.extend_from_slice(&crc.to_le_bytes());
        if entry.local_zip64 {
            buf.extend_from_slice(&(U32_MAX as u32).to_le_bytes());
            buf.extend_from_slice(&(U32_MAX as u32).to_le_bytes());
        } else {
            let size = entry.size as u32;
            buf.extend_from_slice(&size.to_le_bytes());
            buf.extend_from_slice(&size.to_le_bytes());
        }
        buf.extend_from_slice(&(entry.name_bytes.len() as u16).to_le_bytes());
        let extra_len = if entry.local_zip64 {
            (ZIP64_EXTRA_HEADER + ZIP64_LOCAL_DATA) as u16
        } else {
            0
        };
        buf.extend_from_slice(&extra_len.to_le_bytes());
        buf.extend_from_slice(&entry.name_bytes);
        if entry.local_zip64 {
            buf.extend_from_slice(&0x0001u16.to_le_bytes());
            buf.extend_from_slice(&(ZIP64_LOCAL_DATA as u16).to_le_bytes());
            buf.extend_from_slice(&entry.size.to_le_bytes());
            buf.extend_from_slice(&entry.size.to_le_bytes());
        }

        debug_assert_eq!(buf.len() as u64, entry.local_header_len);
        Ok(())
    }

    /// 生成中央目录项字节(仅供测试)
    #[cfg(test)]
    #[allow(dead_code)]
    fn central_entry_bytes(&self, entry: &ZipEntry) -> io::Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(entry.central_len as usize);
        self.write_central_entry(entry, &mut buf)?;
        Ok(buf)
    }

    /// 将中央目录项写入 `buf`(调用前 `buf` 应为空)
    fn write_central_entry(&self, entry: &ZipEntry, buf: &mut Vec<u8>) -> io::Result<()> {
        let crc = self.entry_crc(entry)?;
        let needs_zip64 = entry.central_size_zip64 || entry.central_offset_zip64;
        let version = if needs_zip64 {
            VERSION_ZIP64
        } else {
            VERSION_DEFAULT
        };

        let mut extra_data: Vec<u8> = Vec::new();
        if entry.central_size_zip64 {
            extra_data.extend_from_slice(&entry.size.to_le_bytes());
            extra_data.extend_from_slice(&entry.size.to_le_bytes());
        }
        if entry.central_offset_zip64 {
            extra_data.extend_from_slice(&entry.local_offset.to_le_bytes());
        }
        let extra_len = if extra_data.is_empty() {
            0
        } else {
            (ZIP64_EXTRA_HEADER as usize + extra_data.len()) as u16
        };

        buf.extend_from_slice(&SIGNATURE_CENTRAL.to_le_bytes());
        buf.extend_from_slice(&VERSION_DEFAULT.to_le_bytes()); // version made by
        buf.extend_from_slice(&version.to_le_bytes());
        buf.extend_from_slice(&FLAG_UTF8.to_le_bytes());
        buf.extend_from_slice(&METHOD_STORE.to_le_bytes());
        buf.extend_from_slice(&DOS_TIME.to_le_bytes());
        buf.extend_from_slice(&DOS_DATE.to_le_bytes());
        buf.extend_from_slice(&crc.to_le_bytes());
        if entry.central_size_zip64 {
            buf.extend_from_slice(&(U32_MAX as u32).to_le_bytes());
            buf.extend_from_slice(&(U32_MAX as u32).to_le_bytes());
        } else {
            let size = entry.size as u32;
            buf.extend_from_slice(&size.to_le_bytes());
            buf.extend_from_slice(&size.to_le_bytes());
        }
        buf.extend_from_slice(&(entry.name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(&extra_len.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // 注释长度
        buf.extend_from_slice(&0u16.to_le_bytes()); // 起始磁盘号
        buf.extend_from_slice(&0u16.to_le_bytes()); // 内部属性
        let external = if entry.is_dir { 0x10u32 } else { 0u32 };
        buf.extend_from_slice(&external.to_le_bytes());
        let local_offset = if entry.central_offset_zip64 {
            U32_MAX as u32
        } else {
            entry.local_offset as u32
        };
        buf.extend_from_slice(&local_offset.to_le_bytes());
        buf.extend_from_slice(&entry.name_bytes);
        if !extra_data.is_empty() {
            buf.extend_from_slice(&0x0001u16.to_le_bytes());
            buf.extend_from_slice(&(extra_data.len() as u16).to_le_bytes());
            buf.extend_from_slice(&extra_data);
        }

        debug_assert_eq!(buf.len() as u64, entry.central_len);
        Ok(())
    }

    /// 生成 Zip64 EOCD 记录(仅供测试)
    #[cfg(test)]
    fn zip64_eocd_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(ZIP64_EOCD_LEN as usize);
        self.write_zip64_eocd(&mut buf);
        buf
    }

    /// 将 Zip64 EOCD 记录写入 `buf`(调用前 `buf` 应为空)
    fn write_zip64_eocd(&self, buf: &mut Vec<u8>) {
        let count = self.entries.len() as u64;
        buf.extend_from_slice(&SIGNATURE_ZIP64_EOCD.to_le_bytes());
        buf.extend_from_slice(&(ZIP64_EOCD_LEN - 12).to_le_bytes());
        buf.extend_from_slice(&VERSION_DEFAULT.to_le_bytes());
        buf.extend_from_slice(&VERSION_ZIP64.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
        buf.extend_from_slice(&self.cd_size.to_le_bytes());
        buf.extend_from_slice(&self.cd_offset.to_le_bytes());
    }

    /// 生成 Zip64 EOCD 定位器(仅供测试)
    #[cfg(test)]
    fn zip64_locator_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(ZIP64_LOCATOR_LEN as usize);
        self.write_zip64_locator(&mut buf);
        buf
    }

    /// 将 Zip64 EOCD 定位器写入 `buf`(调用前 `buf` 应为空)
    fn write_zip64_locator(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&SIGNATURE_ZIP64_LOCATOR.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&self.zip64_eocd_offset.to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes());
    }

    /// 生成 EOCD 记录(仅供测试)
    #[cfg(test)]
    fn eocd_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(EOCD_FIXED as usize);
        self.write_eocd(&mut buf);
        buf
    }

    /// 将 EOCD 记录写入 `buf`(调用前 `buf` 应为空)
    fn write_eocd(&self, buf: &mut Vec<u8>) {
        let count = self.entries.len() as u64;
        buf.extend_from_slice(&SIGNATURE_EOCD.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&(count.min(U16_MAX) as u16).to_le_bytes());
        buf.extend_from_slice(&(count.min(U16_MAX) as u16).to_le_bytes());
        buf.extend_from_slice(&(self.cd_size.min(U32_MAX) as u32).to_le_bytes());
        buf.extend_from_slice(&(self.cd_offset.min(U32_MAX) as u32).to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
    }

    /// 将非数据区间写入 `buf`(调用前 `buf` 会被清空)
    fn write_segment_into(&self, kind: SegmentKind, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.clear();
        match kind {
            SegmentKind::LocalHeader(index) => self.write_local_header(&self.entries[index], buf),
            SegmentKind::CentralEntry(index) => self.write_central_entry(&self.entries[index], buf),
            SegmentKind::Zip64Eocd => {
                self.write_zip64_eocd(buf);
                Ok(())
            }
            SegmentKind::Zip64Locator => {
                self.write_zip64_locator(buf);
                Ok(())
            }
            SegmentKind::Eocd => {
                self.write_eocd(buf);
                Ok(())
            }
            SegmentKind::Data(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "数据区间应按流式方式读取",
            )),
        }
    }

    /// 二分查找包含指定偏移的区间
    fn find_segment(&self, pos: u64) -> Option<usize> {
        let index = self
            .segments
            .partition_point(|segment| segment.start <= pos);
        if index == 0 {
            return None;
        }
        let candidate = index - 1;
        let segment = &self.segments[candidate];
        if pos < segment.start + segment.len {
            Some(candidate)
        } else {
            None
        }
    }
}

impl Archive for ZipArchive {
    fn format(&self) -> ArchiveFormat {
        ArchiveFormat::Zip
    }

    fn total_size(&self) -> u64 {
        self.total_size
    }

    fn stream_range(&self, start: u64, end: u64) -> Box<dyn Read + Send + '_> {
        Box::new(ZipRangeReader::new(self, start, end))
    }
}

/// Zip 归档的范围读取器
pub struct ZipRangeReader<'a> {
    archive: &'a ZipArchive,
    end: u64,
    current_pos: u64,
    file_handle_cache: FileHandleCache,
    /// 生成非数据区间(本地头 / 中央目录项 / EOCD)时的复用缓冲,避免逐区间分配
    scratch: Vec<u8>,
}

impl<'a> ZipRangeReader<'a> {
    fn new(archive: &'a ZipArchive, start: u64, end: u64) -> Self {
        ZipRangeReader {
            archive,
            end,
            current_pos: start,
            file_handle_cache: FileHandleCache::new(),
            scratch: Vec::new(),
        }
    }

    fn read_into_buffer(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.current_pos >= self.end || self.current_pos >= self.archive.total_size {
            return Ok(0);
        }

        let mut written = 0usize;
        let mut pos = self.current_pos;

        while written < buf.len() && pos < self.end && pos < self.archive.total_size {
            let segment_index = self.archive.find_segment(pos).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("无法定位偏移 {} 所属的 Zip 区间", pos),
                )
            })?;
            let segment = self.archive.segments[segment_index];
            let offset_in_segment = pos - segment.start;
            let want = (buf.len() - written)
                .min((segment.len - offset_in_segment) as usize)
                .min((self.end - pos) as usize);

            match segment.kind {
                SegmentKind::Data(entry_index) => {
                    let entry = &self.archive.entries[entry_index];
                    let source = entry.source_path.as_ref().expect("数据区间必须包含源路径");
                    let read = self.file_handle_cache.read_at(
                        source,
                        offset_in_segment,
                        &mut buf[written..written + want],
                    )?;
                    if read == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!("读取源文件 {} 时提前结束", entry.name),
                        ));
                    }
                    written += read;
                    pos += read as u64;
                }
                kind => {
                    let archive = self.archive;
                    archive.write_segment_into(kind, &mut self.scratch)?;
                    let start = offset_in_segment as usize;
                    buf[written..written + want]
                        .copy_from_slice(&self.scratch[start..start + want]);
                    written += want;
                    pos += want as u64;
                }
            }
        }

        self.current_pos = pos;
        Ok(written)
    }
}

impl Read for ZipRangeReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_into_buffer(buf)
    }
}

/// 流式计算文件的 CRC-32,并校验大小与索引一致
#[cfg(feature = "zip-crc32")]
fn compute_crc32(path: &Path, expected_size: u64) -> Result<u32, String> {
    let mut file = File::open(path).map_err(|e| format!("打开文件失败 {:?}: {}", path, e))?;
    let mut hasher = crc32fast::Hasher::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;

    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("读取文件失败 {:?}: {}", path, e))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }

    if total != expected_size {
        return Err(format!(
            "源文件在索引构建后发生变化: {:?} 期望 {} 字节, 实际 {} 字节",
            path, expected_size, total
        ));
    }

    Ok(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Cursor, Read};
    use tempfile::TempDir;

    /// 将指定区间读取为完整字节串
    fn read_range(archive: &ZipArchive, start: u64, end: u64) -> Vec<u8> {
        let mut reader = archive.stream_range(start, end);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        bytes
    }

    /// 读取整个归档并校验读取长度
    fn read_all(archive: &ZipArchive) -> Vec<u8> {
        let bytes = read_range(archive, 0, archive.total_size());
        assert_eq!(bytes.len() as u64, archive.total_size());
        bytes
    }

    #[test]
    fn test_zip_entries_listed_by_standard_library() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("hello.txt"), b"hello world").unwrap();
        fs::write(root.join("sub/data.bin"), vec![0x5Au8; 1500]).unwrap();
        fs::write(root.join("empty.txt"), b"").unwrap();
        fs::write(root.join("中文文件.txt"), "内容").unwrap();

        let archive = ZipArchive::create(root).unwrap();
        let bytes = read_all(&archive);

        // 中央目录应可被标准库解析,条目名(含多字节 UTF-8)正确
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).expect("生成的归档不是合法 zip");
        let mut names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "empty.txt",
                "hello.txt",
                "sub/",
                "sub/data.bin",
                "中文文件.txt"
            ]
        );
    }

    /// 启用 CRC 时,标准库读取内容会一并校验 CRC-32
    #[cfg(feature = "zip-crc32")]
    #[test]
    fn test_zip_content_verified_by_standard_library() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("hello.txt"), b"hello world").unwrap();
        fs::write(root.join("sub/data.bin"), vec![0x5Au8; 1500]).unwrap();
        fs::write(root.join("empty.txt"), b"").unwrap();
        fs::write(root.join("中文文件.txt"), "内容").unwrap();

        let archive = ZipArchive::create(root).unwrap();
        let bytes = read_all(&archive);
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();

        let mut content = String::new();
        {
            let mut hello = zip.by_name("hello.txt").unwrap();
            hello.read_to_string(&mut content).unwrap();
        }
        assert_eq!(content, "hello world");

        let mut buf = Vec::new();
        {
            let mut data = zip.by_name("sub/data.bin").unwrap();
            data.read_to_end(&mut buf).unwrap();
        }
        assert_eq!(buf, vec![0x5Au8; 1500]);

        let mut empty_content = Vec::new();
        {
            let mut empty = zip.by_name("empty.txt").unwrap();
            empty.read_to_end(&mut empty_content).unwrap();
        }
        assert!(empty_content.is_empty());

        let mut utf8 = String::new();
        {
            let mut file = zip.by_name("中文文件.txt").unwrap();
            file.read_to_string(&mut utf8).unwrap();
        }
        assert_eq!(utf8, "内容");
    }

    #[test]
    fn test_zip_single_file_source() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("only.txt");
        fs::write(&file, b"only content").unwrap();

        let archive = ZipArchive::create(&file).unwrap();
        let bytes = read_all(&archive);

        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        assert_eq!(zip.len(), 1);
        assert_eq!(zip.by_index(0).unwrap().name(), "only.txt");
    }

    #[test]
    fn test_zip_empty_directory_is_valid() {
        let temp = TempDir::new().unwrap();
        let archive = ZipArchive::create(temp.path()).unwrap();

        // 空归档仅包含 22 字节的 EOCD
        assert_eq!(archive.total_size(), 22);
        let bytes = read_all(&archive);

        let zip = zip::ZipArchive::new(Cursor::new(bytes)).expect("空 zip 应可被标准库解析");
        assert_eq!(zip.len(), 0);
    }

    #[test]
    fn test_zip_random_access_ranges_match_full_read() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("nested")).unwrap();
        fs::write(root.join("a.bin"), vec![0x11u8; 3000]).unwrap();
        fs::write(root.join("nested/b.txt"), "hello").unwrap();
        fs::write(root.join("c.bin"), vec![0x22u8; 700]).unwrap();

        let archive = ZipArchive::create(root).unwrap();
        let total = archive.total_size();
        let full = read_all(&archive);

        // 以非对齐块大小顺序读取,结果应与一次性读取完全一致
        let chunk = 137u64;
        let mut assembled = Vec::new();
        let mut pos = 0u64;
        while pos < total {
            let end = (pos + chunk).min(total);
            assembled.extend_from_slice(&read_range(&archive, pos, end));
            pos = end;
        }
        assert_eq!(assembled, full);

        // 任意起点读取应与整体切片一致
        let starts = [0u64, 1, 29, 30, 31, 512, total / 3, total / 2, total - 10];
        for start in starts {
            if start >= total {
                continue;
            }
            let end = (start + 256).min(total);
            let part = read_range(&archive, start, end);
            assert_eq!(
                part,
                &full[start as usize..end as usize],
                "区间 {}-{} 不一致",
                start,
                end
            );
        }
    }

    /// 默认(未启用 `zip-crc32`)时,本地头与中央目录的 CRC-32 字段应为 0
    #[cfg(not(feature = "zip-crc32"))]
    #[test]
    fn test_zip_crc_fields_are_zero_when_disabled() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join("a.txt"), b"data").unwrap();

        let archive = ZipArchive::create(root).unwrap();
        let entry = &archive.entries[0];

        // 本地文件头:偏移 14..18 为 CRC-32
        let local = archive.local_header_bytes(entry).unwrap();
        assert_eq!(u32::from_le_bytes(local[14..18].try_into().unwrap()), 0);

        // 中央目录项:偏移 16..20 为 CRC-32
        let central = archive.central_entry_bytes(entry).unwrap();
        assert_eq!(u32::from_le_bytes(central[16..20].try_into().unwrap()), 0);
    }

    #[cfg(feature = "zip-crc32")]
    #[test]
    fn test_zip_crc_computed_on_demand() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let content = b"payload-for-crc".to_vec();
        fs::write(root.join("a.txt"), &content).unwrap();

        let archive = ZipArchive::create(root).unwrap();
        assert!(
            archive
                .entries
                .iter()
                .all(|entry| entry.crc.get().is_none())
        );

        // 仅读取数据区间不应触发 CRC 计算
        let data_segment = *archive
            .segments
            .iter()
            .find(|segment| matches!(segment.kind, SegmentKind::Data(_)))
            .unwrap();
        let _ = read_range(
            &archive,
            data_segment.start,
            data_segment.start + data_segment.len,
        );
        assert!(
            archive.entries[0].crc.get().is_none(),
            "仅读取数据区间不应触发 CRC 计算"
        );

        // 读取完整归档(含本地头与中央目录)后 CRC 应已计算且正确
        let _ = read_all(&archive);
        let expected = crc32fast::hash(&content);
        assert_eq!(
            *archive.entries[0].crc.get().unwrap().as_ref().unwrap(),
            expected
        );
    }

    #[test]
    fn test_zip_small_archive_layout() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::write(root.join("f.txt"), b"abc").unwrap();

        let archive = ZipArchive::create(root).unwrap();

        // 小归档不应产生 Zip64 记录
        assert!(
            !archive
                .segments
                .iter()
                .any(|s| matches!(s.kind, SegmentKind::Zip64Eocd | SegmentKind::Zip64Locator))
        );

        let bytes = read_all(&archive);

        // 起始为本地文件头签名
        assert_eq!(&bytes[0..4], &[0x50, 0x4b, 0x03, 0x04]);

        // 末尾为 EOCD 签名,条目数为 1
        let len = bytes.len();
        assert_eq!(&bytes[len - 22..len - 18], &[0x50, 0x4b, 0x05, 0x06]);
        let count = u16::from_le_bytes([bytes[len - 12], bytes[len - 11]]);
        assert_eq!(count, 1);
    }

    #[test]
    fn test_zip64_records_encoding() {
        // 构造一个「大归档」索引,仅验证 Zip64 记录编码与 Zip32 字段截断逻辑
        let big = 0x1_0000_0000u64; // 4GiB,超出 Zip32 表示范围
        let archive = ZipArchive {
            entries: Vec::new(),
            segments: Vec::new(),
            total_size: big,
            cd_offset: big,
            cd_size: big,
            zip64_eocd_offset: big,
        };

        let zip64_eocd = archive.zip64_eocd_bytes();
        assert_eq!(zip64_eocd.len(), 56);
        assert_eq!(&zip64_eocd[0..4], &[0x50, 0x4b, 0x06, 0x06]);
        assert_eq!(
            u64::from_le_bytes(zip64_eocd[4..12].try_into().unwrap()),
            44
        );
        assert_eq!(
            u64::from_le_bytes(zip64_eocd[40..48].try_into().unwrap()),
            big
        );
        assert_eq!(
            u64::from_le_bytes(zip64_eocd[48..56].try_into().unwrap()),
            big
        );

        let locator = archive.zip64_locator_bytes();
        assert_eq!(locator.len(), 20);
        assert_eq!(&locator[0..4], &[0x50, 0x4b, 0x06, 0x07]);
        assert_eq!(u64::from_le_bytes(locator[8..16].try_into().unwrap()), big);

        // 标准 EOCD 中超出 Zip32 的字段应被截断为 0xFFFFFFFF
        let eocd = archive.eocd_bytes();
        assert_eq!(eocd.len(), 22);
        assert_eq!(
            u32::from_le_bytes(eocd[12..16].try_into().unwrap()),
            0xFFFF_FFFF
        );
        assert_eq!(
            u32::from_le_bytes(eocd[16..20].try_into().unwrap()),
            0xFFFF_FFFF
        );
    }
}
