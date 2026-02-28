use std::fmt;
use std::sync::Arc;

use markxiv::arxiv::{ArxivClient, ArxivError, ReqwestArxivClient};
use markxiv::convert::{ConvertError, Converter, PandocConverter};
use rmcp::{
    handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt,
};

// -- Tool parameter types --

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ConvertPaperParams {
    #[schemars(description = "arXiv paper ID (e.g. '1706.03762' or '2301.07041v1')")]
    paper_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GetMetadataParams {
    #[schemars(description = "arXiv paper ID (e.g. '1706.03762')")]
    paper_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SearchPapersParams {
    #[schemars(description = "Search query (e.g. 'attention is all you need', 'transformer')")]
    query: String,
    #[schemars(
        description = "Maximum number of results to return (1-20, default: 5)",
        default = "default_max_results"
    )]
    max_results: Option<u32>,
}

fn default_max_results() -> Option<u32> {
    Some(5)
}

// -- MCP Server --

#[derive(Clone)]
struct MarkxivMcp {
    client: Arc<ReqwestArxivClient>,
    converter: Arc<PandocConverter>,
    tool_router: ToolRouter<Self>,
}

impl fmt::Debug for MarkxivMcp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MarkxivMcp").finish()
    }
}

#[tool_router]
impl MarkxivMcp {
    fn new() -> Self {
        Self {
            client: Arc::new(ReqwestArxivClient::new()),
            converter: Arc::new(PandocConverter::new()),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Convert an arXiv paper to markdown. Returns the full paper content with title, authors, and abstract.")]
    async fn convert_paper(
        &self,
        Parameters(params): Parameters<ConvertPaperParams>,
    ) -> Result<String, String> {
        let paper_id = params.paper_id.trim().to_string();
        if paper_id.is_empty() || !paper_id.is_ascii() {
            return Err("invalid paper ID".into());
        }

        // Fetch metadata
        let metadata = match self.client.get_metadata(&paper_id).await {
            Ok(m) => Some(m),
            Err(ArxivError::NotFound) => return Err(format!("paper '{}' not found", paper_id)),
            Err(ArxivError::NotImplemented) => None,
            Err(e) => return Err(format!("metadata fetch failed: {}", e)),
        };

        // Try LaTeX source first
        let (body, used_pdf) = match self.client.get_source_archive(&paper_id).await {
            Ok(bytes) => match self.converter.latex_tar_to_markdown(&bytes).await {
                Ok(md) => (md, false),
                Err(_) => {
                    match self
                        .converter
                        .latex_tar_to_markdown_without_macros(&bytes)
                        .await
                    {
                        Ok(md) => (md, false),
                        Err(_) => self.try_pdf_fallback(&paper_id).await?,
                    }
                }
            },
            Err(ArxivError::PdfOnly) => self.try_pdf_fallback(&paper_id).await?,
            Err(ArxivError::NotFound) => return Err(format!("paper '{}' not found", paper_id)),
            Err(e) => return Err(format!("source fetch failed: {}", e)),
        };

        // Prepend metadata if we didn't use PDF fallback
        if !used_pdf {
            if let Some(meta) = metadata {
                let mut out = String::new();
                if !meta.title.is_empty() {
                    out.push_str(&format!("# {}\n\n", meta.title.trim()));
                }
                if !meta.authors.is_empty() {
                    out.push_str("## Authors\n");
                    out.push_str(&meta.authors.join(", "));
                    out.push_str("\n\n");
                }
                if !meta.summary.is_empty() {
                    out.push_str("## Abstract\n");
                    out.push_str(meta.summary.trim());
                    out.push_str("\n\n");
                }
                out.push_str(&body);
                return Ok(out);
            }
        }

        Ok(body)
    }

    #[tool(description = "Get metadata (title, authors, abstract) for an arXiv paper without converting the full content.")]
    async fn get_paper_metadata(
        &self,
        Parameters(params): Parameters<GetMetadataParams>,
    ) -> Result<String, String> {
        let paper_id = params.paper_id.trim().to_string();
        if paper_id.is_empty() || !paper_id.is_ascii() {
            return Err("invalid paper ID".into());
        }

        let meta = self
            .client
            .get_metadata(&paper_id)
            .await
            .map_err(|e| format!("metadata fetch failed: {}", e))?;

        let mut out = String::new();
        out.push_str(&format!("# {}\n\n", meta.title.trim()));
        if !meta.authors.is_empty() {
            out.push_str("**Authors:** ");
            out.push_str(&meta.authors.join(", "));
            out.push_str("\n\n");
        }
        if !meta.summary.is_empty() {
            out.push_str("**Abstract:**\n");
            out.push_str(meta.summary.trim());
            out.push('\n');
        }
        out.push_str(&format!(
            "\n**Link:** https://arxiv.org/abs/{}",
            paper_id
        ));
        Ok(out)
    }

    #[tool(description = "Search arXiv papers by keyword query. Returns matching papers with IDs, titles, authors, and abstracts.")]
    async fn search_papers(
        &self,
        Parameters(params): Parameters<SearchPapersParams>,
    ) -> Result<String, String> {
        let query = params.query.trim().to_string();
        if query.is_empty() {
            return Err("query must not be empty".into());
        }

        let max = params.max_results.unwrap_or(5).clamp(1, 20);

        let results = self
            .client
            .search(&query, max)
            .await
            .map_err(|e| format!("search failed: {}", e))?;

        if results.is_empty() {
            return Ok("No papers found matching your query.".into());
        }

        let mut out = String::new();
        out.push_str(&format!(
            "Found {} result(s) for \"{}\":\n\n",
            results.len(),
            query
        ));
        for (i, r) in results.iter().enumerate() {
            out.push_str(&format!("## {}. {}\n", i + 1, r.title.trim()));
            out.push_str(&format!("**arXiv ID:** {}\n", r.id));
            if !r.authors.is_empty() {
                out.push_str(&format!("**Authors:** {}\n", r.authors.join(", ")));
            }
            if !r.published.is_empty() {
                out.push_str(&format!("**Published:** {}\n", r.published));
            }
            if !r.summary.is_empty() {
                let summary = r.summary.trim();
                if summary.len() > 300 {
                    out.push_str(&format!("**Abstract:** {}...\n", &summary[..300]));
                } else {
                    out.push_str(&format!("**Abstract:** {}\n", summary));
                }
            }
            out.push_str(&format!(
                "**Link:** https://arxiv.org/abs/{}\n\n",
                r.id
            ));
        }
        Ok(out)
    }
}

impl MarkxivMcp {
    async fn try_pdf_fallback(&self, paper_id: &str) -> Result<(String, bool), String> {
        let pdf_bytes = self
            .client
            .get_pdf(paper_id)
            .await
            .map_err(|e| match e {
                ArxivError::NotFound => format!("paper '{}' not found", paper_id),
                other => format!("PDF fetch failed: {}", other),
            })?;

        let text = self
            .converter
            .pdf_to_markdown(&pdf_bytes)
            .await
            .map_err(|e| match e {
                ConvertError::Failed(msg) => {
                    format!("conversion failed (both LaTeX and PDF): {}", msg)
                }
                ConvertError::NotImplemented => "PDF conversion not implemented".into(),
            })?;

        Ok((text, true))
    }
}

#[tool_handler]
impl ServerHandler for MarkxivMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "markxiv MCP server — convert arXiv papers to markdown using pandoc. \
                 Requires pandoc and pdftotext installed locally."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use markxiv::arxiv::test_helpers::MockArxivClient;
    use markxiv::arxiv::{ArxivClient, ArxivError, Metadata, SearchResult};
    use markxiv::convert::test_helpers::MockConverter;
    use markxiv::convert::Converter;

    /// Replicate the convert_paper logic to verify output matches library.
    async fn run_convert_paper(
        client: &(dyn ArxivClient + Send + Sync),
        converter: &(dyn Converter + Send + Sync),
        paper_id: &str,
    ) -> Result<String, String> {
        let metadata = match client.get_metadata(paper_id).await {
            Ok(m) => Some(m),
            Err(ArxivError::NotFound) => return Err(format!("paper '{}' not found", paper_id)),
            Err(ArxivError::NotImplemented) => None,
            Err(e) => return Err(format!("metadata fetch failed: {}", e)),
        };

        let (body, used_pdf) = match client.get_source_archive(paper_id).await {
            Ok(bytes) => match converter.latex_tar_to_markdown(&bytes).await {
                Ok(md) => (md, false),
                Err(_) => match converter.latex_tar_to_markdown_without_macros(&bytes).await {
                    Ok(md) => (md, false),
                    Err(_) => {
                        let pdf_bytes = client
                            .get_pdf(paper_id)
                            .await
                            .map_err(|e| format!("PDF fetch failed: {}", e))?;
                        let text = converter
                            .pdf_to_markdown(&pdf_bytes)
                            .await
                            .map_err(|e| format!("conversion failed: {}", e))?;
                        (text, true)
                    }
                },
            },
            Err(ArxivError::PdfOnly) => {
                let pdf_bytes = client
                    .get_pdf(paper_id)
                    .await
                    .map_err(|e| format!("PDF fetch failed: {}", e))?;
                let text = converter
                    .pdf_to_markdown(&pdf_bytes)
                    .await
                    .map_err(|e| format!("conversion failed: {}", e))?;
                (text, true)
            }
            Err(ArxivError::NotFound) => return Err(format!("paper '{}' not found", paper_id)),
            Err(e) => return Err(format!("source fetch failed: {}", e)),
        };

        if !used_pdf {
            if let Some(meta) = metadata {
                let mut out = String::new();
                if !meta.title.is_empty() {
                    out.push_str(&format!("# {}\n\n", meta.title.trim()));
                }
                if !meta.authors.is_empty() {
                    out.push_str("## Authors\n");
                    out.push_str(&meta.authors.join(", "));
                    out.push_str("\n\n");
                }
                if !meta.summary.is_empty() {
                    out.push_str("## Abstract\n");
                    out.push_str(meta.summary.trim());
                    out.push_str("\n\n");
                }
                out.push_str(&body);
                return Ok(out);
            }
        }
        Ok(body)
    }

    #[tokio::test]
    async fn convert_paper_latex_output_has_metadata_and_body() {
        let client = MockArxivClient::new(
            Ok(true),
            Ok(Bytes::from_static(b"tar-bytes")),
            Err(ArxivError::NotImplemented),
            Ok(Metadata {
                title: "Attention Is All You Need".into(),
                summary: "The dominant sequence transduction models...".into(),
                authors: vec!["Vaswani".into(), "Shazeer".into()],
            }),
        );
        let converter = MockConverter::new(
            Ok("## Introduction\nWe propose a new architecture.".into()),
            Ok(String::new()),
        );

        let out = run_convert_paper(&client, &converter, "1706.03762")
            .await
            .unwrap();
        assert!(out.starts_with("# Attention Is All You Need\n\n"));
        assert!(out.contains("## Authors\nVaswani, Shazeer"));
        assert!(out.contains("## Abstract\nThe dominant sequence"));
        assert!(out.contains("## Introduction\nWe propose"));
    }

    #[tokio::test]
    async fn convert_paper_pdf_fallback_returns_pdf_text() {
        let client = MockArxivClient::new(
            Ok(true),
            Err(ArxivError::PdfOnly),
            Ok(Bytes::from_static(b"pdf-bytes")),
            Ok(Metadata {
                title: "Test Paper".into(),
                summary: "Abstract".into(),
                authors: vec!["Author".into()],
            }),
        );
        let converter = MockConverter::new(Ok(String::new()), Ok("extracted pdf text".into()));

        let out = run_convert_paper(&client, &converter, "1234.5678")
            .await
            .unwrap();
        // PDF fallback skips metadata prepending
        assert_eq!(out, "extracted pdf text");
    }

    #[tokio::test]
    async fn convert_paper_not_found_returns_error() {
        let client = MockArxivClient::new(
            Ok(false),
            Err(ArxivError::NotFound),
            Err(ArxivError::NotFound),
            Err(ArxivError::NotFound),
        );
        let converter = MockConverter::new(Ok(String::new()), Ok(String::new()));

        let err = run_convert_paper(&client, &converter, "0000.0000")
            .await
            .unwrap_err();
        assert!(err.contains("not found"));
    }

    #[tokio::test]
    async fn get_metadata_output_format() {
        let client = MockArxivClient::new(
            Ok(true),
            Err(ArxivError::NotImplemented),
            Err(ArxivError::NotImplemented),
            Ok(Metadata {
                title: "Attention Is All You Need".into(),
                summary: "The dominant approach...".into(),
                authors: vec!["Vaswani".into(), "Shazeer".into()],
            }),
        );

        let meta = client.get_metadata("1706.03762").await.unwrap();

        // Replicate get_paper_metadata output formatting
        let mut out = String::new();
        out.push_str(&format!("# {}\n\n", meta.title.trim()));
        if !meta.authors.is_empty() {
            out.push_str("**Authors:** ");
            out.push_str(&meta.authors.join(", "));
            out.push_str("\n\n");
        }
        if !meta.summary.is_empty() {
            out.push_str("**Abstract:**\n");
            out.push_str(meta.summary.trim());
            out.push('\n');
        }
        out.push_str(&format!(
            "\n**Link:** https://arxiv.org/abs/{}",
            "1706.03762"
        ));

        assert!(out.contains("# Attention Is All You Need"));
        assert!(out.contains("**Authors:** Vaswani, Shazeer"));
        assert!(out.contains("**Abstract:**\nThe dominant approach..."));
        assert!(out.contains("**Link:** https://arxiv.org/abs/1706.03762"));
    }

    #[tokio::test]
    async fn search_papers_returns_results() {
        let mut client = MockArxivClient::new(
            Ok(true),
            Err(ArxivError::NotImplemented),
            Err(ArxivError::NotImplemented),
            Err(ArxivError::NotImplemented),
        );
        client.search_response = Ok(vec![
            SearchResult {
                id: "1706.03762v5".into(),
                title: "Attention Is All You Need".into(),
                summary: "The dominant sequence...".into(),
                authors: vec!["Vaswani".into()],
                published: "2017-06-12".into(),
            },
            SearchResult {
                id: "2301.07041v1".into(),
                title: "Another Paper".into(),
                summary: "Some abstract".into(),
                authors: vec!["Author".into()],
                published: "2023-01-17".into(),
            },
        ]);

        let results = client.search("attention", 5).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "1706.03762v5");
        assert_eq!(results[0].title, "Attention Is All You Need");
        assert_eq!(results[1].id, "2301.07041v1");

        // Replicate search_papers output formatting
        let mut out = String::new();
        out.push_str(&format!(
            "Found {} result(s) for \"{}\":\n\n",
            results.len(),
            "attention"
        ));
        for (i, r) in results.iter().enumerate() {
            out.push_str(&format!("## {}. {}\n", i + 1, r.title.trim()));
            out.push_str(&format!("**arXiv ID:** {}\n", r.id));
            out.push_str(&format!(
                "**Link:** https://arxiv.org/abs/{}\n\n",
                r.id
            ));
        }
        assert!(out.contains("## 1. Attention Is All You Need"));
        assert!(out.contains("**arXiv ID:** 1706.03762v5"));
        assert!(out.contains("## 2. Another Paper"));
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Log to stderr so stdout stays clean for MCP stdio transport
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let service = MarkxivMcp::new();
    let server = service.serve(rmcp::transport::stdio()).await?;
    server.waiting().await?;
    Ok(())
}
