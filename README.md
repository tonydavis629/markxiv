# markxiv

Just replace 'arxiv' in the URL with 'markxiv' to get the markdown

https://arxiv.org/abs/1706.03762 → https://markxiv.org/abs/1706.03762

## Basics

A minimal web service that mimics arXiv but serves Markdown instead of PDFs/HTML.

Given an arXiv ID, the server:
- Checks a local LRU cache for a converted result
- Fetches the paper’s LaTeX source from arXiv (if available)
- Extracts the archive, picks the main `.tex` file, converts it to Markdown using pandoc
- Falls back to `pdftotext` when LaTeX sources are unavailable or pandoc conversion fails
- Returns `text/markdown; charset=utf-8`

If a paper is PDF-only (no source available) or pandoc conversion fails, the server falls back to `pdftotext` and returns the extracted Markdown/plain text when that succeeds.

Returned Markdown includes the paper title and abstract prepended at the top.

## Requirements

- Rust toolchain (`cargo`, `rustc`) via rustup
- pandoc (for LaTeX → Markdown conversion)
- pdftotext (Poppler CLI, usually packaged as `poppler-utils`)
- tar (for extracting the arXiv source archive)

Most Linux/macOS environments already include `tar`. Windows 10+ includes `bsdtar` as `tar`.

## Install Rust (cargo)

Recommended: install via rustup.

- Linux/macOS:
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  # then restart your shell or `source $HOME/.cargo/env`
  rustup update
  cargo --version
  ```
- Windows (PowerShell):
  - Download and run: https://win.rustup.rs
  - After install: open a new terminal and run `rustup update` and `cargo --version`

Alternative (macOS):
```bash
brew install rustup
rustup-init
```

## Install pandoc and pdftotext

- macOS (Homebrew):
  ```bash
  brew install pandoc poppler
  ```
- Debian/Ubuntu:
  ```bash
  sudo apt-get update
  sudo apt-get install -y pandoc poppler-utils tar
  ```
- Fedora:
  ```bash
  sudo dnf install -y pandoc poppler-utils tar
  ```
- Arch:
  ```bash
  sudo pacman -S pandoc poppler-utils tar
  ```
- Windows:
  - Chocolatey: `choco install pandoc poppler`
  - Scoop (main bucket): `scoop install pandoc poppler`
  - Manual binaries: https://blog.alivate.com.au/poppler-windows/
  - MSI installers: https://pandoc.org/installing.html

Verify:
```bash
pandoc --version
pdftotext -v
```

## Build and Run

```bash
# from repo root
cargo build
cargo run
# server listens on 0.0.0.0:8080 by default
```

Environment variables:
- `PORT` (default `8080`)
- `MARKXIV_CACHE_CAP` (default `128`) — number of cached papers
- `MARKXIV_INDEX_MD` (default `content/index.md`) — path to landing page Markdown
- `MARKXIV_PANDOC_PATH` (default `pandoc`) — path to pandoc binary
- `MARKXIV_CACHE_DIR` (default `./cache`) — on-disk cache root directory
- `MARKXIV_DISK_CACHE_CAP_BYTES` (default `0`) — on-disk cache size cap in bytes (0 disables disk cache)
- `MARKXIV_SWEEP_INTERVAL_SECS` (default `600`) — background sweeper interval seconds
- `MARKXIV_LOG_PATH` — optional absolute or relative path to the log file; takes precedence over `MARKXIV_LOG_DIR`
- `MARKXIV_LOG_DIR` (default `./logs`) — directory used when `MARKXIV_LOG_PATH` is unset; file name defaults to `markxiv.log`

## Endpoints

- `GET /` → serves landing page from Markdown file
  - Content negotiation: `Accept: text/html` renders Markdown to HTML; `Accept: text/markdown` returns raw Markdown
- `GET /health` → `200 OK`, body `ok`
- `GET /abs/:id[?refresh=1]` → `200 OK` with `text/markdown`
  - `:id` can be a base arXiv id (`1601.00001`) or versioned (`1601.00001v2`)
  - `?refresh=1` bypasses the cache and re-fetches/convert
  - Response is pure Markdown, prefixed by `# {title}` and a `##Abstract` section containing the abstract text
  - Two-tier caching: in-memory LRU first, then on-disk gzip store; cache populated on miss
- `GET /pdf/:id[?refresh=1]` → same response as `/abs/:id`, useful for links that expect the `/pdf/` prefix
  - Requests like `/pdf/:id.pdf` are normalized automatically

Error mapping:
- `404 Not Found` — unknown arXiv id
- `422 Unprocessable Entity` — PDF only (no e-print source) and the `pdftotext` fallback also failed
- `502 Bad Gateway` — upstream/network error contacting arXiv
- `500 Internal Server Error` — conversion/extraction errors

## Development

Run tests (unit + route tests with mocks):
```bash
cargo test
```

Project layout:
- `src/main.rs` — server bootstrap
- `src/routes.rs` — handlers (`/`, `/health`, `/abs/:id`, `/pdf/:id`)
- `src/state.rs` — shared state (LRU cache + clients)
- `src/cache.rs` — thin wrapper around `lru::LruCache`
- `src/arxiv.rs` — arXiv client + metadata fetch via Atom API
- `src/convert.rs` — pandoc-based converter + sanitization
- `src/tex_main.rs` — heuristic for picking the main `.tex` file

### How it works

- Metadata (title, abstract): `https://export.arxiv.org/api/query?id_list=:id` (Atom feed), minimal parse of `<entry><title>` and `<summary>`.
- Source archive: `https://arxiv.org/e-print/:id` (tar/tar.gz). 400/403/404 → treated as PDF-only.
- Conversion: save archive to temp dir → extract with `tar` → pick main `.tex` → `pandoc -f latex -t gfm` → sanitize.
- Fallback: when LaTeX sources are unavailable or pandoc fails, download the PDF and shell out to `pdftotext -raw`.
- Sanitization: remove entire `<figure>...</figure>` blocks and strip all remaining HTML tags from the Markdown output.
- Caching: small in-memory LRU for hot entries, plus an on-disk gzip store with size cap and background sweeper that deletes oldest files when over cap.

## Example usage

```bash
# health
curl -s http://localhost:8080/health

# landing page (HTML by default)
curl -sI http://localhost:8080/ | grep -i content-type

# landing page as raw Markdown
curl -sH 'Accept: text/markdown' http://localhost:8080/

# fetch a paper (replace with a source-available id)
curl -sH 'Accept: text/markdown' http://localhost:8080/abs/1601.00001

# force refresh (bypass cache)
curl -s http://localhost:8080/abs/1601.00001?refresh=1

# enable disk cache with ~10 GB cap
MARKXIV_DISK_CACHE_CAP_BYTES=$((10*1024*1024*1024)) cargo run
```

## MCP Server

markxiv includes an MCP (Model Context Protocol) server that lets Claude and other AI assistants convert arXiv papers to markdown directly using the markxiv library — no web service needed.

**Tools:** `convert_paper`, `get_paper_metadata`, `search_papers`

Quick setup:
```bash
cd mcp
cargo build --release
```

Claude Desktop config (`~/Library/Application Support/Claude/claude_desktop_config.json`):
```json
{
  "mcpServers": {
    "markxiv": {
      "command": "/path/to/target/release/markxiv-mcp"
    }
  }
}
```

See [`mcp/README.md`](mcp/README.md) for full documentation.

## Notes

- Conversion fidelity depends on pandoc and the paper’s LaTeX structure; complex macros/environments may not convert perfectly.
- Title and abstract are prepended to the Markdown as `# Title` and a `##Abstract` heading followed by the abstract.
- HTML is stripped from the final Markdown; embedded PDF figures are removed.
- Caching is in-memory and optional on-disk; restart clears the in-memory cache.
- For production use, consider timeouts, rate limiting, and persistent caching.

## Logging

- By default the server writes structured request/error logs to `./logs/markxiv.log`; ensure the process user can create that directory.
- Override the location with `MARKXIV_LOG_PATH=/abs/path/to/markxiv.log` or `MARKXIV_LOG_DIR=/var/log/markxiv` (generates `/var/log/markxiv/markxiv.log`).
- If the file cannot be opened (e.g., missing write permissions), logging automatically falls back to stderr/journald.
- When running under systemd, confirm the service `User`/`Group` owns or can write to the configured log directory, e.g. `sudo chown -R markxiv:markxiv /var/log/markxiv`.
- Quick check: verify the service account can write to the cache (or any target directory) via `sudo -u markxiv test -w /mnt/markxiv-cache && echo writable`.
