# markxiv.org

Markdown formatted arxiv papers

## How it works
Simply replace the 'arxiv' in an arXiv URL with 'markxiv' to get the Markdown version

### `arxiv.org/abs/1234.56789` `==>` `markxiv.org/abs/1234.56789`

## Free and [Open Source](https://github.com/tonydavis629/markxiv)

## MCP Server

Use markxiv as an MCP tool in any MCP client to allow agents to convert arXiv papers to markdown directly.

### Install

```bash
# requires pandoc and poppler-utils (pdftotext)
cargo install --git https://github.com/tonydavis629/markxiv --path mcp
```

### Configure

Add to your MCP config:

```json
{
  "mcpServers": {
    "markxiv": {
      "command": "markxiv-mcp"
    }
  }
}
```

### Tools

- **convert_paper** — convert an arXiv paper to markdown (e.g. `paper_id: "1706.03762"`)
- **get_paper_metadata** — get title, authors, and abstract
- **search_papers** — search arXiv by keyword

See [mcp/README.md](https://github.com/tonydavis629/markxiv/tree/main/mcp) for full documentation.

## Support
- [GitHub Sponsors](https://github.com/sponsors/tonydavis629)
- Bitcoin: `bc1qskdeeraajt7lvzd0vr99ny7lw33wafcujgf0rj`
- Monero: `846uyz3sTjfPYBTJko7NsFQQveX5nB1L3jQ2btV1shiRQ6HBDJ84Pxg25VdNz1PULMR7EUsHDMebuB8KHCd3FNov75FR79y`

Made with ❤️ by [Tony Davis](https://tonyd.co)
