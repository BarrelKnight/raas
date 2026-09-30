use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use raas::{AppConfig, AppState, create_app_routes};
use std::fs;
use tempfile::TempDir;
use tower::ServiceExt;

#[tokio::test]
async fn test_download_full_archive() {
    // 创建测试目录和文件
    let temp_dir = TempDir::new().expect("创建临时目录失败");
    let test_file = temp_dir.path().join("test.txt");
    fs::write(&test_file, "Hello, World!").expect("写入测试文件失败");

    let config = AppConfig {
        data_root: temp_dir.path().to_path_buf(),
        bind_addr: "0.0.0.0:8080".to_string(),
        server_performance: Default::default(),
    };

    let state = AppState::new(config);
    let app = create_app_routes(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/archive/download?path=test.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/x-tar"
    );
    assert!(response.headers().contains_key("content-length"));

    // 验证响应体不为空
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(!body.is_empty());
}

#[tokio::test]
async fn test_download_directory() {
    // 创建测试目录结构
    let temp_dir = TempDir::new().expect("创建临时目录失败");
    let sub_dir = temp_dir.path().join("subdir");
    fs::create_dir(&sub_dir).expect("创建子目录失败");
    fs::write(sub_dir.join("file1.txt"), "File 1 content").expect("写入文件失败");
    fs::write(temp_dir.path().join("file2.txt"), "File 2 content").expect("写入文件失败");

    let config = AppConfig {
        data_root: temp_dir.path().to_path_buf(),
        bind_addr: "0.0.0.0:8080".to_string(),
        server_performance: Default::default(),
    };

    let state = AppState::new(config);
    let app = create_app_routes(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/archive/download?path=subdir")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/x-tar"
    );

    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(!body.is_empty());
}

#[tokio::test]
async fn test_range_request() {
    // 创建测试文件
    let temp_dir = TempDir::new().expect("创建临时目录失败");
    let test_file = temp_dir.path().join("large.txt");
    let content = "A".repeat(10000); // 10KB 文件
    fs::write(&test_file, &content).expect("写入测试文件失败");

    let config = AppConfig {
        data_root: temp_dir.path().to_path_buf(),
        bind_addr: "0.0.0.0:8080".to_string(),
        server_performance: Default::default(),
    };

    let state = AppState::new(config);
    let app = create_app_routes(state);

    // 测试 Range 请求
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/archive/download?path=large.txt")
                .header("range", "bytes=0-1023")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert!(response.headers().contains_key("content-range"));

    let content_range = response
        .headers()
        .get("content-range")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(content_range.starts_with("bytes 0-1023/"));

    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.len(), 1024);
}

#[tokio::test]
async fn test_path_traversal_protection() {
    let temp_dir = TempDir::new().expect("创建临时目录失败");
    fs::write(temp_dir.path().join("safe.txt"), "safe").expect("写入文件失败");

    let config = AppConfig {
        data_root: temp_dir.path().to_path_buf(),
        bind_addr: "0.0.0.0:8080".to_string(),
        server_performance: Default::default(),
    };

    let state = AppState::new(config);
    let app = create_app_routes(state);

    // 测试路径穿越攻击
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/archive/download?path=../../../etc/passwd")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_nonexistent_path() {
    let temp_dir = TempDir::new().expect("创建临时目录失败");

    let config = AppConfig {
        data_root: temp_dir.path().to_path_buf(),
        bind_addr: "0.0.0.0:8080".to_string(),
        server_performance: Default::default(),
    };

    let state = AppState::new(config);
    let app = create_app_routes(state);

    // 测试不存在的路径
    let response = app
        .oneshot(
            Request::builder()
                .uri("/download?path=nonexistent.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // 应该返回 404 或 500 错误
    assert!(response.status().is_client_error() || response.status().is_server_error());
}

#[tokio::test]
async fn test_empty_path_parameter() {
    let temp_dir = TempDir::new().expect("创建临时目录失败");

    let config = AppConfig {
        data_root: temp_dir.path().to_path_buf(),
        bind_addr: "0.0.0.0:8080".to_string(),
        server_performance: Default::default(),
    };

    let state = AppState::new(config);
    let app = create_app_routes(state);

    // 测试空路径参数
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/archive/download?path=")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_content_disposition_header() {
    let temp_dir = TempDir::new().expect("创建临时目录失败");
    fs::write(temp_dir.path().join("myfile.txt"), "content").expect("写入文件失败");

    let config = AppConfig {
        data_root: temp_dir.path().to_path_buf(),
        bind_addr: "0.0.0.0:8080".to_string(),
        server_performance: Default::default(),
    };

    let state = AppState::new(config);
    let app = create_app_routes(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/archive/download?path=myfile.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let content_disposition = response.headers().get("content-disposition").unwrap();
    let cd_str = content_disposition.to_str().unwrap();
    assert!(cd_str.contains("attachment"));
    assert!(cd_str.contains("myfile.txt.tar"));
}

#[tokio::test]
async fn test_accept_ranges_header() {
    let temp_dir = TempDir::new().expect("创建临时目录失败");
    fs::write(temp_dir.path().join("test.txt"), "content").expect("写入文件失败");

    let config = AppConfig {
        data_root: temp_dir.path().to_path_buf(),
        bind_addr: "0.0.0.0:8080".to_string(),
        server_performance: Default::default(),
    };

    let state = AppState::new(config);
    let app = create_app_routes(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/archive/download?path=test.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get("accept-ranges").unwrap(), "bytes");
}

#[tokio::test]
async fn test_concurrent_requests() {
    // 测试并发请求处理
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio::task;

    let temp_dir = TempDir::new().expect("创建临时目录失败");
    fs::write(temp_dir.path().join("file1.txt"), "Content 1").expect("写入文件失败");
    fs::write(temp_dir.path().join("file2.txt"), "Content 2").expect("写入文件失败");

    let config = AppConfig {
        data_root: temp_dir.path().to_path_buf(),
        bind_addr: "0.0.0.0:8080".to_string(),
        server_performance: Default::default(),
    };

    let state = AppState::new(config);
    let app = create_app_routes(state);
    let app = Arc::new(Mutex::new(app));

    // 发起 5 个并发请求
    let mut handles = vec![];
    for i in 0..5 {
        let app_clone = app.clone();
        let path = if i % 2 == 0 { "file1.txt" } else { "file2.txt" };

        let handle = task::spawn(async move {
            let mut app_locked = app_clone.lock().await;
            let response = (&mut *app_locked)
                .oneshot(
                    Request::builder()
                        .uri(format!("/api/archive/download?path={}", path))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            response.status()
        });
        handles.push(handle);
    }

    // 验证所有请求都成功
    for handle in handles {
        let status = handle.await.unwrap();
        assert_eq!(status, StatusCode::OK);
    }
}

#[tokio::test]
async fn test_large_file_download() {
    // 测试大文件下载（1MB）
    let temp_dir = TempDir::new().expect("创建临时目录失败");
    let large_file = temp_dir.path().join("large.bin");

    // 生成 1MB 数据
    let data = vec![0xAB; 1024 * 1024];
    fs::write(&large_file, &data).expect("写入大文件失败");

    let config = AppConfig {
        data_root: temp_dir.path().to_path_buf(),
        bind_addr: "0.0.0.0:8080".to_string(),
        server_performance: Default::default(),
    };

    let state = AppState::new(config);
    let app = create_app_routes(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/archive/download?path=large.bin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let content_length: u64 = response
        .headers()
        .get("content-length")
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();

    // 验证内容长度大于文件大小（包含tar头部）
    assert!(content_length > 1024 * 1024);
}

#[tokio::test]
async fn test_error_response_json_format() {
    // 验证错误响应的JSON格式
    let temp_dir = TempDir::new().expect("创建临时目录失败");

    let config = AppConfig {
        data_root: temp_dir.path().to_path_buf(),
        bind_addr: "0.0.0.0:8080".to_string(),
        server_performance: Default::default(),
    };

    let state = AppState::new(config);
    let app = create_app_routes(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/archive/download?path=")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8_lossy(&body);

    // 验证JSON格式
    assert!(body_str.contains("\"success\":false"));
    assert!(body_str.contains("\"error\""));
    assert!(body_str.contains("\"type\""));
    assert!(body_str.contains("\"message\""));
}

#[tokio::test]
async fn test_cache_invalidation_on_file_change() {
    use std::time::{Duration, Instant};

    let temp_dir = TempDir::new().expect("创建临时目录失败");
    let test_file = temp_dir.path().join("data.txt");
    fs::write(&test_file, "initial").expect("写入测试文件失败");

    // 缩短抖动窗口,加快测试
    let server_performance = raas::config::ServerPerformanceConfig {
        enable_file_watcher: true,
        file_watcher_debounce_ms: 50,
        ..Default::default()
    };

    let config = AppConfig {
        data_root: temp_dir.path().to_path_buf(),
        bind_addr: "0.0.0.0:8080".to_string(),
        server_performance,
    };

    let state = AppState::new(config);
    assert!(state.file_watcher.is_some(), "默认配置下应启用文件系统监听");
    let cache = state.archive_cache.clone();
    let app = create_app_routes(state);

    let canonical_file = test_file.canonicalize().expect("解析文件路径失败");

    // 首次请求,建立缓存并缓存源文件
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/archive/download?path=data.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let first_content_length: u64 = response
        .headers()
        .get("content-length")
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        cache.is_cached(&canonical_file),
        "首次请求后应将存档写入缓存"
    );

    // 修改文件内容为跨越 512 字节边界的长数据,确保 tar 总长发生变化
    fs::write(&test_file, "X".repeat(2000)).expect("更新测试文件失败");

    // 等待监听器自动失效缓存
    let deadline = Instant::now() + Duration::from_secs(10);
    while cache.is_cached(&canonical_file) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !cache.is_cached(&canonical_file),
        "文件变更后缓存应被自动失效"
    );

    // 再次请求,内容长度应反映更新后的文件
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/archive/download?path=data.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let second_content_length: u64 = response
        .headers()
        .get("content-length")
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        second_content_length > first_content_length,
        "缓存失效后重新扫描应反映更新的文件大小"
    );
}

#[tokio::test]
async fn test_file_watcher_can_be_disabled() {
    let temp_dir = TempDir::new().expect("创建临时目录失败");
    fs::write(temp_dir.path().join("a.txt"), "a").expect("写入测试文件失败");

    let server_performance = raas::config::ServerPerformanceConfig {
        enable_file_watcher: false,
        ..Default::default()
    };

    let config = AppConfig {
        data_root: temp_dir.path().to_path_buf(),
        bind_addr: "0.0.0.0:8080".to_string(),
        server_performance,
    };

    let state = AppState::new(config);
    assert!(
        state.file_watcher.is_none(),
        "禁用配置下不应启动文件系统监听"
    );
}

/// 构建一个测试用的应用与数据根目录
fn zip_test_app(temp_dir: &TempDir) -> axum::Router {
    let config = AppConfig {
        data_root: temp_dir.path().to_path_buf(),
        bind_addr: "0.0.0.0:8080".to_string(),
        server_performance: Default::default(),
    };

    create_app_routes(AppState::new(config))
}

#[tokio::test]
async fn test_download_zip_archive() {
    use std::io::Cursor;

    let temp_dir = TempDir::new().expect("创建临时目录失败");
    let sub_dir = temp_dir.path().join("subdir");
    fs::create_dir(&sub_dir).expect("创建子目录失败");
    fs::write(sub_dir.join("a.txt"), "zip content").expect("写入文件失败");

    let app = zip_test_app(&temp_dir);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/archive/download?path=subdir&format=zip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/zip"
    );
    assert_eq!(response.headers().get("accept-ranges").unwrap(), "bytes");

    let content_disposition = response
        .headers()
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(content_disposition.contains("subdir.zip"));

    let body = response.into_body().collect().await.unwrap().to_bytes();

    // 生成的字节流应为标准可解析的 zip
    let mut zip = zip::ZipArchive::new(Cursor::new(body.to_vec())).expect("生成的归档不是合法 zip");
    assert_eq!(zip.len(), 1);
    assert_eq!(zip.by_index(0).unwrap().name(), "a.txt");

    // 仅在启用 `zip-crc32` 时才能通过标准库读取内容(否则 CRC 字段为 0)
    #[cfg(feature = "zip-crc32")]
    {
        let mut entry = zip.by_name("a.txt").unwrap();
        let mut content = String::new();
        std::io::Read::read_to_string(&mut entry, &mut content).unwrap();
        assert_eq!(content, "zip content");
    }
}

#[tokio::test]
async fn test_zip_range_request() {
    let temp_dir = TempDir::new().expect("创建临时目录失败");
    fs::write(temp_dir.path().join("data.bin"), vec![0x33u8; 5000]).expect("写入文件失败");

    let app = zip_test_app(&temp_dir);

    // 先取完整归档,作为区间比较的基准
    let full = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/archive/download?path=data.bin&format=zip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/archive/download?path=data.bin&format=zip")
                .header("range", "bytes=512-1023")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let content_range = response
        .headers()
        .get("content-range")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(content_range.starts_with("bytes 512-1023/"));

    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body.len(), 512);
    assert_eq!(&body[..], &full[512..1024]);
}

#[tokio::test]
async fn test_unsupported_format_is_rejected() {
    let temp_dir = TempDir::new().expect("创建临时目录失败");
    fs::write(temp_dir.path().join("a.txt"), "a").expect("写入文件失败");

    let app = zip_test_app(&temp_dir);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/archive/download?path=a.txt&format=rar")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
