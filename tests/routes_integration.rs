use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::Request;
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use tower::ServiceExt;

use markxiv::arxiv::{test_helpers::MockArxivClient, ArxivError, Metadata};
use markxiv::convert::{test_helpers::MockConverter, ConvertError};
use markxiv::disk_cache::{DiskCache, DiskCacheConfig};
use markxiv::routes;
use markxiv::state::AppState;

fn tmp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("markxiv-int-{}-{}", name, uuid()))
}

fn uuid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{:x}", nanos)
}

#[tokio::test]
async fn disk_cache_survives_across_states() {
    let root = tmp_dir("disk-cache");
    let cfg = DiskCacheConfig {
        root: root.clone(),
        cap_bytes: 1_000_000,
        sweep_interval: Duration::from_secs(600),
    };
    let disk = DiskCache::new(cfg).await.unwrap();

    let tar_bytes = Bytes::from_static(b"tar-bytes");
    let body_md = "# Hello".to_string();

    let client1 = MockArxivClient::new(
        Ok(true),
        Ok(tar_bytes.clone()),
        Err(ArxivError::NotImplemented),
        Err(ArxivError::NotImplemented),
    );
    let converter1 = MockConverter::new(Ok(body_md.clone()), Ok(body_md.clone()));
    let state1 = AppState::new(8, client1, converter1, Some(disk.clone()));

    let app1 = Router::new()
        .route("/abs/:id", get(routes::paper))
        .with_state(state1.clone());

    let res = app1
        .clone()
        .oneshot(
            Request::builder()
                .uri("/abs/1234.5678")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), axum::http::StatusCode::OK);
    let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), body_md.as_bytes());

    // New state with failing client should be satisfied from disk
    let client2 = MockArxivClient::new(
        Ok(true),
        Err(ArxivError::Network("should not fetch".into())),
        Err(ArxivError::Network("should not fetch".into())),
        Err(ArxivError::Network("should not fetch".into())),
    );
    let archive_calls = client2.archive_calls.clone();
    let pdf_calls = client2.pdf_calls.clone();
    let metadata_calls = client2.metadata_calls.clone();
    let converter2 = MockConverter::new(
        Err(ConvertError::Failed("should not convert".into())),
        Err(ConvertError::Failed("should not convert".into())),
    );
    let state2 = AppState::new(8, client2, converter2, Some(disk.clone()));

    let app2 = Router::new()
        .route("/abs/:id", get(routes::paper))
        .with_state(state2.clone());

    let res2 = app2
        .clone()
        .oneshot(
            Request::builder()
                .uri("/abs/1234.5678")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res2.status(), axum::http::StatusCode::OK);
    let body2 = to_bytes(res2.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body2.as_ref(), body_md.as_bytes());
    assert_eq!(archive_calls.load(Ordering::SeqCst), 0);
    assert_eq!(pdf_calls.load(Ordering::SeqCst), 0);
    assert_eq!(metadata_calls.load(Ordering::SeqCst), 0);

    let _ = tokio::fs::remove_dir_all(root).await;
}

#[tokio::test]
async fn refresh_query_triggers_pdf_fallback() {
    let tar_bytes = Bytes::from_static(b"tar-bytes");
    let pdf_bytes = Bytes::from_static(b"pdf-bytes");
    let client = MockArxivClient::new(
        Ok(true),
        Ok(tar_bytes.clone()),
        Ok(pdf_bytes.clone()),
        Ok(Metadata {
            title: String::new(),
            summary: String::new(),
            authors: Vec::new(),
        }),
    );
    let archive_calls = client.archive_calls.clone();
    let pdf_calls = client.pdf_calls.clone();

    let mut converter = MockConverter::new(
        Err(ConvertError::Failed("primary".into())),
        Ok("pdf text".into()),
    );
    converter.latex_nomacro_result = Some(Err(ConvertError::Failed("retry".into())));
    let latex_calls = converter.latex_calls.clone();
    let latex_nomacro_calls = converter.latex_nomacro_calls.clone();
    let converter_pdf_calls = converter.pdf_calls.clone();

    let state = AppState::new(8, client, converter, None);

    let app = Router::new()
        .route("/abs/:id", get(routes::paper))
        .with_state(state.clone());

    let req = |uri: &str| Request::builder().uri(uri).body(Body::empty()).unwrap();

    for uri in ["/abs/5678.1234", "/abs/5678.1234?refresh=1"] {
        let resp = app.clone().oneshot(req(uri)).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body.as_ref(), b"pdf text");
    }

    assert_eq!(archive_calls.load(Ordering::SeqCst), 2);
    assert_eq!(pdf_calls.load(Ordering::SeqCst), 2);
    assert_eq!(latex_calls.load(Ordering::SeqCst), 2);
    assert_eq!(latex_nomacro_calls.load(Ordering::SeqCst), 2);
    assert_eq!(converter_pdf_calls.load(Ordering::SeqCst), 2);
}
