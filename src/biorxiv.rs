use async_trait::async_trait;
use bytes::Bytes;
use reqwest::Url;

use crate::arxiv::{Metadata, PaperSource, PaperSourceError};

const BIORXIV_HOST_SUFFIX: &str = "biorxiv.org";
const BIORXIV_CONTENT_PREFIX: &str = "content/";

pub struct ReqwestBiorxivClient {
    http: reqwest::Client,
}

impl ReqwestBiorxivClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent("markxiv/0.1 (+https://github.com/)")
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("failed to build reqwest client");
        Self { http }
    }

    fn article_url(id: &str) -> String {
        format!("https://www.biorxiv.org/content/{}.full", id)
    }

    fn pdf_url(id: &str) -> String {
        format!("https://www.biorxiv.org/content/{}.full.pdf", id)
    }

    async fn fetch_article_page(&self, id: &str) -> Result<String, PaperSourceError> {
        let res = self
            .http
            .get(Self::article_url(id))
            .send()
            .await
            .map_err(|e| PaperSourceError::Network(e.to_string()))?;

        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(PaperSourceError::NotFound);
        }
        if !res.status().is_success() {
            return Err(PaperSourceError::Network(format!(
                "bioRxiv article HTTP {}",
                res.status()
            )));
        }

        res.text()
            .await
            .map_err(|e| PaperSourceError::Network(e.to_string()))
    }
}

#[async_trait]
impl PaperSource for ReqwestBiorxivClient {
    async fn exists(&self, id: &str) -> Result<bool, PaperSourceError> {
        match self.fetch_article_page(id).await {
            Ok(_) => Ok(true),
            Err(PaperSourceError::NotFound) => Ok(false),
            Err(err) => Err(err),
        }
    }

    async fn get_source_archive(&self, _id: &str) -> Result<Bytes, PaperSourceError> {
        Err(PaperSourceError::PdfOnly)
    }

    async fn get_pdf(&self, id: &str) -> Result<Bytes, PaperSourceError> {
        let res = self
            .http
            .get(Self::pdf_url(id))
            .header(reqwest::header::ACCEPT, "application/pdf")
            .send()
            .await
            .map_err(|e| PaperSourceError::Network(e.to_string()))?;

        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(PaperSourceError::NotFound);
        }
        if !res.status().is_success() {
            return Err(PaperSourceError::Network(format!(
                "bioRxiv pdf HTTP {}",
                res.status()
            )));
        }

        let bytes = res
            .bytes()
            .await
            .map_err(|e| PaperSourceError::Network(e.to_string()))?;

        if bytes.starts_with(b"%PDF-") {
            Ok(bytes)
        } else {
            Err(PaperSourceError::Network(
                "unexpected non-PDF payload when requesting bioRxiv PDF".into(),
            ))
        }
    }

    async fn get_metadata(&self, id: &str) -> Result<Metadata, PaperSourceError> {
        let html = self.fetch_article_page(id).await?;
        parse_biorxiv_metadata(&html).ok_or(PaperSourceError::NotFound)
    }
}

pub fn normalize_biorxiv_id(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    let candidate = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let url = Url::parse(trimmed).ok()?;
        let host = url.host_str()?.to_ascii_lowercase();
        if !host.ends_with(BIORXIV_HOST_SUFFIX) {
            return None;
        }
        url.path().trim_matches('/').to_string()
    } else {
        trimmed.trim_start_matches('/').to_string()
    };

    let candidate = candidate
        .strip_prefix(BIORXIV_CONTENT_PREFIX)
        .unwrap_or(&candidate);
    let candidate = strip_ascii_suffix(candidate, ".full.pdf")
        .or_else(|| strip_ascii_suffix(candidate, ".full"))
        .or_else(|| strip_ascii_suffix(candidate, ".pdf"))
        .unwrap_or(candidate);

    if candidate.is_empty() || !candidate.is_ascii() || !candidate.contains('/') {
        return None;
    }

    Some(candidate.to_string())
}

pub fn canonical_biorxiv_path(id: &str) -> String {
    format!("/bio/{}", id)
}

pub fn biorxiv_article_url(id: &str) -> String {
    ReqwestBiorxivClient::article_url(id)
}

fn parse_biorxiv_metadata(html: &str) -> Option<Metadata> {
    let title = extract_meta_value(html, &["citation_title", "dc.title", "og:title"])?;
    let authors = extract_meta_values(html, &["citation_author"]);
    let summary = extract_meta_value(
        html,
        &[
            "citation_abstract",
            "dc.description",
            "description",
            "og:description",
        ],
    )
    .unwrap_or_default();

    Some(Metadata {
        title,
        summary,
        authors,
    })
}

fn extract_meta_value(html: &str, names: &[&str]) -> Option<String> {
    extract_meta_values(html, names).into_iter().next()
}

fn extract_meta_values(html: &str, names: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut remainder = html;

    while let Some(start) = remainder.find("<meta") {
        let section = &remainder[start..];
        let Some(end_rel) = section.find('>') else {
            break;
        };
        let tag = &section[..=end_rel];
        remainder = &section[end_rel + 1..];

        let attrs = parse_meta_attributes(tag);
        let Some(content) = attrs.get("content") else {
            continue;
        };
        let Some(name) = attrs
            .get("name")
            .or_else(|| attrs.get("property"))
            .map(|v| v.to_ascii_lowercase())
        else {
            continue;
        };

        if names
            .iter()
            .any(|candidate| name == candidate.to_ascii_lowercase())
        {
            let value = html_unescape(content).trim().to_string();
            if !value.is_empty() {
                out.push(value);
            }
        }
    }

    out
}

fn parse_meta_attributes(tag: &str) -> std::collections::HashMap<String, String> {
    let mut attrs = std::collections::HashMap::new();
    let bytes = tag.as_bytes();
    let mut idx = 0;

    while idx < bytes.len() {
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx >= bytes.len() || matches!(bytes[idx], b'<' | b'>' | b'/') {
            idx += 1;
            continue;
        }

        let key_start = idx;
        while idx < bytes.len()
            && (bytes[idx].is_ascii_alphanumeric() || matches!(bytes[idx], b'-' | b'_' | b':'))
        {
            idx += 1;
        }
        if idx == key_start {
            idx += 1;
            continue;
        }
        let key = tag[key_start..idx].to_ascii_lowercase();

        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx >= bytes.len() || bytes[idx] != b'=' {
            continue;
        }
        idx += 1;
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        if idx >= bytes.len() {
            break;
        }

        let quote = bytes[idx];
        if quote != b'"' && quote != b'\'' {
            continue;
        }
        idx += 1;
        let value_start = idx;
        while idx < bytes.len() && bytes[idx] != quote {
            idx += 1;
        }
        if idx <= bytes.len() {
            attrs.insert(key, tag[value_start..idx].to_string());
        }
        idx += 1;
    }

    attrs
}

fn strip_ascii_suffix<'a>(input: &'a str, suffix: &str) -> Option<&'a str> {
    if input.len() < suffix.len() {
        return None;
    }
    let cut = input.len() - suffix.len();
    if input.is_char_boundary(cut) && input[cut..].eq_ignore_ascii_case(suffix) {
        Some(&input[..cut])
    } else {
        None
    }
}

fn html_unescape(input: &str) -> String {
    input
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_biorxiv_bare_id() {
        assert_eq!(
            normalize_biorxiv_id("10.1101/2024.01.02.123456v1").as_deref(),
            Some("10.1101/2024.01.02.123456v1")
        );
    }

    #[test]
    fn normalize_biorxiv_full_suffix() {
        assert_eq!(
            normalize_biorxiv_id("10.1101/2024.01.02.123456v1.full").as_deref(),
            Some("10.1101/2024.01.02.123456v1")
        );
    }

    #[test]
    fn normalize_biorxiv_full_pdf_suffix() {
        assert_eq!(
            normalize_biorxiv_id("10.1101/2024.01.02.123456v1.full.pdf").as_deref(),
            Some("10.1101/2024.01.02.123456v1")
        );
    }

    #[test]
    fn normalize_biorxiv_url() {
        assert_eq!(
            normalize_biorxiv_id(
                "https://www.biorxiv.org/content/10.1101/2024.01.02.123456v1.full.pdf"
            )
            .as_deref(),
            Some("10.1101/2024.01.02.123456v1")
        );
    }

    #[test]
    fn parse_metadata_from_meta_tags() {
        let html = r#"
            <html>
                <head>
                    <meta name="citation_title" content="Sample bioRxiv paper" />
                    <meta name="citation_author" content="Alice Example" />
                    <meta name="citation_author" content="Bob Example" />
                    <meta property="og:description" content="A concise abstract." />
                </head>
            </html>
        "#;

        let meta = parse_biorxiv_metadata(html).expect("metadata");
        assert_eq!(meta.title, "Sample bioRxiv paper");
        assert_eq!(meta.summary, "A concise abstract.");
        assert_eq!(meta.authors, vec!["Alice Example", "Bob Example"]);
    }
}
