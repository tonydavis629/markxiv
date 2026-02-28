# markxiv-mcp

A Rust MCP (Model Context Protocol) server that converts arXiv papers to markdown using the markxiv library directly — no web service dependency.

Unlike other arXiv MCP servers that scrape HTML or call APIs, this one uses markxiv's pandoc-based conversion pipeline locally for the highest fidelity LaTeX-to-markdown output.

## Requirements

- **pandoc** — LaTeX to markdown conversion
- **pdftotext** (poppler-utils) — PDF fallback extraction
- **tar** — archive extraction (usually pre-installed)

Install on macOS:
```bash
brew install pandoc poppler
```

Install on Debian/Ubuntu:
```bash
sudo apt-get install -y pandoc poppler-utils
```

## Installation

### From source

```bash
cd mcp
cargo build --release
# Binary at ../target/release/markxiv-mcp
```

### Via cargo install

```bash
cargo install --path mcp
```

## Run The MCP Server

From the repo root (after `cargo build --release`):

```bash
./target/release/markxiv-mcp
```

Or if installed via `cargo install`:

```bash
markxiv-mcp
```

This server uses stdio transport, so it is meant to be launched by an MCP client (Claude Desktop, Inspector, etc.) rather than used as a standalone HTTP server.

For local debugging with MCP Inspector:

```bash
npx @modelcontextprotocol/inspector ./target/release/markxiv-mcp
```

## Claude Desktop Configuration

Add to your Claude Desktop config (`~/Library/Application Support/Claude/claude_desktop_config.json` on macOS):

```json
{
  "mcpServers": {
    "markxiv": {
      "command": "/path/to/markxiv-mcp"
    }
  }
}
```

Or if installed via `cargo install`:

```json
{
  "mcpServers": {
    "markxiv": {
      "command": "markxiv-mcp"
    }
  }
}
```

## Available Tools

### `convert_paper`

Convert an arXiv paper to markdown. Fetches the LaTeX source, converts via pandoc, and returns clean GitHub-Flavored Markdown with title, authors, and abstract prepended. Falls back to PDF text extraction when no LaTeX source is available.

**Parameters:**
- `paper_id` (string, required) — arXiv paper ID (e.g. `"1706.03762"` or `"2301.07041v1"`)

**Example:** "Convert the Attention Is All You Need paper" → calls `convert_paper` with `paper_id: "1706.03762"`

### `get_paper_metadata`

Get metadata (title, authors, abstract) for an arXiv paper without converting the full content. Useful for quick lookups.

**Parameters:**
- `paper_id` (string, required) — arXiv paper ID

### `search_papers`

Search arXiv papers by keyword query. Returns matching papers with IDs, titles, authors, and abstracts.

**Parameters:**
- `query` (string, required) — Search query (e.g. `"transformer architecture"`)
- `max_results` (integer, optional) — Number of results, 1-20 (default: 5)

## Environment Variables

- `MARKXIV_PANDOC_PATH` — custom path to pandoc binary (default: `pandoc`)
- `MARKXIV_PDFTOTEXT_PATH` — custom path to pdftotext binary (default: `pdftotext`)

## How It Differs From Other arXiv MCP Servers

Most arXiv MCP servers (arxiv-mcp-server, arxiv-latex-mcp, etc.) either:
- Fetch and return raw LaTeX source
- Use basic text extraction from PDFs
- Scrape the arXiv HTML pages

markxiv-mcp runs pandoc locally on the actual LaTeX source to produce clean, readable GitHub-Flavored Markdown — the same pipeline used by [markxiv.org](https://markxiv.org). This gives much better output for papers with complex math, tables, and formatting.
