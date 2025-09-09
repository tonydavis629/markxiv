use std::net::SocketAddr;

use axum::{routing::get, Router};

mod routes;
mod state;
mod cache;
mod arxiv;
mod convert;
mod tex_main;

use crate::arxiv::ReqwestArxivClient;
use crate::convert::PandocConverter;
use crate::state::AppState;

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(8080);
    let cache_cap: usize = std::env::var("MARKXIV_CACHE_CAP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(128);

    let client = ReqwestArxivClient::new();
    let converter = PandocConverter::new();

    let state = AppState::new(cache_cap, client, converter);

    let app = Router::new()
        .route("/", get(routes::index))
        .route("/health", get(routes::health))
        .route("/paper/:id", get(routes::paper))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("Listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    axum::serve(listener, app).await.expect("server");
}
