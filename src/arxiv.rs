use async_trait::async_trait;
use bytes::Bytes;
use reqwest::Url;
use std::future::Future;
use std::time::Duration;
// tokio's clock, not `std`'s, so the deadline and the backoff sleep share one
// time source and both can be driven deterministically under paused time.
use thiserror::Error;
use tokio::time::Instant;

/// Why an upstream call failed.
///
/// [`ArxivError`] has to stay `Clone` (`MockArxivClient` hands out clones of a
/// stored `Result`) and `reqwest::Error` is not `Clone`, so the error is
/// classified *before* `to_string()` flattens it and only the verdict is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetKind {
    /// No response within the per-attempt budget.
    Timeout,
    /// Never got as far as an HTTP exchange (DNS, TCP, TLS).
    Connect,
    /// A response arrived carrying a non-success status.
    Status(u16),
    /// The body could not be read or was not what the endpoint promised.
    Decode,
    /// A deterministic local failure, or anything unclassified.
    Other,
}

impl NetKind {
    fn classify(e: &reqwest::Error) -> Self {
        if e.is_timeout() {
            NetKind::Timeout
        } else if e.is_connect() {
            NetKind::Connect
        } else if let Some(status) = e.status() {
            NetKind::Status(status.as_u16())
        } else if e.is_decode() || e.is_body() {
            NetKind::Decode
        } else {
            NetKind::Other
        }
    }

    /// Stable token for log queries, so the 429-vs-timeout-vs-slow-success split
    /// is a `grep 'outcome='` away.
    pub fn outcome(&self) -> &'static str {
        match self {
            NetKind::Timeout => "timeout",
            NetKind::Connect => "connect",
            NetKind::Status(429) => "http_429",
            NetKind::Status(s) if *s >= 500 => "http_5xx",
            NetKind::Status(_) => "http_4xx",
            NetKind::Decode => "decode",
            NetKind::Other => "other",
        }
    }

    /// Transient classes worth a second attempt.
    ///
    /// 403 is included because arXiv also serves it while throttling, not only
    /// to mean "no source available".
    fn is_retryable(&self) -> bool {
        match self {
            NetKind::Timeout | NetKind::Connect => true,
            NetKind::Status(429) | NetKind::Status(403) => true,
            NetKind::Status(s) => *s >= 500,
            NetKind::Decode | NetKind::Other => false,
        }
    }
}

#[derive(Clone, Debug, Error)]
pub enum ArxivError {
    #[error("not found")]
    NotFound,
    #[error("pdf only")]
    PdfOnly,
    #[error("network error: {msg}")]
    Network {
        kind: NetKind,
        /// Internal detail — carries upstream URLs, so log it, never serve it.
        msg: String,
        /// Upstream `Retry-After`, when one was sent.
        retry_after: Option<Duration>,
    },
    #[error("not implemented")]
    NotImplemented,
}

impl ArxivError {
    fn from_reqwest(e: reqwest::Error) -> Self {
        ArxivError::Network {
            kind: NetKind::classify(&e),
            msg: e.to_string(),
            retry_after: None,
        }
    }

    /// Builds a network error with an explicit classification, for call sites
    /// (and tests) that have no `reqwest::Error` to classify.
    pub fn network(kind: NetKind, msg: impl Into<String>) -> Self {
        ArxivError::Network {
            kind,
            msg: msg.into(),
            retry_after: None,
        }
    }

    fn http(what: &str, status: reqwest::StatusCode, retry_after: Option<Duration>) -> Self {
        ArxivError::Network {
            kind: NetKind::Status(status.as_u16()),
            msg: format!("arXiv {} HTTP {}", what, status),
            retry_after,
        }
    }

    pub fn kind(&self) -> Option<NetKind> {
        match self {
            ArxivError::Network { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// Log token for any error, network or not.
    pub fn outcome(&self) -> &'static str {
        match self {
            ArxivError::Network { kind, .. } => kind.outcome(),
            ArxivError::NotFound => "not_found",
            ArxivError::PdfOnly => "pdf_only",
            ArxivError::NotImplemented => "not_implemented",
        }
    }

    fn is_retryable(&self) -> bool {
        match self {
            ArxivError::Network { kind, .. } => kind.is_retryable(),
            // NotFound / PdfOnly are upstream's verdict, not a transient failure.
            _ => false,
        }
    }

    fn retry_after(&self) -> Option<Duration> {
        match self {
            ArxivError::Network { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Request budget
// ---------------------------------------------------------------------------

/// Whole-request budget for the upstream calls one user request may make.
///
/// `routes::paper` issues up to four on a cache miss, so per-attempt timeouts
/// alone compound to ~140 s. This caps the total instead, so a doomed request
/// fails fast rather than spending every per-call budget in turn.
pub const UPSTREAM_BUDGET: Duration = Duration::from_secs(45);

/// Below this, the budget is spent: another attempt only delays the error.
const BUDGET_FLOOR: Duration = Duration::from_millis(500);

tokio::task_local! {
    static UPSTREAM_DEADLINE: Instant;
}

/// Runs `fut` under a request-scoped upstream deadline.
///
/// Calls made outside such a scope (the `mcp` binary, unit tests) are unbounded
/// in aggregate and simply get the per-attempt timeout.
pub async fn with_upstream_budget<F: Future>(budget: Duration, fut: F) -> F::Output {
    UPSTREAM_DEADLINE.scope(Instant::now() + budget, fut).await
}

/// `None` when no budget is in scope.
fn budget_remaining() -> Option<Duration> {
    UPSTREAM_DEADLINE
        .try_with(|deadline| deadline.saturating_duration_since(Instant::now()))
        .ok()
}

fn budget_spent() -> bool {
    budget_remaining().is_some_and(|left| left < BUDGET_FLOOR)
}

// ---------------------------------------------------------------------------
// Retry
// ---------------------------------------------------------------------------

/// Initial attempt plus one retry.
///
/// Deeper retry does not pay: arXiv's failures arrive in 1-2 minute bursts, so
/// within a burst a third attempt fails too, and outside one a single retry
/// already catches the isolated case.
const MAX_ATTEMPTS: usize = 2;

/// Deliberately short. A long wait cannot outlast a burst and only hurts the
/// common case.
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_JITTER_MS: u64 = 400;

/// An honest `Retry-After` longer than this should fail the request rather than
/// stall the user behind it.
const RETRY_AFTER_CAP: Duration = Duration::from_secs(3);

/// Cheap jitter without a RNG dependency: the sub-second clock is uncorrelated
/// enough to keep concurrent retries from re-colliding.
fn jitter_ms(span: u64) -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()) % span.max(1))
        .unwrap_or(0)
}

/// How long to wait before retrying, or `None` for "do not retry".
fn retry_delay(err: &ArxivError) -> Option<Duration> {
    let base = match err.retry_after() {
        Some(hint) if hint > RETRY_AFTER_CAP => return None,
        Some(hint) => hint,
        None => BACKOFF_BASE,
    };
    let delay = base + Duration::from_millis(jitter_ms(BACKOFF_JITTER_MS));
    match budget_remaining() {
        // Sleeping would consume the rest of the budget; fail now instead.
        Some(left) if left <= delay + BUDGET_FLOOR => None,
        _ => Some(delay),
    }
}

/// Drives one upstream endpoint with a bounded retry and the shared request
/// budget, emitting one structured line per terminal outcome.
///
/// Lives here rather than in `routes` so the `mcp` crate, which uses
/// [`ReqwestArxivClient`] directly, gets the same behaviour.
async fn with_retries<T, F, Fut>(
    upstream: &'static str,
    paper_id: &str,
    mut attempt_fn: F,
) -> Result<T, ArxivError>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Result<T, ArxivError>>,
{
    let mut last_err: Option<ArxivError> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        if budget_spent() {
            let err = last_err.unwrap_or_else(|| {
                ArxivError::network(NetKind::Timeout, "upstream request budget exhausted")
            });
            tracing::error!(
                upstream,
                paper_id,
                attempts = attempt - 1,
                outcome = "budget_exhausted",
                error = %err,
                "arXiv upstream abandoned: request budget spent"
            );
            return Err(err);
        }

        match attempt_fn(attempt).await {
            Ok(value) => {
                if attempt > 1 {
                    tracing::info!(
                        upstream,
                        paper_id,
                        attempt,
                        outcome = "ok",
                        retry_rescued = true,
                        "arXiv upstream succeeded after retry"
                    );
                }
                return Ok(value);
            }
            Err(err) => {
                let delay = if err.is_retryable() && attempt < MAX_ATTEMPTS {
                    retry_delay(&err)
                } else {
                    None
                };
                match delay {
                    Some(delay) => {
                        tracing::warn!(
                            upstream,
                            paper_id,
                            attempt,
                            outcome = err.outcome(),
                            backoff_ms = delay.as_millis() as u64,
                            error = %err,
                            "arXiv upstream attempt failed; retrying"
                        );
                        last_err = Some(err);
                        tokio::time::sleep(delay).await;
                    }
                    None => {
                        tracing::error!(
                            upstream,
                            paper_id,
                            attempts = attempt,
                            outcome = err.outcome(),
                            error = %err,
                            "arXiv upstream failed"
                        );
                        return Err(err);
                    }
                }
            }
        }
    }
    // The loop always returns; this only satisfies the type checker.
    Err(last_err
        .unwrap_or_else(|| ArxivError::network(NetKind::Other, "arXiv retry loop exhausted")))
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

/// arXiv's API host (Atom metadata and search).
const API_BASE: &str = "https://export.arxiv.org";
/// arXiv's main site (e-print archives, PDFs, HTML renderings).
const SITE_BASE: &str = "https://arxiv.org";

pub struct ReqwestArxivClient {
    http: reqwest::Client,
    api_base: String,
    site_base: String,
}

impl Default for ReqwestArxivClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Connect is fast in practice (~4 ms measured against arXiv from prod), so a
/// slow connect is a genuine failure rather than the origin slow tail we want to
/// absorb.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-attempt ceiling. Above the 12.8 s real-world success measured on prod and
/// the 6 s observed TTFB, below the point where waiting stops paying. Sized from
/// the measured TTFB distribution: p50 ~0.18 s, occasional 6 s, tail > 25 s.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(20);

impl ReqwestArxivClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent("markxiv/0.1 (+https://github.com/)")
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(ATTEMPT_TIMEOUT)
            .build()
            .expect("failed to build reqwest client");
        Self {
            http,
            api_base: API_BASE.to_string(),
            site_base: SITE_BASE.to_string(),
        }
    }

    /// Test seam: points the client at a local server so the real retry, timeout
    /// and status handling can be exercised. Deliberately not wired to any env
    /// var, so prod cannot be misconfigured onto a different host.
    pub fn with_bases(api_base: &str, site_base: &str) -> Self {
        Self {
            api_base: api_base.trim_end_matches('/').to_string(),
            site_base: site_base.trim_end_matches('/').to_string(),
            ..Self::new()
        }
    }

    /// Issues one attempt, emitting exactly one `outcome=` line for it.
    ///
    /// Non-success statuses come back as `Ok`: each endpoint maps them to its
    /// own semantics (404 means "no source" on one endpoint and "no paper" on
    /// another), so that decision stays with the caller.
    async fn send_attempt(
        &self,
        upstream: &'static str,
        paper_id: &str,
        attempt: usize,
        req: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, ArxivError> {
        // Never let a single attempt outlive the whole-request budget.
        let req = match budget_remaining() {
            Some(left) if left < ATTEMPT_TIMEOUT => req.timeout(left),
            _ => req,
        };
        let started = Instant::now();
        match req.send().await {
            Ok(res) => {
                // `send()` resolves on response headers, so this is a true TTFB.
                let status = res.status();
                tracing::info!(
                    upstream,
                    paper_id,
                    attempt,
                    outcome = status_outcome(status),
                    status = status.as_u16(),
                    ttfb_ms = started.elapsed().as_millis() as u64,
                    "arXiv upstream attempt"
                );
                Ok(res)
            }
            Err(e) => {
                let kind = NetKind::classify(&e);
                tracing::info!(
                    upstream,
                    paper_id,
                    attempt,
                    outcome = kind.outcome(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "arXiv upstream attempt"
                );
                Err(ArxivError::from_reqwest(e))
            }
        }
    }
}

fn status_outcome(status: reqwest::StatusCode) -> &'static str {
    if status.is_success() {
        "ok"
    } else {
        NetKind::Status(status.as_u16()).outcome()
    }
}

/// Parses `Retry-After`'s delta-seconds form.
///
/// The HTTP-date form is ignored: honouring it would need a date parser, and any
/// value we are willing to wait for is only a few seconds anyway.
fn retry_after_hint(res: &reqwest::Response) -> Option<Duration> {
    let raw = res
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    raw.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// URL construction is deterministic, so a failure here is a bug rather than a
/// transient upstream problem — classified as non-retryable.
fn query_url(base: &str, params: &[(&str, &str)]) -> Result<Url, ArxivError> {
    Url::parse_with_params(&format!("{}/api/query", base), params).map_err(|e| {
        ArxivError::network(NetKind::Other, format!("building arXiv query url: {}", e))
    })
}

/// True when the body is a well-formed Atom feed that simply carries no entries
/// — arXiv's way of saying "no such paper".
fn is_empty_atom_feed(body: &str) -> bool {
    body.contains("<feed") && !body.contains("<entry")
}

#[async_trait]
impl ArxivClient for ReqwestArxivClient {
    async fn exists(&self, id: &str) -> Result<bool, ArxivError> {
        with_retries("exists", id, |attempt| async move {
            let url = query_url(&self.api_base, &[("id_list", id)])?;
            let req = self
                .http
                .get(url)
                .header(reqwest::header::ACCEPT, "application/atom+xml");
            let res = self.send_attempt("exists", id, attempt, req).await?;
            let status = res.status();
            if !status.is_success() {
                return Err(ArxivError::http(
                    "exists check",
                    status,
                    retry_after_hint(&res),
                ));
            }
            let body = res.text().await.map_err(ArxivError::from_reqwest)?;
            // Minimal parse: an empty feed has no <entry>; existing id yields at least one <entry>
            Ok(body.contains("<entry"))
        })
        .await
    }

    async fn get_source_archive(&self, id: &str) -> Result<Bytes, ArxivError> {
        let result = with_retries("e_print", id, |attempt| async move {
            let url = format!("{}/e-print/{}", self.site_base, id);
            let req = self.http.get(url).header(
                reqwest::header::ACCEPT,
                "application/x-eprint-tar, application/x-tar, application/octet-stream",
            );
            let res = self.send_attempt("e_print", id, attempt, req).await?;

            let status = res.status();
            if status.is_success() {
                // Inspect content-type and payload to avoid passing non-archives downstream
                let content_type = res
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_ascii_lowercase();

                let bytes = res.bytes().await.map_err(ArxivError::from_reqwest)?;

                // If arXiv returns a PDF (no source available), map to PdfOnly
                if content_type.contains("application/pdf") || looks_like_pdf(&bytes) {
                    return Err(ArxivError::PdfOnly);
                }
                // Some upstream issues (e.g., HTML error pages) can slip through as 200s
                if content_type.contains("text/html") || looks_like_html(&bytes) {
                    return Err(ArxivError::network(
                        NetKind::Decode,
                        "arXiv returned HTML when requesting e-print",
                    ));
                }
                // Otherwise, assume it's an archive (tar or tar.gz). The converter will try both.
                return Ok(bytes);
            }
            // 400/404 mean this paper genuinely has no source archive.
            if status.as_u16() == 400 || status.as_u16() == 404 {
                return Err(ArxivError::PdfOnly);
            }
            // 403 is deliberately NOT folded in above: arXiv also serves it while
            // throttling, where treating it as "no source" silently downgrades a
            // transient failure into a permanently worse PDF rendering.
            Err(ArxivError::http("e-print", status, retry_after_hint(&res)))
        })
        .await;

        match result {
            // A 403 that survived the retry is most likely the real thing, so keep
            // the historical PDF fallback rather than failing the request — but it
            // is now logged instead of silent.
            Err(ArxivError::Network {
                kind: NetKind::Status(403),
                ..
            }) => {
                tracing::warn!(
                    upstream = "e_print",
                    paper_id = id,
                    outcome = "http_403_persisted",
                    "arXiv kept refusing the e-print after retry; falling back to PDF"
                );
                Err(ArxivError::PdfOnly)
            }
            other => other,
        }
    }

    async fn get_pdf(&self, id: &str) -> Result<Bytes, ArxivError> {
        with_retries("pdf", id, |attempt| async move {
            let url = format!("{}/pdf/{}.pdf", self.site_base, id);
            let req = self
                .http
                .get(&url)
                .header(reqwest::header::ACCEPT, "application/pdf");
            let res = self.send_attempt("pdf", id, attempt, req).await?;

            let status = res.status();
            if status == reqwest::StatusCode::NOT_FOUND {
                return Err(ArxivError::NotFound);
            }
            if !status.is_success() {
                return Err(ArxivError::http("pdf", status, retry_after_hint(&res)));
            }

            let bytes = res.bytes().await.map_err(ArxivError::from_reqwest)?;

            if looks_like_pdf(&bytes) {
                Ok(bytes)
            } else {
                Err(ArxivError::network(
                    NetKind::Decode,
                    "unexpected non-PDF payload when requesting PDF",
                ))
            }
        })
        .await
    }

    async fn get_metadata(&self, id: &str) -> Result<Metadata, ArxivError> {
        with_retries("metadata", id, |attempt| async move {
            let url = query_url(&self.api_base, &[("id_list", id)])?;
            let req = self
                .http
                .get(url)
                .header(reqwest::header::ACCEPT, "application/atom+xml");
            let res = self.send_attempt("metadata", id, attempt, req).await?;

            let status = res.status();
            if status == reqwest::StatusCode::NOT_FOUND {
                return Err(ArxivError::NotFound);
            }
            if !status.is_success() {
                return Err(ArxivError::http("metadata", status, retry_after_hint(&res)));
            }
            let body = res.text().await.map_err(ArxivError::from_reqwest)?;
            match parse_atom_metadata(&body) {
                Some(meta) => Ok(meta),
                // A feed that parsed but carried no <entry> means arXiv has no such
                // paper: a real 404. A body that is not a feed at all is an upstream
                // failure dressed as a 200, and must not reach the user as a 404.
                None if is_empty_atom_feed(&body) => Err(ArxivError::NotFound),
                None => Err(ArxivError::network(
                    NetKind::Decode,
                    "arXiv metadata response was not a parseable Atom feed",
                )),
            }
        })
        .await
    }

    async fn search(&self, query: &str, max_results: u32) -> Result<Vec<SearchResult>, ArxivError> {
        let max_results = max_results.min(50);
        let max_results = max_results.to_string();
        // Borrow, so the `async move` block captures a `Copy` &str and the closure
        // stays `FnMut` across attempts.
        let max_results = max_results.as_str();
        with_retries("search", query, |attempt| async move {
            let url = query_url(
                &self.api_base,
                &[
                    ("search_query", format!("all:{}", query).as_str()),
                    ("start", "0"),
                    ("max_results", max_results),
                ],
            )?;
            let req = self
                .http
                .get(url)
                .header(reqwest::header::ACCEPT, "application/atom+xml");
            let res = self.send_attempt("search", query, attempt, req).await?;

            let status = res.status();
            if !status.is_success() {
                return Err(ArxivError::http("search", status, retry_after_hint(&res)));
            }

            let body = res.text().await.map_err(ArxivError::from_reqwest)?;

            Ok(parse_atom_search_results(&body))
        })
        .await
    }

    async fn get_html_figure_image_urls(&self, id: &str) -> Result<Vec<String>, ArxivError> {
        // Deliberately un-retried: the caller already treats this as best-effort
        // (`unwrap_or_default()`), and it is the third round-trip of a cache miss.
        if budget_spent() {
            tracing::warn!(
                upstream = "html",
                paper_id = id,
                outcome = "budget_exhausted",
                "skipping best-effort figure lookup; request budget spent"
            );
            return Ok(vec![]);
        }
        let base_url = format!("{}/html/{}", self.site_base, id);
        let res = self
            .send_attempt("html", id, 1, self.http.get(&base_url))
            .await?;
        if !res.status().is_success() {
            return Ok(vec![]);
        }
        let html = res.text().await.map_err(ArxivError::from_reqwest)?;
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
        let arxiv_id = id.rsplit('/').next().unwrap_or(&id).to_string();

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
            let full_url = resolve_figure_img_url(base_url, src);
            urls.push(full_url);
        }
    }

    urls
}

fn resolve_figure_img_url(base_url: &str, src: &str) -> String {
    if src.starts_with("http://") || src.starts_with("https://") {
        return src.to_string();
    }

    let base = base_url.trim_end_matches('/');
    let src_trimmed = src.trim_start_matches('/');

    let Some(base_url) = Url::parse(base).ok() else {
        return format!("{}/{}", base, src_trimmed);
    };

    // arXiv HTML pages can reference figure images as "<paper-id>vN/Figures/..."
    // even when the page URL is "/html/<paper-id>". In that case, joining with
    // "/html/<paper-id>/" duplicates the id segment, so anchor at "/html/".
    if let Some(base_id) = base_url
        .path_segments()
        .and_then(|mut segs| segs.next_back())
    {
        let base_id_no_version = strip_version_suffix(base_id);
        let version_prefixed = src_trimmed.starts_with(&(base_id.to_string() + "/"))
            || src_trimmed.starts_with(&(base_id_no_version.to_string() + "v"));
        if version_prefixed {
            if let Some(host) = base_url.host_str() {
                return format!("{}://{}/html/{}", base_url.scheme(), host, src_trimmed);
            }
        }
    }

    if src.starts_with('/') {
        if let Some(host) = base_url.host_str() {
            return format!("{}://{}{}", base_url.scheme(), host, src);
        }
    }

    format!("{}/{}", base, src_trimmed)
}

fn strip_version_suffix(id: &str) -> &str {
    let Some(vpos) = id.rfind('v') else {
        return id;
    };
    let suffix = &id[vpos + 1..];
    if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
        return id;
    }
    &id[..vpos]
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
mod retry_tests {
    use super::*;
    use std::sync::Mutex;

    /// Drives the real `with_retries` loop off a script, so retry behaviour is
    /// testable without any HTTP. `MockArxivClient` cannot reach this code: retry
    /// lives inside `ReqwestArxivClient`, behind the `ArxivClient` trait.
    async fn run_script(script: Vec<Result<u8, ArxivError>>) -> (Result<u8, ArxivError>, usize) {
        let queue = Mutex::new(std::collections::VecDeque::from(script));
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let out = with_retries("test", "1234.5678", |_attempt| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let next = queue.lock().unwrap().pop_front();
            async move { next.unwrap_or(Err(ArxivError::network(NetKind::Other, "script empty"))) }
        })
        .await;
        (out, calls.load(std::sync::atomic::Ordering::SeqCst))
    }

    fn timeout_err() -> ArxivError {
        ArxivError::network(NetKind::Timeout, "timed out")
    }

    fn status_err(code: u16, retry_after: Option<Duration>) -> ArxivError {
        ArxivError::Network {
            kind: NetKind::Status(code),
            msg: format!("HTTP {}", code),
            retry_after,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retries_a_timeout_then_succeeds() {
        let (out, calls) = run_script(vec![Err(timeout_err()), Ok(7)]).await;
        assert_eq!(out.unwrap(), 7);
        assert_eq!(
            calls, 2,
            "should have made an initial attempt plus one retry"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stops_at_max_attempts() {
        let (out, calls) = run_script(vec![Err(timeout_err()), Err(timeout_err()), Ok(7)]).await;
        assert!(out.is_err());
        assert_eq!(calls, MAX_ATTEMPTS, "must not retry past MAX_ATTEMPTS");
    }

    #[tokio::test(start_paused = true)]
    async fn does_not_retry_unclassified_failures() {
        let (out, calls) = run_script(vec![
            Err(ArxivError::network(NetKind::Other, "bad url")),
            Ok(7),
        ])
        .await;
        assert!(out.is_err());
        assert_eq!(calls, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn does_not_retry_a_decoded_but_wrong_payload() {
        let (out, calls) = run_script(vec![
            Err(ArxivError::network(NetKind::Decode, "html")),
            Ok(7),
        ])
        .await;
        assert!(out.is_err());
        assert_eq!(calls, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn does_not_retry_upstream_verdicts() {
        for verdict in [ArxivError::NotFound, ArxivError::PdfOnly] {
            let (out, calls) = run_script(vec![Err(verdict), Ok(7)]).await;
            assert!(out.is_err());
            assert_eq!(calls, 1, "NotFound/PdfOnly are answers, not failures");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retries_429_and_5xx_and_403() {
        for code in [429, 500, 503, 403] {
            let (out, calls) = run_script(vec![Err(status_err(code, None)), Ok(7)]).await;
            assert_eq!(out.unwrap(), 7, "HTTP {} should be retried", code);
            assert_eq!(calls, 2);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn does_not_retry_ordinary_4xx() {
        let (out, calls) = run_script(vec![Err(status_err(400, None)), Ok(7)]).await;
        assert!(out.is_err());
        assert_eq!(calls, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn honours_a_short_retry_after() {
        let (out, calls) = run_script(vec![
            Err(status_err(429, Some(Duration::from_secs(2)))),
            Ok(7),
        ])
        .await;
        assert_eq!(out.unwrap(), 7);
        assert_eq!(calls, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn refuses_to_wait_out_a_long_retry_after() {
        // An honest 30 s should fail the request, not stall the user behind it.
        let (out, calls) = run_script(vec![
            Err(status_err(429, Some(Duration::from_secs(30)))),
            Ok(7),
        ])
        .await;
        assert!(out.is_err());
        assert_eq!(calls, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_spent_budget_stops_further_attempts() {
        let budget = Duration::from_millis(600);
        let (out, calls) = with_upstream_budget(budget, async {
            // Burn the budget inside the scope, then let the retry loop notice.
            tokio::time::sleep(Duration::from_secs(1)).await;
            run_script(vec![Err(timeout_err()), Ok(7)]).await
        })
        .await;
        assert!(out.is_err());
        assert_eq!(calls, 0, "no attempt should start once the budget is spent");
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_is_skipped_when_it_would_outlast_the_budget() {
        // Enough budget for the first attempt but not for a ~1 s backoff after it.
        let (out, calls) = with_upstream_budget(Duration::from_millis(900), async {
            run_script(vec![Err(timeout_err()), Ok(7)]).await
        })
        .await;
        assert!(out.is_err());
        assert_eq!(calls, 1);
    }

    #[test]
    fn retry_delay_rejects_long_hints_and_accepts_short_ones() {
        assert!(retry_delay(&status_err(429, Some(Duration::from_secs(30)))).is_none());
        let short = retry_delay(&status_err(429, Some(Duration::from_secs(2)))).expect("delay");
        assert!(short >= Duration::from_secs(2));
        assert!(short < Duration::from_secs(3));
    }

    #[test]
    fn retry_delay_defaults_to_about_one_second_with_jitter() {
        let delay = retry_delay(&timeout_err()).expect("delay");
        assert!(delay >= BACKOFF_BASE);
        assert!(delay < BACKOFF_BASE + Duration::from_millis(BACKOFF_JITTER_MS));
    }

    #[test]
    fn jitter_stays_inside_its_span() {
        for _ in 0..100 {
            assert!(jitter_ms(400) < 400);
        }
        assert_eq!(jitter_ms(0), 0, "a zero span must not divide by zero");
    }
}

#[cfg(test)]
mod classification_tests {
    use super::*;

    #[test]
    fn outcome_tokens_are_stable_for_log_queries() {
        assert_eq!(NetKind::Timeout.outcome(), "timeout");
        assert_eq!(NetKind::Connect.outcome(), "connect");
        assert_eq!(NetKind::Status(429).outcome(), "http_429");
        assert_eq!(NetKind::Status(503).outcome(), "http_5xx");
        assert_eq!(NetKind::Status(404).outcome(), "http_4xx");
        assert_eq!(NetKind::Decode.outcome(), "decode");
        assert_eq!(NetKind::Other.outcome(), "other");
    }

    #[test]
    fn retryable_set_matches_the_transient_classes() {
        assert!(NetKind::Timeout.is_retryable());
        assert!(NetKind::Connect.is_retryable());
        assert!(NetKind::Status(429).is_retryable());
        assert!(NetKind::Status(500).is_retryable());
        assert!(NetKind::Status(503).is_retryable());
        // arXiv serves 403 while throttling, so it is worth one more try.
        assert!(NetKind::Status(403).is_retryable());

        assert!(!NetKind::Status(400).is_retryable());
        assert!(!NetKind::Status(404).is_retryable());
        assert!(!NetKind::Decode.is_retryable());
        // URL construction is deterministic; retrying it cannot help.
        assert!(!NetKind::Other.is_retryable());
    }

    #[test]
    fn empty_feed_is_distinguished_from_an_unparseable_body() {
        let empty = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom">
            <title>ArXiv Query</title></feed>"#;
        assert!(is_empty_atom_feed(empty));

        let populated = r#"<feed><entry><title>T</title></entry></feed>"#;
        assert!(!is_empty_atom_feed(populated));

        // An upstream error page is not an empty feed, and must not become a 404.
        assert!(!is_empty_atom_feed(
            "<html><body>502 Bad Gateway</body></html>"
        ));
        assert!(!is_empty_atom_feed(""));
    }

    #[test]
    fn query_url_builds_the_export_endpoint() {
        let url = query_url(API_BASE, &[("id_list", "2606.29150")]).expect("url");
        assert_eq!(url.host_str(), Some("export.arxiv.org"));
        assert_eq!(url.query(), Some("id_list=2606.29150"));
    }

    #[test]
    fn network_errors_report_their_kind() {
        let err = ArxivError::network(NetKind::Timeout, "boom");
        assert_eq!(err.kind(), Some(NetKind::Timeout));
        assert_eq!(err.outcome(), "timeout");
        assert!(err.is_retryable());

        assert_eq!(ArxivError::NotFound.kind(), None);
        assert_eq!(ArxivError::NotFound.outcome(), "not_found");
        assert_eq!(ArxivError::PdfOnly.outcome(), "pdf_only");
    }
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
    fn parse_html_figure_images_avoids_duplicate_id_segment_for_versioned_src() {
        let html = r#"<figure><img src="1706.03762v7/Figures/ModalNet-21.png"/></figure>"#;
        let urls = parse_html_figure_image_urls(html, "https://arxiv.org/html/1706.03762");
        assert_eq!(
            urls,
            vec!["https://arxiv.org/html/1706.03762v7/Figures/ModalNet-21.png"]
        );
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
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Responses to hand out in order before falling back to the canned one, so a
    /// test can express "fail, then succeed" rather than one fixed answer.
    type Script<T> = Arc<Mutex<VecDeque<Result<T, ArxivError>>>>;

    fn next_scripted<T: Clone>(
        script: &Script<T>,
        fallback: &Result<T, ArxivError>,
    ) -> Result<T, ArxivError> {
        script
            .lock()
            .expect("mock script poisoned")
            .pop_front()
            .unwrap_or_else(|| fallback.clone())
    }

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
        pub html_figure_urls_calls: Arc<AtomicUsize>,
        pub archive_script: Script<Bytes>,
        pub pdf_script: Script<Bytes>,
        pub metadata_script: Script<Metadata>,
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
                html_figure_urls_calls: Arc::new(AtomicUsize::new(0)),
                archive_script: Script::default(),
                pdf_script: Script::default(),
                metadata_script: Script::default(),
            }
        }

        /// Queues metadata responses consumed in order, ahead of the canned one.
        pub fn script_metadata(self, responses: Vec<Result<Metadata, ArxivError>>) -> Self {
            *self.metadata_script.lock().unwrap() = responses.into();
            self
        }

        /// Queues source-archive responses consumed in order.
        pub fn script_archive(self, responses: Vec<Result<Bytes, ArxivError>>) -> Self {
            *self.archive_script.lock().unwrap() = responses.into();
            self
        }

        /// Queues PDF responses consumed in order.
        pub fn script_pdf(self, responses: Vec<Result<Bytes, ArxivError>>) -> Self {
            *self.pdf_script.lock().unwrap() = responses.into();
            self
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
            next_scripted(&self.archive_script, &self.archive_response)
        }

        async fn get_pdf(&self, _id: &str) -> Result<Bytes, ArxivError> {
            self.pdf_calls.fetch_add(1, Ordering::SeqCst);
            next_scripted(&self.pdf_script, &self.pdf_response)
        }

        async fn get_metadata(&self, _id: &str) -> Result<Metadata, ArxivError> {
            self.metadata_calls.fetch_add(1, Ordering::SeqCst);
            next_scripted(&self.metadata_script, &self.metadata_response)
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
            self.html_figure_urls_calls.fetch_add(1, Ordering::SeqCst);
            self.html_figure_urls_response.clone()
        }
    }
}
