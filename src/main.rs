use std::net::SocketAddr;

use axum::{routing::get, Router};

mod routes;
mod state;
mod cache;
mod arxiv;
mod convert;
mod tex_main;
mod disk_cache;

use crate::arxiv::ReqwestArxivClient;
use crate::convert::PandocConverter;
use crate::state::AppState;
use crate::disk_cache::{DiskCache, DiskCacheConfig};

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(8080);
    let cache_cap: usize = std::env::var("MARKXIV_CACHE_CAP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(128);

    let client = ReqwestArxivClient::new();
    let converter = PandocConverter::new();

    // Optional disk cache
    let disk_cap_bytes = std::env::var("MARKXIV_DISK_CACHE_CAP_BYTES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let disk = if disk_cap_bytes > 0 {
        let root = std::env::var("MARKXIV_CACHE_DIR").map(std::path::PathBuf::from).unwrap_or_else(|_| std::path::PathBuf::from("cache"));
        let sweep_secs = std::env::var("MARKXIV_SWEEP_INTERVAL_SECS").ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(600);
        let cfg = DiskCacheConfig { root, cap_bytes: disk_cap_bytes, sweep_interval: std::time::Duration::from_secs(sweep_secs) };
        match DiskCache::new(cfg).await {
            Ok(dc) => Some(dc),
            Err(e) => { eprintln!("disk cache init failed: {}", e); None }
        }
    } else { None };

    let state = AppState::new(cache_cap, client, converter, disk);

    let app = Router::new()
        .route("/", get(routes::index))
        .route("/health", get(routes::health))
        .route("/abs/:id", get(routes::paper))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("Listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    axum::serve(listener, app).await.expect("server");
}
