//! Exercises `ReqwestArxivClient` itself against a local HTTP server.
//!
//! The retry loop is unit-tested in `src/arxiv.rs`, but everything between the
//! socket and that loop — reqwest error classification, per-endpoint status
//! mapping, `Retry-After` parsing — only exists on the real client, behind the
//! `ArxivClient` trait where `MockArxivClient` cannot reach it.

use markxiv::arxiv::{ArxivClient, ArxivError, NetKind, ReqwestArxivClient};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ATOM_ENTRY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <title>Attention Is All You Need</title>
    <summary>We propose a new architecture.</summary>
    <author><name>Ashish Vaswani</name></author>
  </entry>
</feed>"#;

const ATOM_EMPTY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>ArXiv Query</title>
</feed>"#;

fn client(server: &MockServer) -> ReqwestArxivClient {
    ReqwestArxivClient::with_bases(&server.uri(), &server.uri())
}

fn net_kind(err: &ArxivError) -> Option<NetKind> {
    err.kind()
}

#[tokio::test]
async fn metadata_retries_a_503_then_succeeds() {
    let server = MockServer::start().await;
    // wiremock serves mounted mocks in order of specificity; `up_to_n_times`
    // plus ordering gives us "fail once, then succeed".
    Mock::given(method("GET"))
        .and(path("/api/query"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/query"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ATOM_ENTRY))
        .mount(&server)
        .await;

    let meta = client(&server)
        .get_metadata("1706.03762")
        .await
        .expect("retry should have rescued the 503");
    assert_eq!(meta.title, "Attention Is All You Need");
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn metadata_gives_up_after_two_attempts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/query"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let err = client(&server)
        .get_metadata("1706.03762")
        .await
        .unwrap_err();
    assert_eq!(net_kind(&err), Some(NetKind::Status(503)));
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        2,
        "initial attempt plus exactly one retry"
    );
}

#[tokio::test]
async fn metadata_honours_a_short_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/query"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/query"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ATOM_ENTRY))
        .mount(&server)
        .await;

    let meta = client(&server).get_metadata("1706.03762").await;
    assert!(meta.is_ok());
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn metadata_refuses_to_wait_out_a_long_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/query"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "600"))
        .mount(&server)
        .await;

    let err = client(&server)
        .get_metadata("1706.03762")
        .await
        .unwrap_err();
    assert_eq!(net_kind(&err), Some(NetKind::Status(429)));
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "an honest 600 s should fail the request, not stall it"
    );
}

#[tokio::test]
async fn empty_feed_is_a_404_but_a_broken_body_is_not() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/query"))
        .respond_with(ResponseTemplate::new(200).set_body_string(ATOM_EMPTY))
        .mount(&server)
        .await;
    let err = client(&server)
        .get_metadata("0000.00000")
        .await
        .unwrap_err();
    assert!(matches!(err, ArxivError::NotFound));

    // A 200 carrying an upstream error page must not reach the user as a 404.
    let server2 = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/query"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>502 Bad Gateway</html>"))
        .mount(&server2)
        .await;
    let err = client(&server2)
        .get_metadata("1706.03762")
        .await
        .unwrap_err();
    assert!(
        !matches!(err, ArxivError::NotFound),
        "an unparseable body must not masquerade as a missing paper"
    );
    assert_eq!(net_kind(&err), Some(NetKind::Decode));
}

#[tokio::test]
async fn eprint_404_is_pdf_only_without_a_retry() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/e-print/1706.03762"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let err = client(&server)
        .get_source_archive("1706.03762")
        .await
        .unwrap_err();
    assert!(matches!(err, ArxivError::PdfOnly));
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "a genuine 404 means no source exists; retrying cannot change that"
    );
}

#[tokio::test]
async fn eprint_403_is_retried_before_falling_back_to_pdf() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/e-print/1706.03762"))
        .respond_with(ResponseTemplate::new(403))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/e-print/1706.03762"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"fake-tar-bytes".to_vec()))
        .mount(&server)
        .await;

    let bytes = client(&server)
        .get_source_archive("1706.03762")
        .await
        .expect("a throttling 403 should be retried, not read as 'no source'");
    assert_eq!(bytes.as_ref(), b"fake-tar-bytes");
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_persistent_403_still_degrades_to_pdf() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/e-print/1706.03762"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;

    let err = client(&server)
        .get_source_archive("1706.03762")
        .await
        .unwrap_err();
    assert!(
        matches!(err, ArxivError::PdfOnly),
        "after the retry fails, keep the historical PDF fallback"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn eprint_serving_a_pdf_means_pdf_only() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/e-print/1706.03762"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"%PDF-1.7 body".to_vec()))
        .mount(&server)
        .await;

    let err = client(&server)
        .get_source_archive("1706.03762")
        .await
        .unwrap_err();
    assert!(matches!(err, ArxivError::PdfOnly));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn pdf_404_is_not_found_and_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/pdf/1706.03762.pdf"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let err = client(&server).get_pdf("1706.03762").await.unwrap_err();
    assert!(matches!(err, ArxivError::NotFound));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn pdf_5xx_is_retried() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/pdf/1706.03762.pdf"))
        .respond_with(ResponseTemplate::new(502))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/pdf/1706.03762.pdf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"%PDF-1.7 body".to_vec()))
        .mount(&server)
        .await;

    let bytes = client(&server).get_pdf("1706.03762").await.expect("pdf");
    assert!(bytes.starts_with(b"%PDF-"));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn figure_lookup_is_best_effort_and_never_retried() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/html/1706.03762"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let urls = client(&server)
        .get_html_figure_image_urls("1706.03762")
        .await
        .expect("a failed figure lookup must not fail the request");
    assert!(urls.is_empty());
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "already best-effort; a retry would add a round-trip to every cache miss"
    );
}
