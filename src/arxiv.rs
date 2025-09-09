use async_trait::async_trait;
use bytes::Bytes;
use thiserror::Error;
use reqwest::Url;

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
        let url = Url::parse_with_params(
            "https://export.arxiv.org/api/query",
            &[("id_list", id)],
        )
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
            .header(reqwest::header::ACCEPT, "application/x-eprint-tar, application/x-tar, application/octet-stream")
            .send()
            .await
            .map_err(|e| ArxivError::Network(e.to_string()))?;

        let status = res.status();
        if status.is_success() {
            let bytes = res
                .bytes()
                .await
                .map_err(|e| ArxivError::Network(e.to_string()))?;
            return Ok(bytes);
        }
        // Common cases: 400/403/404 when no source available → treat as PDF only
        if status.as_u16() == 400 || status.as_u16() == 403 || status.as_u16() == 404 {
            return Err(ArxivError::PdfOnly);
        }
        Err(ArxivError::Network(format!("arXiv e-print HTTP {}", status)))
    }

    async fn get_metadata(&self, id: &str) -> Result<Metadata, ArxivError> {
        let url = Url::parse_with_params(
            "https://export.arxiv.org/api/query",
            &[("id_list", id)],
        )
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
}

fn parse_atom_metadata(atom: &str) -> Option<Metadata> {
    // A very small and forgiving parser to avoid XML deps: look for first <entry>...</entry>
    let entry_start = atom.find("<entry")?;
    let entry_end_rel = atom[entry_start..].find("</entry>")?;
    let entry = &atom[entry_start..entry_start + entry_end_rel + "</entry>".len()];
    let title = extract_tag(entry, "title")?.trim().to_string();
    let summary = extract_tag(entry, "summary").unwrap_or_default().trim().to_string();
    Some(Metadata { title, summary })
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

#[cfg(test)]
pub mod test_helpers {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    pub struct MockArxivClient {
        pub exists_response: Result<bool, ArxivError>,
        pub archive_response: Result<Bytes, ArxivError>,
        pub metadata_response: Result<Metadata, ArxivError>,
        pub exists_calls: Arc<AtomicUsize>,
        pub archive_calls: Arc<AtomicUsize>,
        pub metadata_calls: Arc<AtomicUsize>,
    }

    impl MockArxivClient {
        pub fn new(
            exists_response: Result<bool, ArxivError>,
            archive_response: Result<Bytes, ArxivError>,
            metadata_response: Result<Metadata, ArxivError>,
        ) -> Self {
            Self {
                exists_response,
                archive_response,
                metadata_response,
                exists_calls: Arc::new(AtomicUsize::new(0)),
                archive_calls: Arc::new(AtomicUsize::new(0)),
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

        async fn get_metadata(&self, _id: &str) -> Result<Metadata, ArxivError> {
            self.metadata_calls.fetch_add(1, Ordering::SeqCst);
            self.metadata_response.clone()
        }
    }
}
