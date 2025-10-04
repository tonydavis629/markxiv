use markxiv::convert::{Converter, PandocConverter};

async fn read_fixture(path: &str) -> Result<Vec<u8>, std::io::Error> {
    tokio::fs::read(path).await
}

#[tokio::test]
async fn latex_tar_to_markdown_uses_real_arxiv_source() {
    let tar_bytes = read_fixture("tests/data/2509.17765.tar")
        .await
        .expect("fixture missing: tests/data/2509.17765.tar");
    let converter = PandocConverter::new();

    let md = converter
        .latex_tar_to_markdown_without_macros(&tar_bytes)
        .await
        .expect("pandoc failed to convert tarball");

    assert!(!md.is_empty(), "markdown output should not be empty");
    assert!(
        md.starts_with("# Introduction"),
        "missing introduction heading"
    );
    assert!(
        md.contains("Qwen3-Omni builds on the Thinker–Talker architecture"),
        "expected multimodal architecture discussion"
    );
    assert!(
        !md.contains("<figure"),
        "sanitization should strip figure blocks"
    );
}

#[tokio::test]
async fn pdf_to_markdown_uses_real_arxiv_pdf() {
    let pdf_bytes = read_fixture("tests/data/2509.14476.pdf")
        .await
        .expect("fixture missing: tests/data/2509.14476.pdf");
    let converter = PandocConverter::new();

    let text = converter
        .pdf_to_markdown(&pdf_bytes)
        .await
        .expect("pdftotext failed to convert pdf");

    assert!(
        text.contains("ATOKEN: A UNIFIED TOKENIZER FOR VISION"),
        "expected paper title in pdftotext output"
    );
    assert!(
        text.contains("We present ATOKEN, the first unified visual tokenizer"),
        "expected abstract content in pdftotext output"
    );
}
