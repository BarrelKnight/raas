mod archive;
mod cache;
mod config;
mod error;
mod server;
mod state;

use tower::limit::ConcurrencyLimitLayer;
use tracing::info;
use anyhow::{Result, Context};
use crate::config::AppConfig;
use tokio::runtime::Builder;
use std::net::SocketAddr;

fn main() -> Result<()> {
    // 加载配置
    let config = AppConfig::from_env()
        .context("加载配置失败")?;

    // 使用自定义运行时配置以支持线程池大小设置
    let runtime = Builder::new_multi_thread()
        .worker_threads(config.server_performance.thread_pool_size)
        .max_blocking_threads(config.server_performance.blocking_queue_size)
        .enable_all()
        .build()
        .context("创建Tokio运行时失败")?;
    
    runtime.block_on(async_main(config))
}

async fn async_main(config: AppConfig) -> Result<()> {
    // 初始化日志
    tracing_subscriber::fmt()
        .with_target(true)
        .with_level(true)
        .init();
    
    info!("启动 RAAS (Random Access Archive Stream) 服务...");
    info!("数据根目录: {:?}", config.data_root);

    // 初始化应用状态
    let state = state::AppState::new(config);

    let app = server::create_app_routes(state.clone())
        // 添加并发限制层
        .layer(ConcurrencyLimitLayer::new(state.config.server_performance.max_concurrent_requests));

    // 配置TCP监听器，应用自定义缓冲区设置
    let addr = state.config.bind_addr.parse::<SocketAddr>()
        .context("解析绑定地址失败")?;
    
    let socket = tokio::net::TcpSocket::new_v4()
        .context("创建TCP socket失败")?;
    
    // 设置发送和接收缓冲区大小
    socket.set_send_buffer_size(state.config.server_performance.send_buffer_size as u32)
        .context("设置发送缓冲区失败")?;
    socket.set_recv_buffer_size(state.config.server_performance.recv_buffer_size as u32)
        .context("设置接收缓冲区失败")?;
    
    socket.set_reuseaddr(true)
        .context("设置地址重用失败")?;
    socket.bind(addr)
        .context("绑定地址失败")?;
    
    let listener = socket.listen(1024)
        .context("监听连接失败")?;
    
    info!("服务器运行在 http://{}", state.config.bind_addr);
    info!("服务器性能配置:");
    info!("  最大并发请求数: {}", state.config.server_performance.max_concurrent_requests);
    info!("  线程池大小: {}", state.config.server_performance.thread_pool_size);
    info!("  阻塞队列大小: {}", state.config.server_performance.blocking_queue_size);
    info!("  发送缓冲区大小: {} 字节", state.config.server_performance.send_buffer_size);
    info!("  接收缓冲区大小: {} 字节", state.config.server_performance.recv_buffer_size);
    info!("API 端点:");
    info!("  GET /api/archive/download?path=<路径> - 流式压缩下载");
    info!("");
    info!("按 Ctrl+C 停止服务");
    
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .context("启动服务器失败")?;
    
    Ok(())
}
