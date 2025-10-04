use async_trait::async_trait;
use bytes::Bytes;
use reqwest::Url;
use thiserror::Error;

#[derive(Clone, Debug, Error)]
pub enum ArxivError {
    #[error("not found")]
    NotFound,
    #[error("pdf only")]
    PdfOnly,
    #[error("network error: {0}")]
    Network(String),
    #[error("not implemented")]
    NotImplemented,
}

#[async_trait]
pub trait ArxivClient {
    async fn exists(&self, id: &str) -> Result<bool, ArxivError>;
    async fn get_source_archive(&self, id: &str) -> Result<Bytes, ArxivError>;
    async fn get_pdf(&self, id: &str) -> Result<Bytes, ArxivError>;
    async fn get_metadata(&self, id: &str) -> Result<Metadata, ArxivError>;
}

pub struct ReqwestArxivClient {
    http: reqwest::Client,
}

impl ReqwestArxivClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent("markxiv/0.1 (+https://github.com/)")
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("failed to build reqwest client");
        Self { http }
    }
}

#[async_trait]
impl ArxivClient for ReqwestArxivClient {
    async fn exists(&self, id: &str) -> Result<bool, ArxivError> {
        let url = Url::parse_with_params("https://export.arxiv.org/api/query", &[("id_list", id)])
            .map_err(|e| ArxivError::Network(e.to_string()))?;
        let res = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/atom+xml")
            .send()
            .await
            .map_err(|e| ArxivError::Network(e.to_string()))?;
        if !res.status().is_success() {
            return Err(ArxivError::Network(format!(
                "arXiv exists check HTTP {}",
                res.status()
            )));
        }
        let body = res
            .text()
            .await
            .map_err(|e| ArxivError::Network(e.to_string()))?;
        // Minimal parse: an empty feed has no <entry>; existing id yields at least one <entry>
        Ok(body.contains("<entry"))
    }

    async fn get_source_archive(&self, id: &str) -> Result<Bytes, ArxivError> {
        let url = format!("https://arxiv.org/e-print/{}", id);
        let res = self
            .http
            .get(url)
            .header(
                reqwest::header::ACCEPT,
                "application/x-eprint-tar, application/x-tar, application/octet-stream",
            )
            .send()
            .await
            .map_err(|e| ArxivError::Network(e.to_string()))?;

        let status = res.status();
        if status.is_success() {
            // Inspect content-type and payload to avoid passing non-archives downstream
            let content_type = res
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_ascii_lowercase();

            let bytes = res
                .bytes()
                .await
                .map_err(|e| ArxivError::Network(e.to_string()))?;

            // If arXiv returns a PDF (no source available), map to PdfOnly
            if content_type.contains("application/pdf") || looks_like_pdf(&bytes) {
                return Err(ArxivError::PdfOnly);
            }
            // Some upstream issues (e.g., HTML error pages) can slip through as 200s
            if content_type.contains("text/html") || looks_like_html(&bytes) {
                return Err(ArxivError::Network(
                    "arXiv returned HTML when requesting e-print".into(),
                ));
            }
            // Otherwise, assume it's an archive (tar or tar.gz). The converter will try both.
            return Ok(bytes);
        }
        // Common cases: 400/403/404 when no source available → treat as PDF only
        if status.as_u16() == 400 || status.as_u16() == 403 || status.as_u16() == 404 {
            return Err(ArxivError::PdfOnly);
        }
        Err(ArxivError::Network(format!(
            "arXiv e-print HTTP {}",
            status
        )))
    }

    async fn get_pdf(&self, id: &str) -> Result<Bytes, ArxivError> {
        let url = format!("https://arxiv.org/pdf/{}.pdf", id);
        let res = self
            .http
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/pdf")
            .send()
            .await
            .map_err(|e| ArxivError::Network(e.to_string()))?;

        let status = res.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ArxivError::NotFound);
        }
        if !status.is_success() {
            return Err(ArxivError::Network(format!("arXiv pdf HTTP {}", status)));
        }

        let bytes = res
            .bytes()
            .await
            .map_err(|e| ArxivError::Network(e.to_string()))?;

        if looks_like_pdf(&bytes) {
            Ok(bytes)
        } else {
            Err(ArxivError::Network(
                "unexpected non-PDF payload when requesting PDF".into(),
            ))
        }
    }

    async fn get_metadata(&self, id: &str) -> Result<Metadata, ArxivError> {
        let url = Url::parse_with_params("https://export.arxiv.org/api/query", &[("id_list", id)])
            .map_err(|e| ArxivError::Network(e.to_string()))?;
        let res = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/atom+xml")
            .send()
            .await
            .map_err(|e| ArxivError::Network(e.to_string()))?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(ArxivError::NotFound);
        }
        if !res.status().is_success() {
            return Err(ArxivError::Network(format!(
                "arXiv metadata HTTP {}",
                res.status()
            )));
        }
        let body = res
            .text()
            .await
            .map_err(|e| ArxivError::Network(e.to_string()))?;
        parse_atom_metadata(&body).ok_or(ArxivError::NotFound)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub title: String,
    pub summary: String,
    pub authors: Vec<String>,
}

fn parse_atom_metadata(atom: &str) -> Option<Metadata> {
    // A very small and forgiving parser to avoid XML deps: look for first <entry>...</entry>
    let entry_start = atom.find("<entry")?;
    let entry_end_rel = atom[entry_start..].find("</entry>")?;
    let entry = &atom[entry_start..entry_start + entry_end_rel + "</entry>".len()];
    let title = extract_tag(entry, "title")?.trim().to_string();
    let summary = extract_tag(entry, "summary")
        .unwrap_or_default()
        .trim()
        .to_string();
    let authors = extract_authors(entry);
    Some(Metadata {
        title,
        summary,
        authors,
    })
}

fn extract_tag(s: &str, tag: &str) -> Option<String> {
    // Handles optional attributes on the opening tag
    let open = format!("<{}", tag);
    let start = s.find(&open)?;
    let after_open = &s[start..];
    let end_open_rel = after_open.find('>')?;
    let after = &after_open[end_open_rel + 1..];
    let close = format!("</{}>", tag);
    let end_rel = after.find(&close)?;
    Some(after[..end_rel].to_string())
}

fn extract_authors(entry: &str) -> Vec<String> {
    let mut authors = Vec::new();
    let mut remainder = entry;
    while let Some(start) = remainder.find("<author") {
        let author_section = &remainder[start..];
        let Some(end_rel) = author_section.find("</author>") else {
            break;
        };
        let end = start + end_rel + "</author>".len();
        let block = &remainder[start..end];
        if let Some(name) = extract_tag(block, "name") {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                authors.push(trimmed.to_string());
            }
        }
        remainder = &remainder[end..];
    }
    authors
}

// Heuristics to detect unexpected payloads from the e-print endpoint
fn looks_like_pdf(bytes: &[u8]) -> bool {
    bytes.len() >= 5 && &bytes[..5] == b"%PDF-"
}

fn looks_like_html(bytes: &[u8]) -> bool {
    // Look at the first 1024 bytes, trim leading whitespace, then match common HTML signatures
    let n = bytes.len().min(1024);
    let mut i = 0;
    while i < n && matches!(bytes[i], b'\t' | b'\n' | b'\r' | b' ') {
        i += 1;
    }
    if i >= n {
        return false;
    }
    if bytes[i] != b'<' {
        return false;
    }
    let s = String::from_utf8_lossy(&bytes[i..n]).to_ascii_lowercase();
    s.starts_with("<!doctype html") || s.starts_with("<html")
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[test]
    fn parse_atom_metadata_extracts_fields() {
        let atom = r#"<?xml version='1.0'?>
            <feed>
              <entry>
                <title>Sample &lt;b&gt;Title&lt;/b&gt;</title>
                <summary> Summary text </summary>
                <author><name>Alice</name></author>
                <author><name> Bob </name></author>
              </entry>
            </feed>"#;
        let meta = parse_atom_metadata(atom).expect("metadata");
        assert_eq!(meta.title, "Sample &lt;b&gt;Title&lt;/b&gt;");
        assert_eq!(meta.summary, "Summary text");
        assert_eq!(meta.authors, vec!["Alice".to_string(), "Bob".to_string()]);
    }

    #[test]
    fn looks_like_pdf_recognizes_signature() {
        assert!(looks_like_pdf(b"%PDF-1.7 rest"));
        assert!(!looks_like_pdf(b"%!PS-Adobe"));
    }

    #[test]
    fn looks_like_html_handles_whitespace() {
        let html = b"\n  \t<html><body></body></html>";
        assert!(looks_like_html(html));
        assert!(!looks_like_html(b"{\"json\":true}"));
    }
}

pub mod test_helpers {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    pub struct MockArxivClient {
        pub exists_response: Result<bool, ArxivError>,
        pub archive_response: Result<Bytes, ArxivError>,
        pub pdf_response: Result<Bytes, ArxivError>,
        pub metadata_response: Result<Metadata, ArxivError>,
        pub exists_calls: Arc<AtomicUsize>,
        pub archive_calls: Arc<AtomicUsize>,
        pub pdf_calls: Arc<AtomicUsize>,
        pub metadata_calls: Arc<AtomicUsize>,
    }

    impl MockArxivClient {
        pub fn new(
            exists_response: Result<bool, ArxivError>,
            archive_response: Result<Bytes, ArxivError>,
            pdf_response: Result<Bytes, ArxivError>,
            metadata_response: Result<Metadata, ArxivError>,
        ) -> Self {
            Self {
                exists_response,
                archive_response,
                pdf_response,
                metadata_response,
                exists_calls: Arc::new(AtomicUsize::new(0)),
                archive_calls: Arc::new(AtomicUsize::new(0)),
                pdf_calls: Arc::new(AtomicUsize::new(0)),
                metadata_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl ArxivClient for MockArxivClient {
        async fn exists(&self, _id: &str) -> Result<bool, ArxivError> {
            self.exists_calls.fetch_add(1, Ordering::SeqCst);
            self.exists_response.clone()
        }

        async fn get_source_archive(&self, _id: &str) -> Result<Bytes, ArxivError> {
            self.archive_calls.fetch_add(1, Ordering::SeqCst);
            self.archive_response.clone()
        }

        async fn get_pdf(&self, _id: &str) -> Result<Bytes, ArxivError> {
            self.pdf_calls.fetch_add(1, Ordering::SeqCst);
            self.pdf_response.clone()
        }

        async fn get_metadata(&self, _id: &str) -> Result<Metadata, ArxivError> {
            self.metadata_calls.fetch_add(1, Ordering::SeqCst);
            self.metadata_response.clone()
        }
    }
}
