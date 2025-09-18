# markxiv

Just replace 'arxiv' in the URL with 'markxiv' to get the markdown

https://arxiv.org/abs/1706.03762 → https://markxiv.org/abs/1706.03762

## Basics

A minimal web service that mimics arXiv but serves Markdown instead of PDFs/HTML.

Given an arXiv ID, the server:
- Checks a local LRU cache for a converted result
- Fetches the paper’s LaTeX source from arXiv (if available)
- Extracts the archive, picks the main `.tex` file, converts it to Markdown using pandoc
- Returns `text/markdown; charset=utf-8`

If a paper is PDF-only (no source available), the server returns `422 Unprocessable Entity` with body `PDF only`.

Returned Markdown includes the paper title and abstract prepended at the top.

## Requirements

- Rust toolchain (`cargo`, `rustc`) via rustup
- pandoc (for LaTeX → Markdown conversion)
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

## Install pandoc

- macOS (Homebrew):
  ```bash
  brew install pandoc
  ```
- Debian/Ubuntu:
  ```bash
  sudo apt-get update
  sudo apt-get install -y pandoc tar
  ```
- Fedora:
  ```bash
  sudo dnf install -y pandoc tar
  ```
- Arch:
  ```bash
  sudo pacman -S pandoc tar
  ```
- Windows:
  - Chocolatey: `choco install pandoc`
  - Scoop: `scoop install pandoc`
  - MSI installers: https://pandoc.org/installing.html

Verify:
```bash
pandoc --version
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

## Endpoints

- `GET /` → serves landing page from Markdown file
  - Content negotiation: `Accept: text/html` renders Markdown to HTML; `Accept: text/markdown` returns raw Markdown
- `GET /health` → `200 OK`, body `ok`
- `GET /abs/:id[?refresh=1]` → `200 OK` with `text/markdown`
  - `:id` can be a base arXiv id (`1601.00001`) or versioned (`1601.00001v2`)
  - `?refresh=1` bypasses the cache and re-fetches/convert
  - Response is pure Markdown, prefixed by `# {title}` and a `##Abstract` section containing the abstract text
  - Two-tier caching: in-memory LRU first, then on-disk gzip store; cache populated on miss

Error mapping:
- `404 Not Found` — unknown arXiv id
- `422 Unprocessable Entity` — PDF only (no e-print source)
- `502 Bad Gateway` — upstream/network error contacting arXiv
- `500 Internal Server Error` — conversion/extraction errors

## Development

Run tests (unit + route tests with mocks):
```bash
cargo test
```

Project layout:
- `src/main.rs` — server bootstrap
- `src/routes.rs` — handlers (`/`, `/health`, `/abs/:id`)
- `src/state.rs` — shared state (LRU cache + clients)
- `src/cache.rs` — thin wrapper around `lru::LruCache`
- `src/arxiv.rs` — arXiv client + metadata fetch via Atom API
- `src/convert.rs` — pandoc-based converter + sanitization
- `src/tex_main.rs` — heuristic for picking the main `.tex` file

### How it works

- Metadata (title, abstract): `https://export.arxiv.org/api/query?id_list=:id` (Atom feed), minimal parse of `<entry><title>` and `<summary>`.
- Source archive: `https://arxiv.org/e-print/:id` (tar/tar.gz). 400/403/404 → treated as PDF-only.
- Conversion: save archive to temp dir → extract with `tar` → pick main `.tex` → `pandoc -f latex -t gfm` → sanitize.
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

## Notes

- Conversion fidelity depends on pandoc and the paper’s LaTeX structure; complex macros/environments may not convert perfectly.
- Title and abstract are prepended to the Markdown as `# Title` and a `##Abstract` heading followed by the abstract.
- HTML is stripped from the final Markdown; embedded PDF figures are removed.
- Caching is in-memory only; restart clears cache.
- For production use, consider timeouts, rate limiting, and persistent caching.
