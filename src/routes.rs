use std::sync::Arc;

use axum::{extract::{Path, RawQuery, State}, http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}};

use crate::{arxiv::{ArxivClient, ArxivError, Metadata}, cache::MkCache, convert::{ConvertError, Converter}, disk_cache::DiskCache};
use tokio::sync::Mutex;

pub async fn index(headers: HeaderMap) -> Response {
    let path = std::env::var("MARKXIV_INDEX_MD").unwrap_or_else(|_| "content/index.md".to_string());
    match tokio::fs::read_to_string(&path).await {
        Ok(md) => {
            let wants_html = wants_html(headers.get(axum::http::header::ACCEPT).and_then(|v| v.to_str().ok()));
            if wants_html {
                let html = render_markdown_html(&md);
                (
                    StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    html,
                )
                    .into_response()
            } else {
                (
                    StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
                    md,
                )
                    .into_response()
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to read index markdown: {}", e),
        )
            .into_response(),
    }
}

fn wants_html(accept: Option<&str>) -> bool {
    match accept {
        None => true,
        Some(s) => {
            let s = s.to_ascii_lowercase();
            s.contains("text/html") || s.contains("*/*")
        }
    }
}

fn render_markdown_html(md: &str) -> String {
    use pulldown_cmark::{html, Options, Parser};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(md, opts);
    let mut html_output = String::from("<!doctype html><meta charset=\"utf-8\"><title>markxiv</title><body>");
    html::push_html(&mut html_output, parser);
    html_output.push_str("</body>");
    html_output
}

pub async fn health() -> &'static str {
    "ok"
}

pub async fn paper(
    State(cache): State<Arc<Mutex<MkCache>>>,
    State(client): State<Arc<dyn ArxivClient + Send + Sync>>,
    State(converter): State<Arc<dyn Converter + Send + Sync>>,
    State(disk): State<Option<Arc<DiskCache>>>,
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
        if let Some(dc) = &disk {
            match dc.get(&id).await {
                Ok(Some(md)) => {
                    cache.lock().await.put(id.clone(), md.clone());
                    return markdown_response(md);
                }
                Ok(None) => {}
                Err(e) => eprintln!("disk cache read error: {}", e),
            }
        }
    }

    // Fetch metadata (title, abstract). If not implemented, continue without them.
    let metadata = match client.get_metadata(&id).await {
        Ok(m) => Some(m),
        Err(ArxivError::NotFound) => return (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(ArxivError::NotImplemented) => None,
        Err(e) => return map_arxiv_err(e),
    };

    let tar_bytes = match client.get_source_archive(&id).await {
        Ok(b) => b,
        Err(e) => return map_arxiv_err(e),
    };

    let body_md = match converter.latex_tar_to_markdown(&tar_bytes).await {
        Ok(s) => s,
        Err(e) => return map_convert_err(e),
    };

    let final_md = if let Some(meta) = metadata {
        prepend_metadata(&meta, &body_md)
    } else {
        body_md
    };

    cache.lock().await.put(id.clone(), final_md.clone());
    if let Some(dc) = &disk {
        if let Err(e) = dc.put(&id, &final_md).await {
            eprintln!("disk cache write error: {}", e);
        }
    }
    markdown_response(final_md)
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
        ArxivError::PdfOnly => (StatusCode::UNPROCESSABLE_ENTITY, "Error: PDF only").into_response(),
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

fn strip_html_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '<' => {
                in_tag = true;
            }
            '>' => {
                if in_tag {
                    in_tag = false;
                } else {
                    out.push(ch);
                }
            }
            _ => {
                if !in_tag {
                    out.push(ch);
                }
            }
        }
    }
    out
}

fn prepend_metadata(meta: &Metadata, body_md: &str) -> String {
    let title = strip_html_tags(&meta.title).trim().to_string();
    let abstract_text = strip_html_tags(&meta.summary).trim().to_string();
    let mut out = String::new();
    if !title.is_empty() {
        out.push_str("# ");
        out.push_str(&title);
        out.push_str("\n\n");
    }
    if !abstract_text.is_empty() {
        out.push_str("Abstract: ");
        out.push_str(&abstract_text);
        out.push_str("\n\n");
    }
    out.push_str(body_md);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};
    use bytes::Bytes;
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

        let meta = Metadata { title: "Sample Title".into(), summary: "Sample abstract".into() };
        let client = MockArxivClient::new(Ok(true), Ok(tar), Ok(meta));
        let converter = MockConverter { result: Ok(md.clone()) };
        let state = AppState::new(8, client, converter, None);

        let app = Router::new()
            .route("/abs/:id", get(super::paper))
            .with_state(state.clone());

        // First request populates cache
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/abs/{}", id))
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
                    .uri(format!("/abs/{}", id))
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
        let client = MockArxivClient::new(Ok(true), Err(ArxivError::PdfOnly), Ok(Metadata { title: String::new(), summary: String::new() }));
        let converter = MockConverter { result: Ok("".into()) };
        let state = AppState::new(8, client, converter, None);

        let app = Router::new()
            .route("/abs/:id", get(super::paper))
            .with_state(state);

        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/abs/{}", id))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn invalid_id_400() {
        let client = MockArxivClient::new(Ok(true), Err(ArxivError::PdfOnly), Ok(Metadata { title: String::new(), summary: String::new() }));
        let converter = MockConverter { result: Ok("".into()) };
        let state = AppState::new(8, client, converter, None);

        let app = Router::new()
            .route("/abs/:id", get(super::paper))
            .with_state(state);

        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/abs/%F0%9F%92%A9")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn index_returns_markdown_when_requested() {
        let app = Router::new().route("/", get(super::index));
        let req = axum::http::Request::builder()
            .uri("/")
            .header(axum::http::header::ACCEPT, "text/markdown")
            .body(axum::body::Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let ct = res.headers().get(axum::http::header::CONTENT_TYPE).unwrap();
        assert_eq!(ct, "text/markdown; charset=utf-8");
    }

    #[tokio::test]
    async fn index_defaults_to_html() {
        let app = Router::new().route("/", get(super::index));
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let ct = res.headers().get(axum::http::header::CONTENT_TYPE).unwrap();
        assert_eq!(ct, "text/html; charset=utf-8");
    }
}
