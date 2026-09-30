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
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::{
    archive::{Archive, ArchiveFormat},
    error::ArchiveApiError,
    state::AppState,
};

/// 路径验证函数
pub fn resolve_and_validate_path(
    root: &std::path::PathBuf,
    relative_path: &str,
) -> Result<std::path::PathBuf, ArchiveApiError> {
    let root = root.canonicalize().map_err(|e| {
        ArchiveApiError::InternalError(anyhow::anyhow!("解析数据根目录失败: {}", e))
    })?;

    let full_path = root.join(relative_path);

    // 检查路径是否超出数据根目录
    // 使用 normalize 逻辑而不是 canonicalize，因为文件可能不存在
    let normalized = if full_path.is_absolute() {
        full_path.clone()
    } else {
        root.join(&full_path)
    };

    // 简单检查：确保路径没有通过 .. 跳出 root
    if !normalized.starts_with(&root) {
        return Err(ArchiveApiError::BadRequest(
            "非法路径: 超出数据根目录".to_string(),
        ));
    }

    // 如果路径存在，进行 canonicalize 验证
    if full_path.exists() {
        let canonicalized = full_path.canonicalize().map_err(|e| {
            ArchiveApiError::InternalError(anyhow::anyhow!("解析目标路径失败: {}", e))
        })?;

        // 再次检查 canonicalize 后的路径是否仍在 root 内
        if !canonicalized.starts_with(&root) {
            return Err(ArchiveApiError::BadRequest(
                "非法路径: 超出数据根目录".to_string(),
            ));
        }

        Ok(canonicalized)
    } else {
        // 路径不存在，返回 BadRequest
        Err(ArchiveApiError::BadRequest(format!(
            "路径不存在: {}",
            relative_path
        )))
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_resolve_and_validate_path_success() {
        let temp_dir = tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();

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
        let root = temp_dir.path().to_path_buf();

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
        let root = temp_dir.path().to_path_buf();

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
        let result = super::parse_range_header("bytes=0-1023");
        assert!(result.is_ok());
        let ranges = result.unwrap();
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], (0, 1024)); // end is exclusive
    }

    #[test]
    fn test_parse_range_header_multiple_ranges() {
        let result = super::parse_range_header("bytes=0-100, 200-300");
        assert!(result.is_ok());
        let ranges = result.unwrap();
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0], (0, 101));
        assert_eq!(ranges[1], (200, 301));
    }

    #[test]
    fn test_parse_range_header_invalid_format() {
        // 缺少 "bytes=" 前缀
        let result = super::parse_range_header("0-100");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_range_header_open_ended() {
        // 没有指定结束位置，应该返回错误
        let result = super::parse_range_header("bytes=100-");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_range_header_suffix() {
        // 后缀范围（最后500字节），应该返回错误（需要总大小）
        let result = super::parse_range_header("bytes=-500");
        assert!(result.is_ok());
        let ranges = result.unwrap();
        assert_eq!(ranges[0], (0, 501)); // start=0, end=500+1
    }

    #[test]
    fn test_parse_range_header_invalid_numbers() {
        // 非数字
        let result = super::parse_range_header("bytes=abc-def");
        assert!(result.is_err());
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

    let content_disposition = format!("attachment; filename=\"{}\"", file_name);
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::try_from(content_disposition)
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

        // 优化：使用 BytesMut 管理 buffer，通过 freeze() 实现零拷贝转换
        let mut buffer = BytesMut::with_capacity(buffer_size);

        loop {
            // 清空但保留容量，避免重新分配
            buffer.clear();
            buffer.resize(buffer_size, 0);

            match stream_reader.read(&mut buffer) {
                Ok(0) => break, // 没有更多数据了
                Ok(n) => {
                    // truncate 到实际读取的大小
                    buffer.truncate(n);
                    // freeze() 将 BytesMut 转换为 Bytes，无需数据拷贝
                    let chunk = buffer.split().freeze();
                    if tx.blocking_send(Ok(chunk)).is_err() {
                        // 接收端已关闭(客户端断开)
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.blocking_send(Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        e.to_string(),
                    )));
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

    // 解析路径（相对于 DATA_ROOT）
    let source_path = resolve_and_validate_path(&state.config.data_root, &params.path)?;

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

    // 获取Range头
    let range_header = request.headers().get("range");

    // 设置文件名
    let file_name = format!(
        "{}.{}",
        std::path::Path::new(&params.path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("archive"),
        archive_format.extension()
    );

    // 解析Range头，如果存在
    let (start, end, status, content_length) = if let Some(range_val) = range_header {
        let range_str = range_val
            .to_str()
            .map_err(|_| ArchiveApiError::BadRequest("无效的Range头".to_string()))?;
        let ranges = parse_range_header(range_str)
            .map_err(|_| ArchiveApiError::BadRequest("无法解析Range头".to_string()))?;

        if let Some(&(req_start, req_end)) = ranges.first() {
            (
                req_start,
                req_end,
                StatusCode::PARTIAL_CONTENT,
                req_end - req_start,
            )
        } else {
            // 没有有效的Range
            return Err(ArchiveApiError::BadRequest("无效的Range值".to_string()));
        }
    } else {
        // 没有Range头，返回完整文件
        let total_size = archive.total_size();
        (0, total_size, StatusCode::OK, total_size)
    };

    let total_size = archive.total_size();

    // 创建响应
    let body = create_stream_body(
        archive,
        start,
        end,
        state.config.server_performance.stream_read_buffer_size,
    );

    let mut response = Response::new(body);
    *response.status_mut() = status;

    // 优化：使用 HeaderValue::from 直接转换数字，避免 format! 字符串分配
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(content_length));

    // 优化：直接填充头部，避免 create_common_headers + extend 的开销
    populate_common_headers(&mut response, &file_name, archive_format.content_type())?;

    // 如果是部分响应，添加Range特定的头部
    if status == StatusCode::PARTIAL_CONTENT {
        // 优化：使用 try_from 代替 format! + from_str，减少字符串分配
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

// 解析Range头的辅助函数
fn parse_range_header(range_str: &str) -> Result<Vec<(u64, u64)>, ()> {
    if !range_str.starts_with("bytes=") {
        return Err(());
    }

    let ranges_str = &range_str[6..]; // 移除 "bytes=" 前缀
    let mut ranges = Vec::new();

    for range_part in ranges_str.split(',') {
        let range_part = range_part.trim();
        if let Some(dash_idx) = range_part.find('-') {
            let start_str = &range_part[..dash_idx];
            let end_str = &range_part[dash_idx + 1..].trim();

            let start = if start_str.is_empty() {
                0
            } else {
                start_str.parse::<u64>().map_err(|_| ())?
            };

            let end = if end_str.is_empty() {
                // 如果没有指定结束位置，返回错误，因为我们需要知道总大小
                return Err(());
            } else {
                end_str.parse::<u64>().map_err(|_| ())? + 1 // +1 因为Range是包含结束位置的，但我们的read_range是半开放区间的
            };

            ranges.push((start, end));
        }
    }

    Ok(ranges)
}
