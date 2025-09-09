# markxiv

A minimal web service that mimics arXiv but serves Markdown instead of PDFs/HTML.

Given an arXiv ID, the server:
- Checks a local LRU cache for a converted result
- Fetches the paper’s LaTeX source from arXiv (if available)
- Extracts the archive, picks the main `.tex` file, converts it to Markdown using pandoc
- Returns `text/markdown; charset=utf-8`

If a paper is PDF-only (no source available), the server returns `422 Unprocessable Entity` with body `PDF only`.

> Note: The repository is scaffolded with the full HTTP pipeline and tests using mocks. The real arXiv client and pandoc-based converter are wired but currently stubbed. You can still run the server and tests; conversion will return `501 Not Implemented` until the implementations are completed.

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

## Endpoints

- `GET /` → serves landing page from Markdown file
  - Content negotiation: `Accept: text/html` renders Markdown to HTML; `Accept: text/markdown` returns raw Markdown
- `GET /health` → `200 OK`, body `ok`
- `GET /paper/:id[?refresh=1]` → `200 OK` with `text/markdown`
  - `:id` can be a base arXiv id (`1601.00001`) or versioned (`1601.00001v2`)
  - `?refresh=1` bypasses the cache and re-fetches/convert

Error mapping:
- `404 Not Found` — unknown arXiv id
- `422 Unprocessable Entity` — PDF only (no e-print source)
- `502 Bad Gateway` — upstream/network error contacting arXiv
- `500 Internal Server Error` — conversion/extraction errors
- `501 Not Implemented` — current placeholder until the download/convert is implemented

## Development

Run tests (unit + route tests with mocks):
```bash
cargo test
```

Project layout:
- `src/main.rs` — server bootstrap
- `src/routes.rs` — handlers (`/health`, `/paper/:id`)
- `src/state.rs` — shared state (LRU cache + clients)
- `src/cache.rs` — thin wrapper around `lru::LruCache`
- `src/arxiv.rs` — arXiv client trait, errors, reqwest stub, and test mock
- `src/convert.rs` — converter trait, errors, pandoc stub, and test mock
- `src/tex_main.rs` — heuristic for picking the main `.tex` file

### Implementation plan (next)

- Implement `ArxivClient` using:
  - `exists(id)`: `https://export.arxiv.org/api/query?id_list=:id` → check for an `<entry>` element
  - `get_source_archive(id)`: `https://arxiv.org/e-print/:id` → returns tar-like content if source is available; map missing/unavailable to `PdfOnly`
- Implement `PandocConverter`:
  - Write bytes to a temp dir, extract with `tar -xf`
  - Read `.tex` files, choose main using `select_main_tex`
  - Invoke `pandoc -f latex -t gfm main.tex` and capture stdout

Once implemented, the server will return actual Markdown for source-available arXiv IDs.

## Example usage

```bash
# health
curl -s http://localhost:8080/health

# landing page (HTML by default)
curl -sI http://localhost:8080/ | grep -i content-type

# landing page as raw Markdown
curl -sH 'Accept: text/markdown' http://localhost:8080/

# fetch a paper (replace with a source-available id)
curl -sH 'Accept: text/markdown' http://localhost:8080/paper/1601.00001

# force refresh (bypass cache)
curl -s http://localhost:8080/paper/1601.00001?refresh=1
```

## Notes

- Conversion fidelity depends on pandoc and the paper’s LaTeX structure; complex macros/environments may not convert perfectly.
- Caching is in-memory only; restart clears cache.
- For production use, consider timeouts, rate limiting, and persistent caching.
