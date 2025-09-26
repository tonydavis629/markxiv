use std::sync::Arc;

use axum::{
    extract::{OriginalUri, Path, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};

use crate::{
    arxiv::{ArxivClient, ArxivError, Metadata},
    cache::MkCache,
    convert::{ConvertError, Converter},
    disk_cache::DiskCache,
};
use tokio::sync::Mutex;

pub async fn index(headers: HeaderMap) -> Response {
    let path = std::env::var("MARKXIV_INDEX_MD").unwrap_or_else(|_| "content/index.md".to_string());
    match tokio::fs::read_to_string(&path).await {
        Ok(md) => {
            let wants_html = wants_html(
                headers
                    .get(axum::http::header::ACCEPT)
                    .and_then(|v| v.to_str().ok()),
            );
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
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/markdown; charset=utf-8",
                    )],
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
    let mut html_output =
        String::from("<!doctype html><meta charset=\"utf-8\"><title>markxiv</title><body>");
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
    Path(raw_id): Path<String>,
    original_uri: OriginalUri,
    raw_query: Option<RawQuery>,
) -> Response {
    let trimmed = raw_id.trim();
    let normalized = normalize_id(trimmed);

    let original_path = original_uri.path().to_string();
    let canonical_path = format!("/abs/{}", normalized);

    // Minimal id validation: non-empty and ascii
    if normalized.is_empty() || !normalized.is_ascii() {
        return (StatusCode::BAD_REQUEST, "invalid id").into_response();
    }

    let id = normalized.to_string();
    let cache_key = canonical_path.clone();

    let refresh = raw_query
        .and_then(|q| q.0)
        .unwrap_or_default()
        .split('&')
        .find_map(|kv| {
            let mut it = kv.splitn(2, '=');
            let k = it.next()?;
            let v = it.next().unwrap_or("");
            if k == "refresh" && v == "1" {
                Some(())
            } else {
                None
            }
        })
        .is_some();

    if !refresh {
        if let Some(md) = cache.lock().await.get(&cache_key) {
            return markdown_response(md, &original_path);
        }
        if let Some(dc) = &disk {
            match dc.get(&cache_key).await {
                Ok(Some(md)) => {
                    cache.lock().await.put(cache_key.clone(), md.clone());
                    return markdown_response(md, &original_path);
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

    let (body_md, skip_metadata) = match client.get_source_archive(&id).await {
        Ok(bytes) => match converter.latex_tar_to_markdown(&bytes).await {
            Ok(s) => (s, false),
            Err(err) => {
                eprintln!("pandoc conversion failed for {id}: {err}");
                match pdf_fallback(client.as_ref(), converter.as_ref(), &id).await {
                    Ok(s) => (s, true),
                    Err(resp) => return resp,
                }
            }
        },
        Err(ArxivError::PdfOnly) => {
            match pdf_fallback(client.as_ref(), converter.as_ref(), &id).await {
                Ok(s) => (s, true),
                Err(resp) => return resp,
            }
        }
        Err(e) => return map_arxiv_err(e),
    };

    let final_md = if skip_metadata {
        body_md
    } else if let Some(meta) = metadata {
        prepend_metadata(&meta, &body_md)
    } else {
        body_md
    };

    cache.lock().await.put(cache_key.clone(), final_md.clone());
    if let Some(dc) = &disk {
        if let Err(e) = dc.put(&cache_key, &final_md).await {
            eprintln!("disk cache write error: {}", e);
        }
    }
    markdown_response(final_md, &original_path)
}

fn normalize_id(raw: &str) -> &str {
    if raw.len() >= 4 {
        let cut = raw.len() - 4;
        if raw.is_char_boundary(cut) && raw[cut..].eq_ignore_ascii_case(".pdf") {
            return &raw[..cut];
        }
    }
    raw
}

fn markdown_response(md: String, content_location: &str) -> Response {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/markdown; charset=utf-8"),
    );
    if let Ok(val) = axum::http::HeaderValue::from_str(content_location) {
        headers.insert(axum::http::header::CONTENT_LOCATION, val);
    }
    (StatusCode::OK, headers, md).into_response()
}

fn map_arxiv_err(e: ArxivError) -> Response {
    match e {
        ArxivError::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
        ArxivError::PdfOnly => {
            (StatusCode::UNPROCESSABLE_ENTITY, "Error: PDF only").into_response()
        }
        ArxivError::Network(msg) => (StatusCode::BAD_GATEWAY, msg).into_response(),
        ArxivError::NotImplemented => {
            (StatusCode::NOT_IMPLEMENTED, "not implemented").into_response()
        }
    }
}

fn map_convert_err(e: ConvertError) -> Response {
    match e {
        ConvertError::Failed(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response(),
        ConvertError::NotImplemented => {
            (StatusCode::NOT_IMPLEMENTED, "not implemented").into_response()
        }
    }
}

async fn pdf_fallback(
    client: &(dyn ArxivClient + Send + Sync),
    converter: &(dyn Converter + Send + Sync),
    id: &str,
) -> Result<String, Response> {
    let pdf_bytes = match client.get_pdf(id).await {
        Ok(b) => b,
        Err(e) => return Err(map_arxiv_err(e)),
    };
    match converter.pdf_to_markdown(&pdf_bytes).await {
        Ok(s) => Ok(s),
        Err(e) => Err(map_convert_err(e)),
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
    let authors: Vec<String> = meta
        .authors
        .iter()
        .map(|a| strip_html_tags(a).trim().to_string())
        .filter(|a| !a.is_empty())
        .collect();
    let mut out = String::new();
    if !title.is_empty() {
        out.push_str("# ");
        out.push_str(&title);
        out.push_str("\n\n");
    }
    if !authors.is_empty() {
        out.push_str("## Authors\n");
        out.push_str(&authors.join(", "));
        out.push_str("\n\n");
    }
    if !abstract_text.is_empty() {
        out.push_str("## Abstract\n");
        out.push_str(&abstract_text);
        out.push_str("\n\n");
    }
    out.push_str(body_md);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, routing::get, Router};
    use bytes::Bytes;
    use std::sync::atomic::Ordering;
    use tower::ServiceExt; // for `oneshot`

    use crate::arxiv::test_helpers::MockArxivClient;
    use crate::convert::test_helpers::MockConverter;
    use crate::state::AppState;

    #[test]
    fn prepend_metadata_includes_authors_section() {
        let meta = Metadata {
            title: "Sample Title".into(),
            summary: "Sample abstract".into(),
            authors: vec!["Alice Example".into(), "Bob <i>Author</i>".into()],
        };
        let out = super::prepend_metadata(&meta, "Body");
        assert!(out.starts_with("# Sample Title\n\n## Authors\nAlice Example, Bob Author\n\n## Abstract\nSample abstract\n\nBody"));
    }

    #[tokio::test]
    async fn health_ok() {
        let app = Router::new().route("/health", get(super::health));
        let res = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn paper_happy_path_with_cache() {
        let id = "1234.5678";
        let tar = Bytes::from_static(b"tar-bytes");
        let md = "# Hello".to_string();

        let meta = Metadata {
            title: "Sample Title".into(),
            summary: "Sample abstract".into(),
            authors: vec!["First Author".into(), "Second Author".into()],
        };
        let client =
            MockArxivClient::new(Ok(true), Ok(tar), Err(ArxivError::NotImplemented), Ok(meta));
        let converter = MockConverter::new(Ok(md.clone()), Ok(md.clone()));
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
        let cl = res
            .headers()
            .get(axum::http::header::CONTENT_LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(cl, format!("/abs/{}", id));

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
        let cl2 = res2
            .headers()
            .get(axum::http::header::CONTENT_LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(cl2, format!("/abs/{}", id));
    }

    #[tokio::test]
    async fn pdf_route_accepts_pdf_suffix_and_hits_cache() {
        let id = "1234.5678v3";
        let tar = Bytes::from_static(b"tar-bytes");
        let md = "# Hello".to_string();

        let meta = Metadata {
            title: "Sample Title".into(),
            summary: "Sample abstract".into(),
            authors: Vec::new(),
        };
        let client =
            MockArxivClient::new(Ok(true), Ok(tar), Err(ArxivError::NotImplemented), Ok(meta));
        let metadata_calls = client.metadata_calls.clone();
        let archive_calls = client.archive_calls.clone();
        let converter = MockConverter::new(Ok(md.clone()), Ok(md.clone()));
        let state = AppState::new(8, client, converter, None);

        let app = Router::new()
            .route("/abs/:id", get(super::paper))
            .route("/pdf/:id", get(super::paper))
            .with_state(state);

        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/pdf/{}.pdf", id))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let cl = res
            .headers()
            .get(axum::http::header::CONTENT_LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(cl, format!("/pdf/{}.pdf", id));
        assert_eq!(metadata_calls.load(Ordering::SeqCst), 1);
        assert_eq!(archive_calls.load(Ordering::SeqCst), 1);

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
        let cl2 = res2
            .headers()
            .get(axum::http::header::CONTENT_LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(cl2, format!("/abs/{}", id));
        assert_eq!(metadata_calls.load(Ordering::SeqCst), 1);
        assert_eq!(archive_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pdf_route_without_suffix_shares_cache_and_sets_header() {
        let id = "1234.5678";
        let tar = Bytes::from_static(b"tar-bytes");
        let md = "# Hello".to_string();

        let meta = Metadata {
            title: "Sample Title".into(),
            summary: "Sample abstract".into(),
            authors: Vec::new(),
        };
        let client =
            MockArxivClient::new(Ok(true), Ok(tar), Err(ArxivError::NotImplemented), Ok(meta));
        let metadata_calls = client.metadata_calls.clone();
        let archive_calls = client.archive_calls.clone();
        let converter = MockConverter::new(Ok(md.clone()), Ok(md.clone()));
        let state = AppState::new(8, client, converter, None);

        let app = Router::new()
            .route("/abs/:id", get(super::paper))
            .route("/pdf/:id", get(super::paper))
            .with_state(state);

        let res = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/pdf/{}", id))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let cl = res
            .headers()
            .get(axum::http::header::CONTENT_LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(cl, format!("/pdf/{}", id));
        assert_eq!(metadata_calls.load(Ordering::SeqCst), 1);
        assert_eq!(archive_calls.load(Ordering::SeqCst), 1);

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
        let cl2 = res2
            .headers()
            .get(axum::http::header::CONTENT_LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(cl2, format!("/abs/{}", id));
        assert_eq!(metadata_calls.load(Ordering::SeqCst), 1);
        assert_eq!(archive_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pdf_only_falls_back_to_pdftotext() {
        let id = "1234.5678";
        let client = MockArxivClient::new(
            Ok(true),
            Err(ArxivError::PdfOnly),
            Ok(Bytes::from_static(b"pdf-bytes")),
            Ok(Metadata {
                title: "Sample Title".into(),
                summary: "Sample abstract".into(),
                authors: vec!["Author One".into()],
            }),
        );
        let pdf_calls = client.pdf_calls.clone();
        let archive_calls = client.archive_calls.clone();
        let converter = MockConverter::new(Ok(String::new()), Ok("pdf text".into()));
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
        let status = res.status();
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_ref(), b"pdf text");
        assert_eq!(archive_calls.load(Ordering::SeqCst), 1);
        assert_eq!(pdf_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pandoc_failure_falls_back_to_pdftotext() {
        let id = "1234.5678";
        let tar = Bytes::from_static(b"tar-bytes");
        let client = MockArxivClient::new(
            Ok(true),
            Ok(tar),
            Ok(Bytes::from_static(b"pdf-bytes")),
            Ok(Metadata {
                title: "Sample Title".into(),
                summary: "Sample abstract".into(),
                authors: vec!["Author One".into()],
            }),
        );
        let pdf_calls = client.pdf_calls.clone();
        let archive_calls = client.archive_calls.clone();
        let converter = MockConverter::new(
            Err(ConvertError::Failed("pandoc failed".into())),
            Ok("pdf text".into()),
        );
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
        let status = res.status();
        let body = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_ref(), b"pdf text");
        assert_eq!(archive_calls.load(Ordering::SeqCst), 1);
        assert_eq!(pdf_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalid_id_400() {
        let client = MockArxivClient::new(
            Ok(true),
            Err(ArxivError::PdfOnly),
            Err(ArxivError::NotImplemented),
            Ok(Metadata {
                title: String::new(),
                summary: String::new(),
                authors: Vec::new(),
            }),
        );
        let converter = MockConverter::new(Ok(String::new()), Ok(String::new()));
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
