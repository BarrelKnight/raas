use async_stream::stream;
use axum::body::Body;
use axum::{
    Router,
    body::Bytes,
    extract::{Query, State},
    http::{HeaderValue, Request, StatusCode, header},
    response::Response,
    routing::get,
};
use bytes::BytesMut;
use serde::Deserialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::{
    archive::{Archive, ArchiveFormat},
    error::ArchiveApiError,
    state::AppState,
};

/// 解析并校验请求路径
///
/// `root` 必须是已规范化的绝对路径(见 [`AppState::data_root`]),
/// 以保证与 `canonicalize` 后的目标路径可直接前缀比较。
pub fn resolve_and_validate_path(
    root: &Path,
    relative_path: &str,
) -> Result<PathBuf, ArchiveApiError> {
    let full_path = root.join(relative_path);

    // 初筛:拼接结果必须仍位于根目录内(拦截显式跳出)
    if !full_path.starts_with(root) {
        return Err(ArchiveApiError::BadRequest(
            "非法路径: 超出数据根目录".to_string(),
        ));
    }

    if !full_path.exists() {
        return Err(ArchiveApiError::BadRequest(format!(
            "路径不存在: {}",
            relative_path
        )));
    }

    // 目标存在,进一步规范化以覆盖符号链接与 `..` 的情况
    let canonicalized = full_path
        .canonicalize()
        .map_err(|e| ArchiveApiError::InternalError(anyhow::anyhow!("解析目标路径失败: {}", e)))?;

    if !canonicalized.starts_with(root) {
        return Err(ArchiveApiError::BadRequest(
            "非法路径: 超出数据根目录".to_string(),
        ));
    }

    Ok(canonicalized)
}

#[cfg(test)]
mod path_tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_resolve_and_validate_path_success() {
        let temp_dir = tempdir().unwrap();
        // 契约:root 必须为已规范化路径
        let root = temp_dir.path().canonicalize().unwrap();

        // 创建测试子目录
        let test_dir = root.join("test");
        fs::create_dir(&test_dir).unwrap();

        // 测试正常路径解析
        let result = resolve_and_validate_path(&root, "test");
        assert!(result.is_ok());

        let resolved = result.unwrap();
        assert_eq!(
            resolved.canonicalize().unwrap(),
            test_dir.canonicalize().unwrap()
        );
    }

    #[test]
    fn test_resolve_and_validate_path_security() {
        let temp_dir = tempdir().unwrap();
        let root = temp_dir.path().canonicalize().unwrap();

        // 创建测试文件
        let safe_file = root.join("safe.txt");
        fs::write(&safe_file, b"safe").unwrap();

        // 正常路径应该成功
        let result = resolve_and_validate_path(&root, "safe.txt");
        assert!(result.is_ok());

        // 路径穿越应该失败
        let result = resolve_and_validate_path(&root, "../../../etc/passwd");
        assert!(result.is_err());

        // 绝对路径跳出 root 应该失败
        let result = resolve_and_validate_path(&root, "/etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_and_validate_path_nonexistent() {
        let temp_dir = tempdir().unwrap();
        let root = temp_dir.path().canonicalize().unwrap();

        // 不存在的路径应该返回 BadRequest
        let result = resolve_and_validate_path(&root, "nonexistent.txt");
        assert!(result.is_err());

        match result {
            Err(ArchiveApiError::BadRequest(msg)) => {
                assert!(msg.contains("不存在"));
            }
            _ => panic!("Expected BadRequest error"),
        }
    }

    #[test]
    fn test_parse_range_header_valid() {
        // 返回闭区间 (start, end)
        let result = super::parse_range_header("bytes=0-1023", 10_000);
        assert_eq!(result, Ok(Some((0, 1023))));
    }

    #[test]
    fn test_parse_range_header_multiple_ranges_uses_first() {
        // 仅支持单段区间,多段时取第一段
        let result = super::parse_range_header("bytes=0-100, 200-300", 10_000);
        assert_eq!(result, Ok(Some((0, 100))));
    }

    #[test]
    fn test_parse_range_header_invalid_format() {
        // 缺少 "bytes=" 前缀
        let result = super::parse_range_header("0-100", 10_000);
        assert_eq!(result, Err(super::RangeParseError::Malformed));
    }

    #[test]
    fn test_parse_range_header_open_ended() {
        // 未指定结束位置:一直读到结尾
        let result = super::parse_range_header("bytes=100-", 500);
        assert_eq!(result, Ok(Some((100, 499))));
    }

    #[test]
    fn test_parse_range_header_suffix() {
        // 后缀范围:最后 500 字节
        let result = super::parse_range_header("bytes=-500", 10_000);
        assert_eq!(result, Ok(Some((9_500, 9_999))));

        // 后缀长度超过总大小时退化为整个资源
        let result = super::parse_range_header("bytes=-500", 100);
        assert_eq!(result, Ok(Some((0, 99))));
    }

    #[test]
    fn test_parse_range_header_clamps_end() {
        // 结束位置超出总大小时裁剪到末尾
        let result = super::parse_range_header("bytes=0-999999999999", 5_632);
        assert_eq!(result, Ok(Some((0, 5_631))));
    }

    #[test]
    fn test_parse_range_header_unsatisfiable() {
        // 起始位置超出资源范围 -> 416
        let result = super::parse_range_header("bytes=999999999-1000000000", 5_632);
        assert_eq!(result, Err(super::RangeParseError::Unsatisfiable));

        let result = super::parse_range_header("bytes=100-", 100);
        assert_eq!(result, Err(super::RangeParseError::Unsatisfiable));

        // 空资源不存在可满足区间
        let result = super::parse_range_header("bytes=0-", 0);
        assert_eq!(result, Err(super::RangeParseError::Unsatisfiable));
    }

    #[test]
    fn test_parse_range_header_reversed_is_malformed() {
        // start > end 属于语法非法,应忽略整个 Range 头而不是 panic
        let result = super::parse_range_header("bytes=100-50", 5_632);
        assert_eq!(result, Err(super::RangeParseError::Malformed));
    }

    #[test]
    fn test_parse_range_header_invalid_numbers() {
        // 非数字
        let result = super::parse_range_header("bytes=abc-def", 10_000);
        assert_eq!(result, Err(super::RangeParseError::Malformed));
    }
}

/// 计算 RFC 5987(`filename*=`)所需的百分号编码
fn percent_encode_utf8(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        if unreserved {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0F) as usize] as char);
        }
    }
    out
}

/// 构建 `Content-Disposition`,对非 ASCII 文件名额外附带 RFC 5987 编码
fn build_content_disposition(file_name: &str) -> String {
    let encoded = percent_encode_utf8(file_name);
    let ascii_safe = file_name.is_ascii() && !file_name.contains(['"', '\\', '\r', '\n']);
    if ascii_safe {
        format!(
            "attachment; filename=\"{}\"; filename*=UTF-8''{}",
            file_name, encoded
        )
    } else {
        format!("attachment; filename*=UTF-8''{}", encoded)
    }
}

/// 填充通用响应头到已有的 Response 对象
fn populate_common_headers(
    response: &mut Response<Body>,
    file_name: &str,
    content_type: &'static str,
) -> Result<(), ArchiveApiError> {
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));

    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::try_from(build_content_disposition(file_name))
            .map_err(|_| ArchiveApiError::BadRequest("文件名包含非法字符".to_string()))?,
    );
    Ok(())
}

/// 流式响应体的有界通道容量(单位为缓冲块)
///
/// 单请求在途内存约为 `读取块大小 × (STREAM_CHANNEL_CAPACITY + 1)`,默认约 384KB。
/// 加大容量只会增加内存,不会提升吞吐。
const STREAM_CHANNEL_CAPACITY: usize = 2;

/// 流式读取块大小的下限
///
/// 读取循环在阻塞线程池中执行,每个数据块要经通道交接回异步流;
/// 块过小会使交接(系统调用)次数暴涨——实测 16KB 时的 CPU 开销约为 128KB 的 4 倍,
/// 因此对配置值设一个下限兜底。
const MIN_STREAM_READ_BUFFER_SIZE: usize = 64 * 1024;

/// 创建流式响应体
///
/// 归档读取是同步文件 I/O,若直接在异步流中轮询会阻塞 Tokio 工作线程
/// (线程池较小时会明显拖慢并发)。因此把读取循环放入 `spawn_blocking`,
/// 通过有界通道把数据块交回异步流。客户端断开时接收端被丢弃,
/// `blocking_send` 随即失败,阻断任务随之退出。
fn create_stream_body(archive: Arc<dyn Archive>, start: u64, end: u64, buffer_size: usize) -> Body {
    let buffer_size = buffer_size.max(MIN_STREAM_READ_BUFFER_SIZE);
    let (tx, mut rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(STREAM_CHANNEL_CAPACITY);

    tokio::task::spawn_blocking(move || {
        let mut stream_reader = archive.stream_range(start, end);

        // 每个数据块都以 Bytes 的所有权交给响应体,故每块需独立分配;
        // 这里通过复用同一 BytesMut 并只在必要时扩容来降低分配开销。
        let mut buffer = BytesMut::with_capacity(buffer_size);

        loop {
            // resize 会保留已有容量,仅在容量不足时扩容
            buffer.resize(buffer_size, 0);

            match stream_reader.read(&mut buffer) {
                Ok(0) => break, // 没有更多数据了
                Ok(n) => {
                    // truncate 到实际读取的大小
                    buffer.truncate(n);
                    // split() + freeze() 把缓冲区所有权转交给 Bytes,无需数据拷贝
                    let chunk = buffer.split().freeze();
                    if tx.blocking_send(Ok(chunk)).is_err() {
                        // 接收端已关闭(客户端断开)
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.blocking_send(Err(std::io::Error::other(e.to_string())));
                    break;
                }
            }
        }
    });

    let stream = stream! {
        while let Some(item) = rx.recv().await {
            yield item;
        }
    };

    Body::from_stream(stream)
}

// 压缩模块路由
pub fn archive_router() -> Router<AppState> {
    Router::new().route("/download", get(download_random_access_archive))
}

#[derive(Debug, Deserialize)]
pub struct RandomAccessArchiveQuery {
    path: String,
    /// 归档格式,支持 `tar`(默认)与 `zip`
    #[serde(default)]
    format: Option<String>,
}

// 支持Range请求的随机访问存档下载
pub async fn download_random_access_archive(
    State(state): State<AppState>,
    Query(params): Query<RandomAccessArchiveQuery>,
    request: Request<Body>,
) -> Result<Response, ArchiveApiError> {
    // 验证参数
    if params.path.is_empty() {
        return Err(ArchiveApiError::BadRequest("路径不能为空".to_string()));
    }

    // 解析归档格式(默认 tar)
    let requested_format = match params.format.as_deref() {
        None => ArchiveFormat::Tar,
        Some(value) => ArchiveFormat::from_name(value)
            .ok_or_else(|| ArchiveApiError::BadRequest(format!("不支持的归档格式: {}", value)))?,
    };

    // 解析路径（相对于 DATA_ROOT,根目录已在 AppState 中规范化）
    let source_path = resolve_and_validate_path(&state.data_root, &params.path)?;

    // 归档扫描是同步目录遍历,放在阻塞线程池中执行,避免占用 Tokio 工作线程
    let cache = state.archive_cache.clone();
    let archive =
        tokio::task::spawn_blocking(move || cache.get_or_create(&source_path, requested_format))
            .await
            .map_err(|e| {
                ArchiveApiError::InternalError(anyhow::anyhow!("归档构建任务失败: {}", e))
            })?
            .map_err(|e| {
                // 如果是路径不存在相关的错误，返回 BadRequest
                let error_msg = e.to_string();
                if error_msg.contains("不存在")
                    || error_msg.contains("not found")
                    || error_msg.contains("No such file")
                {
                    ArchiveApiError::BadRequest(error_msg)
                } else {
                    ArchiveApiError::InternalError(e)
                }
            })?;

    // 归档格式以实际创建结果为准(与缓存键保持一致)
    let archive_format = archive.format();
    let total_size = archive.total_size();

    // 设置文件名
    let file_name = format!(
        "{}.{}",
        std::path::Path::new(&params.path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("archive"),
        archive_format.extension()
    );

    // 解析 Range 头(基于归档总大小裁剪/判定是否可满足)
    let parsed_range = match request.headers().get(header::RANGE) {
        Some(value) => {
            let raw = value
                .to_str()
                .map_err(|_| ArchiveApiError::BadRequest("无效的Range头".to_string()))?;
            parse_range_header(raw, total_size)
        }
        None => Ok(None),
    };

    let (start, end, status, content_length) = match parsed_range {
        // 可满足的区间:end 由闭区间转为半开区间
        Ok(Some((range_start, range_end))) => (
            range_start,
            range_end + 1,
            StatusCode::PARTIAL_CONTENT,
            range_end + 1 - range_start,
        ),
        // 语法非法的 Range 按规范忽略,返回完整内容
        Ok(None) | Err(RangeParseError::Malformed) => (0, total_size, StatusCode::OK, total_size),
        // 语法合法但无法满足:返回 416 并带 `Content-Range: bytes */total`
        Err(RangeParseError::Unsatisfiable) => {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            let content_range = format!("bytes */{}", total_size);
            response.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::try_from(content_range).map_err(|_| {
                    ArchiveApiError::InternalError(anyhow::anyhow!("无效的 Content-Range 头"))
                })?,
            );
            return Ok(response);
        }
    };

    // 创建响应
    let body = create_stream_body(
        archive,
        start,
        end,
        state.config.server_performance.stream_read_buffer_size,
    );

    let mut response = Response::new(body);
    *response.status_mut() = status;

    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(content_length));

    populate_common_headers(&mut response, &file_name, archive_format.content_type())?;

    // 如果是部分响应，添加Range特定的头部(end 为半开区间,减 1 得到包含端点)
    if status == StatusCode::PARTIAL_CONTENT {
        let content_range = format!("bytes {}-{}/{}", start, end - 1, total_size);
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::try_from(content_range).map_err(|_| {
                ArchiveApiError::InternalError(anyhow::anyhow!("无效的 Content-Range 头"))
            })?,
        );
    }

    Ok(response)
}

/// Range 解析失败的原因
#[derive(Debug, PartialEq, Eq)]
enum RangeParseError {
    /// 语法非法,按 HTTP 规范应忽略整个 `Range` 头(返回完整内容)
    Malformed,
    /// 语法合法但与资源范围不相交,应返回 416
    Unsatisfiable,
}

/// 解析 `Range` 头,返回满足的单段闭区间 `(start, end)`
///
/// 支持 `bytes=start-end`、`bytes=start-` 与 `bytes=-suffix` 三种形式;
/// 多段区间仅取第一段。返回值语义:
/// - `Ok(None)`:未提供可解析的区间(应返回完整内容)
/// - `Ok(Some((start, end)))`:已按总大小裁剪的闭区间
/// - `Err(RangeParseError)`:语法非法或无法满足
fn parse_range_header(
    range_str: &str,
    total_size: u64,
) -> Result<Option<(u64, u64)>, RangeParseError> {
    let specs = range_str
        .strip_prefix("bytes=")
        .ok_or(RangeParseError::Malformed)?
        .trim();
    if specs.is_empty() {
        return Err(RangeParseError::Malformed);
    }

    // 空资源不存在任何可满足的字节区间
    if total_size == 0 {
        return Err(RangeParseError::Unsatisfiable);
    }

    let first = specs.split(',').next().unwrap_or("").trim();
    let (start_str, end_str) = first.split_once('-').ok_or(RangeParseError::Malformed)?;
    let (start_str, end_str) = (start_str.trim(), end_str.trim());

    match (start_str.is_empty(), end_str.is_empty()) {
        // -N:最后 N 字节
        (true, false) => {
            let suffix: u64 = end_str.parse().map_err(|_| RangeParseError::Malformed)?;
            if suffix == 0 {
                return Err(RangeParseError::Unsatisfiable);
            }
            let len = suffix.min(total_size);
            Ok(Some((total_size - len, total_size - 1)))
        }
        // N-:从 N 到结尾
        (false, true) => {
            let start: u64 = start_str.parse().map_err(|_| RangeParseError::Malformed)?;
            if start >= total_size {
                return Err(RangeParseError::Unsatisfiable);
            }
            Ok(Some((start, total_size - 1)))
        }
        // N-M:显式区间
        (false, false) => {
            let start: u64 = start_str.parse().map_err(|_| RangeParseError::Malformed)?;
            let end: u64 = end_str.parse().map_err(|_| RangeParseError::Malformed)?;
            // last-byte-pos 小于 first-byte-pos 属于非法语法,忽略整个头
            if start > end {
                return Err(RangeParseError::Malformed);
            }
            if start >= total_size {
                return Err(RangeParseError::Unsatisfiable);
            }
            Ok(Some((start, end.min(total_size - 1))))
        }
        (true, true) => Err(RangeParseError::Malformed),
    }
}
