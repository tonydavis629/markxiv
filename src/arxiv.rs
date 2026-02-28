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
    async fn search(
        &self,
        _query: &str,
        _max_results: u32,
    ) -> Result<Vec<SearchResult>, ArxivError> {
        Err(ArxivError::NotImplemented)
    }

    async fn get_html_figure_image_urls(&self, _id: &str) -> Result<Vec<String>, ArxivError> {
        Ok(vec![])
    }
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

    async fn search(
        &self,
        query: &str,
        max_results: u32,
    ) -> Result<Vec<SearchResult>, ArxivError> {
        let max_results = max_results.min(50);
        let url = Url::parse_with_params(
            "https://export.arxiv.org/api/query",
            &[
                ("search_query", format!("all:{}", query).as_str()),
                ("start", "0"),
                ("max_results", &max_results.to_string()),
            ],
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
                "arXiv search HTTP {}",
                res.status()
            )));
        }

        let body = res
            .text()
            .await
            .map_err(|e| ArxivError::Network(e.to_string()))?;

        Ok(parse_atom_search_results(&body))
    }

    async fn get_html_figure_image_urls(&self, id: &str) -> Result<Vec<String>, ArxivError> {
        let base_url = format!("https://arxiv.org/html/{}", id);
        let res = self
            .http
            .get(&base_url)
            .send()
            .await
            .map_err(|e| ArxivError::Network(e.to_string()))?;
        if !res.status().is_success() {
            return Ok(vec![]);
        }
        let html = res
            .text()
            .await
            .map_err(|e| ArxivError::Network(e.to_string()))?;
        Ok(parse_html_figure_image_urls(&html, &base_url))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub title: String,
    pub summary: String,
    pub authors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub authors: Vec<String>,
    pub published: String,
}

fn parse_atom_search_results(atom: &str) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut remainder = atom;
    while let Some(start) = remainder.find("<entry") {
        let entry_section = &remainder[start..];
        let Some(end_rel) = entry_section.find("</entry>") else {
            break;
        };
        let end = start + end_rel + "</entry>".len();
        let entry = &remainder[start..end];

        let title = extract_tag(entry, "title")
            .unwrap_or_default()
            .trim()
            .to_string();
        let summary = extract_tag(entry, "summary")
            .unwrap_or_default()
            .trim()
            .to_string();
        let authors = extract_authors(entry);
        let published = extract_tag(entry, "published")
            .unwrap_or_default()
            .trim()
            .to_string();
        let id = extract_tag(entry, "id")
            .unwrap_or_default()
            .trim()
            .to_string();
        // Extract arXiv ID from full URL (e.g. "http://arxiv.org/abs/1706.03762v5" → "1706.03762v5")
        let arxiv_id = id
            .rsplit('/')
            .next()
            .unwrap_or(&id)
            .to_string();

        if !title.is_empty() {
            results.push(SearchResult {
                id: arxiv_id,
                title,
                summary,
                authors,
                published,
            });
        }
        remainder = &remainder[end..];
    }
    results
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

fn parse_html_figure_image_urls(html: &str, base_url: &str) -> Vec<String> {
    let base_url = base_url.trim_end_matches('/');
    let mut urls = Vec::new();
    let mut search_from = 0;

    while let Some(rel_start) = html[search_from..].find("<figure") {
        let abs_start = search_from + rel_start;
        let section = &html[abs_start..];
        let Some(fig_end_rel) = section.find("</figure>") else {
            break;
        };
        let fig_block = &section[..fig_end_rel];
        search_from = abs_start + fig_end_rel + "</figure>".len();

        if let Some(src) = extract_img_src(fig_block) {
            let full_url = if src.starts_with("http://") || src.starts_with("https://") {
                src.to_string()
            } else {
                format!("{}/{}", base_url, src.trim_start_matches('/'))
            };
            urls.push(full_url);
        }
    }

    urls
}

fn extract_img_src(block: &str) -> Option<&str> {
    let img_start = block.find("<img")?;
    let img_tag = &block[img_start..];
    let tag_end = img_tag.find('>')?;
    let tag = &img_tag[..tag_end];

    if let Some(pos) = tag.find("src=\"") {
        let after = &tag[pos + 5..];
        let end = after.find('"')?;
        Some(&after[..end])
    } else if let Some(pos) = tag.find("src='") {
        let after = &tag[pos + 5..];
        let end = after.find('\'')?;
        Some(&after[..end])
    } else {
        None
    }
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

    #[test]
    fn parse_html_figure_images_extracts_img_srcs() {
        let html = r#"
            <figure id="S1.F1">
                <img src="extracted/Figures/fig1.png" alt="Figure 1"/>
                <figcaption>Architecture</figcaption>
            </figure>
            <p>Some text</p>
            <figure id="S1.F2">
                <img src="extracted/Figures/fig2.png" alt="Figure 2"/>
                <figcaption>Results</figcaption>
            </figure>
        "#;
        let urls = parse_html_figure_image_urls(html, "https://arxiv.org/html/1706.03762v7");
        assert_eq!(urls.len(), 2);
        assert_eq!(
            urls[0],
            "https://arxiv.org/html/1706.03762v7/extracted/Figures/fig1.png"
        );
        assert_eq!(
            urls[1],
            "https://arxiv.org/html/1706.03762v7/extracted/Figures/fig2.png"
        );
    }

    #[test]
    fn parse_html_figure_images_handles_absolute_urls() {
        let html = r#"<figure><img src="https://cdn.example.com/img.png"/></figure>"#;
        let urls = parse_html_figure_image_urls(html, "https://arxiv.org/html/1234");
        assert_eq!(urls, vec!["https://cdn.example.com/img.png"]);
    }

    #[test]
    fn parse_html_figure_images_empty_when_no_figures() {
        let html = "<html><body><p>No figures here</p></body></html>";
        let urls = parse_html_figure_image_urls(html, "https://arxiv.org/html/1234");
        assert!(urls.is_empty());
    }

    #[test]
    fn parse_html_figure_images_skips_figures_without_img() {
        let html = r#"
            <figure><figcaption>Caption only</figcaption></figure>
            <figure><img src="real.png"/></figure>
        "#;
        let urls = parse_html_figure_image_urls(html, "https://arxiv.org/html/1234");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0], "https://arxiv.org/html/1234/real.png");
    }

    #[test]
    fn extract_img_src_handles_single_quotes() {
        let block = "<figure><img src='image.png' alt='test'/></figure>";
        assert_eq!(extract_img_src(block), Some("image.png"));
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
        pub search_response: Result<Vec<SearchResult>, ArxivError>,
        pub html_figure_urls_response: Result<Vec<String>, ArxivError>,
        pub exists_calls: Arc<AtomicUsize>,
        pub archive_calls: Arc<AtomicUsize>,
        pub pdf_calls: Arc<AtomicUsize>,
        pub metadata_calls: Arc<AtomicUsize>,
        pub search_calls: Arc<AtomicUsize>,
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
                search_response: Ok(Vec::new()),
                html_figure_urls_response: Ok(Vec::new()),
                exists_calls: Arc::new(AtomicUsize::new(0)),
                archive_calls: Arc::new(AtomicUsize::new(0)),
                pdf_calls: Arc::new(AtomicUsize::new(0)),
                metadata_calls: Arc::new(AtomicUsize::new(0)),
                search_calls: Arc::new(AtomicUsize::new(0)),
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

        async fn search(
            &self,
            _query: &str,
            _max_results: u32,
        ) -> Result<Vec<SearchResult>, ArxivError> {
            self.search_calls.fetch_add(1, Ordering::SeqCst);
            self.search_response.clone()
        }

        async fn get_html_figure_image_urls(&self, _id: &str) -> Result<Vec<String>, ArxivError> {
            self.html_figure_urls_response.clone()
        }
    }
}
