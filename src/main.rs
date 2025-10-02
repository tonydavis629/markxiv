use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;

use axum::{routing::get, Router};
use tower_http::trace::{DefaultMakeSpan, DefaultOnFailure, DefaultOnResponse, TraceLayer};

mod arxiv;
mod cache;
mod convert;
mod disk_cache;
mod routes;
mod state;
mod tex_main;

use crate::arxiv::ReqwestArxivClient;
use crate::convert::PandocConverter;
use crate::disk_cache::{DiskCache, DiskCacheConfig};
use crate::state::AppState;
use tracing::Level;
use tracing_subscriber::EnvFilter;

fn resolve_log_path(path_env: Option<OsString>, dir_env: Option<OsString>) -> PathBuf {
    if let Some(path) = path_env {
        let candidate = PathBuf::from(path);
        if !candidate.as_os_str().is_empty() {
            return candidate;
        }
    }

    let dir = dir_env
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("logs"));
    if dir.as_os_str().is_empty() {
        PathBuf::from("markxiv.log")
    } else {
        dir.join("markxiv.log")
    }
}

fn init_tracing() {
    let log_path = resolve_log_path(
        std::env::var_os("MARKXIV_LOG_PATH"),
        std::env::var_os("MARKXIV_LOG_DIR"),
    );
    if let Some(parent) = log_path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("failed to create log directory {}: {}", parent.display(), e);
            }
        }
    }

    let directory = log_path
        .parent()
        .map(|p| p.to_path_buf())
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."));
    let file_name: OsString = log_path
        .file_name()
        .map(|name| name.to_owned())
        .unwrap_or_else(|| OsString::from("markxiv.log"));

    let file_appender = tracing_appender::rolling::never(directory, file_name);
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_ansi(false)
        .with_target(false)
        .with_writer(non_blocking)
        .finish();

    if tracing::subscriber::set_global_default(subscriber).is_ok() {
        Box::leak(Box::new(guard));
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_log_path;
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn path_env_takes_precedence() {
        let result = resolve_log_path(
            Some(OsString::from("/var/log/custom.log")),
            Some(OsString::from("/should/not/use")),
        );
        assert_eq!(result, PathBuf::from("/var/log/custom.log"));
    }

    #[test]
    fn dir_env_used_when_path_missing() {
        let result = resolve_log_path(None, Some(OsString::from("/tmp/markxiv")));
        assert_eq!(result, PathBuf::from("/tmp/markxiv/markxiv.log"));
    }

    #[test]
    fn defaults_to_logs_directory() {
        let result = resolve_log_path(None, None);
        assert_eq!(result, PathBuf::from("logs/markxiv.log"));
    }

    #[test]
    fn empty_dir_env_uses_filename_only() {
        let result = resolve_log_path(None, Some(OsString::from("")));
        assert_eq!(result, PathBuf::from("markxiv.log"));
    }
}

#[tokio::main]
async fn main() {
    init_tracing();

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
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
        let root = std::env::var("MARKXIV_CACHE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("cache"));
        let sweep_secs = std::env::var("MARKXIV_SWEEP_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(600);
        let cfg = DiskCacheConfig {
            root,
            cap_bytes: disk_cap_bytes,
            sweep_interval: std::time::Duration::from_secs(sweep_secs),
        };
        match DiskCache::new(cfg).await {
            Ok(dc) => Some(dc),
            Err(e) => {
                tracing::error!(error = %e, "disk cache init failed");
                None
            }
        }
    } else {
        None
    };

    let state = AppState::new(cache_cap, client, converter, disk);

    let app = Router::new()
        .route("/", get(routes::index))
        .route("/health", get(routes::health))
        .route("/abs/:id", get(routes::paper))
        .route("/pdf/:id", get(routes::paper))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO))
                .on_failure(DefaultOnFailure::new().level(Level::ERROR)),
        )
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(%addr, "listening");
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    axum::serve(listener, app).await.expect("server");
}
