use std::sync::Arc;

use axum::{extract::{Path, RawQuery, State}, http::StatusCode, response::{IntoResponse, Response}};
use bytes::Bytes;

use crate::{arxiv::{ArxivClient, ArxivError}, cache::MkCache, convert::{ConvertError, Converter}};
use tokio::sync::Mutex;

pub async fn health() -> &'static str {
    "ok"
}

pub async fn paper(
    State(cache): State<Arc<Mutex<MkCache>>>,
    State(client): State<Arc<dyn ArxivClient + Send + Sync>>,
    State(converter): State<Arc<dyn Converter + Send + Sync>>,
    Path(id): Path<String>,
    raw_query: Option<RawQuery>,
) -> Response {
    // Minimal id validation: non-empty and ascii
    if id.trim().is_empty() || !id.is_ascii() {
        return (StatusCode::BAD_REQUEST, "invalid id").into_response();
    }

    let refresh = raw_query
        .and_then(|q| q.0)
        .unwrap_or_default()
        .split('&')
        .find_map(|kv| {
            let mut it = kv.splitn(2, '=');
            let k = it.next()?;
            let v = it.next().unwrap_or("");
            if k == "refresh" && v == "1" { Some(()) } else { None }
        })
        .is_some();

    if !refresh {
        if let Some(md) = cache.lock().await.get(&id) {
            return markdown_response(md);
        }
    }

    // Validate existence (best-effort; if not implemented treat as available)
    match client.exists(&id).await {
        Ok(false) => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(ArxivError::NotImplemented) => { /* proceed */ }
        Err(e) => {
            return map_arxiv_err(e);
        }
        Ok(true) => {}
    }

    let tar_bytes = match client.get_source_archive(&id).await {
        Ok(b) => b,
        Err(e) => return map_arxiv_err(e),
    };

    let md = match converter.latex_tar_to_markdown(&tar_bytes).await {
        Ok(s) => s,
        Err(e) => return map_convert_err(e),
    };

    cache.lock().await.put(id.clone(), md.clone());
    markdown_response(md)
}

fn markdown_response(md: String) -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
        md,
    )
        .into_response()
}

fn map_arxiv_err(e: ArxivError) -> Response {
    match e {
        ArxivError::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
        ArxivError::PdfOnly => (StatusCode::UNPROCESSABLE_ENTITY, "PDF only").into_response(),
        ArxivError::Network(msg) => (StatusCode::BAD_GATEWAY, msg).into_response(),
        ArxivError::NotImplemented => (StatusCode::NOT_IMPLEMENTED, "not implemented").into_response(),
    }
}

fn map_convert_err(e: ConvertError) -> Response {
    match e {
        ConvertError::Failed(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response(),
        ConvertError::NotImplemented => (StatusCode::NOT_IMPLEMENTED, "not implemented").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};
    use tower::ServiceExt; // for `oneshot`

    use crate::arxiv::test_helpers::MockArxivClient;
    use crate::convert::test_helpers::MockConverter;
    use crate::state::AppState;

    #[tokio::test]
    async fn health_ok() {
        let app = Router::new().route("/health", get(super::health));
        let res = app
            .oneshot(axum::http::Request::builder().uri("/health").body(axum::body::Body::empty())
            .unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn paper_happy_path_with_cache() {
        let id = "1234.5678";
        let tar = Bytes::from_static(b"tar-bytes");
        let md = "# Hello".to_string();

        let client = MockArxivClient::new(Ok(true), Ok(tar));
        let converter = MockConverter { result: Ok(md.clone()) };
        let state = AppState::new(8, client, converter);

        let app = Router::new()
            .route("/paper/:id", get(super::paper))
            .with_state(state.clone());

        // First request populates cache
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/paper/{}", id))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Second request should hit cache and still be OK
        let res2 = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/paper/{}", id))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res2.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn pdf_only_maps_to_422() {
        let id = "1234.5678";
        let client = MockArxivClient::new(Ok(true), Err(ArxivError::PdfOnly));
        let converter = MockConverter { result: Ok("".into()) };
        let state = AppState::new(8, client, converter);

        let app = Router::new()
            .route("/paper/:id", get(super::paper))
            .with_state(state);

        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/paper/{}", id))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn invalid_id_400() {
        let client = MockArxivClient::new(Ok(true), Err(ArxivError::PdfOnly));
        let converter = MockConverter { result: Ok("".into()) };
        let state = AppState::new(8, client, converter);

        let app = Router::new()
            .route("/paper/:id", get(super::paper))
            .with_state(state);

        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/paper/%FF%FF")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }
}
