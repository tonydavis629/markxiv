use crate::tex_main::select_main_tex;
use async_trait::async_trait;
use regex::Regex;
use std::{
    path::{Path, PathBuf},
    sync::LazyLock,
    time::Duration,
};
use thiserror::Error;
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Clone, Debug, Error)]
pub enum ConvertError {
    #[error("conversion failed: {0}")]
    Failed(String),
    #[error("not implemented")]
    NotImplemented,
}

#[async_trait]
pub trait Converter {
    async fn latex_tar_to_markdown(&self, _tar_bytes: &[u8]) -> Result<String, ConvertError>;
    async fn latex_tar_to_markdown_without_macros(
        &self,
        tar_bytes: &[u8],
    ) -> Result<String, ConvertError> {
        self.latex_tar_to_markdown(tar_bytes).await
    }
    async fn pdf_to_markdown(&self, _pdf_bytes: &[u8]) -> Result<String, ConvertError>;
}

pub struct PandocConverter;

impl PandocConverter {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Converter for PandocConverter {
    async fn latex_tar_to_markdown(&self, tar_bytes: &[u8]) -> Result<String, ConvertError> {
        self.convert_latex(tar_bytes, PandocLatexMode::Standard)
            .await
    }

    async fn latex_tar_to_markdown_without_macros(
        &self,
        tar_bytes: &[u8],
    ) -> Result<String, ConvertError> {
        self.convert_latex(tar_bytes, PandocLatexMode::NoMacros)
            .await
    }

    async fn pdf_to_markdown(&self, pdf_bytes: &[u8]) -> Result<String, ConvertError> {
        let workdir = make_temp_dir()
            .await
            .map_err(|e| ConvertError::Failed(format!("temp dir: {}", e)))?;
        let pdf_path = workdir.join("source.pdf");

        tokio::fs::write(&pdf_path, pdf_bytes)
            .await
            .map_err(|e| ConvertError::Failed(format!("write pdf: {}", e)))?;

        let pdftotext =
            std::env::var("MARKXIV_PDFTOTEXT_PATH").unwrap_or_else(|_| "pdftotext".into());
        let result = run_pdftotext(&pdftotext, &pdf_path).await;

        cleanup(&workdir).await;

        let text_bytes = result?;
        Ok(String::from_utf8_lossy(&text_bytes).into_owned())
    }
}

impl PandocConverter {
    async fn convert_latex(
        &self,
        tar_bytes: &[u8],
        mode: PandocLatexMode,
    ) -> Result<String, ConvertError> {
        let workdir = make_temp_dir()
            .await
            .map_err(|e| ConvertError::Failed(format!("temp dir: {}", e)))?;
        let tar_path = workdir.join("source.tar");
        // write bytes to disk
        tokio::fs::write(&tar_path, tar_bytes)
            .await
            .map_err(|e| ConvertError::Failed(format!("write tar: {}", e)))?;

        // extract: try plain tar, then gzip
        if let Err(e1) = extract_tar(&workdir, &tar_path, false).await {
            extract_tar(&workdir, &tar_path, true)
                .await
                .map_err(|e2| ConvertError::Failed(format!("extract: {}; fallback: {}", e1, e2)))?;
        }

        // Collect .tex files
        let files = collect_tex_files(&workdir)
            .await
            .map_err(|e| ConvertError::Failed(format!("scan: {}", e)))?;
        let Some(main_tex) = select_main_tex(&files) else {
            cleanup(&workdir).await;
            return Err(ConvertError::Failed("no .tex files found".into()));
        };

        // Run pandoc
        let pandoc = std::env::var("MARKXIV_PANDOC_PATH").unwrap_or_else(|_| "pandoc".into());
        let main_parent = main_tex.parent().unwrap_or(Path::new(&workdir));
        let main_file = main_tex
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| ConvertError::Failed("invalid main tex path".into()))?;
        let md_bytes = run_pandoc(&pandoc, main_parent, main_file, mode).await?;

        // cleanup best-effort
        cleanup(&workdir).await;

        let mut md = String::from_utf8_lossy(&md_bytes).into_owned();
        md = sanitize_markdown(&md);
        Ok(md)
    }
}

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
static COUNTER: AtomicU64 = AtomicU64::new(0);

async fn make_temp_dir() -> io::Result<PathBuf> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let base = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..5 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("markxiv-{}-{}-{}", pid, nanos, n));
        match tokio::fs::create_dir(&dir).await {
            Ok(_) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to create unique temp dir",
    ))
}

async fn extract_tar(workdir: &Path, tar_path: &Path, gzip: bool) -> io::Result<()> {
    let mut cmd = Command::new("tar");
    cmd.current_dir(workdir);
    if gzip {
        cmd.args(["-x", "-z", "-f"])
            .arg(tar_path)
            .args(["-C"])
            .arg(workdir);
    } else {
        cmd.args(["-x", "-f"])
            .arg(tar_path)
            .args(["-C"])
            .arg(workdir);
    }
    let out = timeout(Duration::from_secs(60), cmd.output())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "tar timed out"))??;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(io::Error::new(
            io::ErrorKind::Other,
            format!("tar failed: {}", stderr),
        ))
    }
}

async fn collect_tex_files(root: &Path) -> io::Result<Vec<(PathBuf, String)>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut rd = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            let ft = entry.file_type().await?;
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                if path.extension().map(|e| e == "tex").unwrap_or(false) {
                    match tokio::fs::read_to_string(&path).await {
                        Ok(s) => out.push((path, s)),
                        Err(_) => continue,
                    }
                }
            }
        }
    }
    Ok(out)
}

#[derive(Clone, Copy)]
enum PandocLatexMode {
    Standard,
    NoMacros,
}

const PANDOC_TIMEOUT: Duration = Duration::from_secs(5);

async fn run_pandoc(
    pandoc: &str,
    cwd: &Path,
    main_file: &str,
    mode: PandocLatexMode,
) -> Result<Vec<u8>, ConvertError> {
    let mut cmd = Command::new(pandoc);
    let format_arg = match mode {
        PandocLatexMode::Standard => "latex",
        PandocLatexMode::NoMacros => "latex-latex_macros",
    };
    cmd.current_dir(cwd)
        .arg("-f")
        .arg(format_arg)
        .arg("-t")
        .arg("gfm")
        .arg(main_file);
    let out = timeout(PANDOC_TIMEOUT, cmd.output())
        .await
        .map_err(|_| ConvertError::Failed("pandoc timed out".into()))
        .and_then(|r| r.map_err(|e| ConvertError::Failed(format!("pandoc spawn: {}", e))))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(ConvertError::Failed(format!("pandoc failed: {}", stderr)))
    }
}

async fn cleanup(path: &Path) {
    let _ = tokio::fs::remove_dir_all(path).await;
}

async fn run_pdftotext(pdftotext: &str, pdf_path: &Path) -> Result<Vec<u8>, ConvertError> {
    let mut cmd = Command::new(pdftotext);
    cmd.arg("-raw").arg(pdf_path).arg("-");
    let out = timeout(Duration::from_secs(300), cmd.output())
        .await
        .map_err(|_| ConvertError::Failed("pdftotext timed out".into()))
        .and_then(|r| r.map_err(|e| ConvertError::Failed(format!("pdftotext spawn: {}", e))))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(ConvertError::Failed(format!(
            "pdftotext failed: {}",
            stderr
        )))
    }
}

fn fix_katex_commands(input: &str) -> String {
    static RE_MATHCAL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(\\mathcal\{[^}]*\})\{").unwrap());
    static RE_TEXTSC: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\\textsc\{([^}]*)\}").unwrap());
    static RE_CALL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\\Call\{([^}]*)\}\{([^}]*)\}").unwrap());
    static RE_MATHBBM: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\\mathbbm\{([^}]*)\}").unwrap());

    let s = RE_MATHCAL.replace_all(input, "${1}_{");
    let s = RE_TEXTSC.replace_all(&s, r"\textbf{$1}");
    let s = RE_CALL.replace_all(&s, r"\textbf{$1}($2)");
    let s = RE_MATHBBM.replace_all(&s, r"\mathbb{$1}");
    s.into_owned()
}

fn sanitize_markdown(input: &str) -> String {
    // 1) Remove entire <figure ...>...</figure> blocks (with embedded pdfs)
    let mut out = input.to_string();
    loop {
        let Some(start) = out.find("<figure") else {
            break;
        };
        if let Some(rel_end) = out[start..].find("</figure>") {
            let end = start + rel_end + "</figure>".len();
            out.replace_range(start..end, "");
        } else {
            // No closing tag; remove from start to next block break or end
            if let Some(rel_end) = out[start..].find("\n\n") {
                let end = start + rel_end;
                out.replace_range(start..end, "");
            } else {
                out.truncate(start);
                break;
            }
        }
    }
    // 2) Fix KaTeX commands that are unsupported or malformed
    out = fix_katex_commands(&out);
    // 3) Strip HTML tags but preserve math blocks ($...$ and $$...$$) verbatim
    strip_html_tags_preserve_math(out.trim_start())
}

/// Strip HTML tags from text while preserving math blocks verbatim.
///
/// Content inside `$...$` and `$$...$$` is copied as-is so that `<` and `>`
/// in math expressions (e.g. `\texttt{<name>}`, comparisons) survive intact.
fn strip_html_tags_preserve_math(input: &str) -> String {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0;

    while i < len {
        if i + 1 < len && bytes[i] == b'$' && bytes[i + 1] == b'$' {
            // Display math $$...$$ — copy verbatim
            out.push_str("$$");
            i += 2;
            while i + 1 < len {
                if bytes[i] == b'$' && bytes[i + 1] == b'$' {
                    break;
                }
                let ch = input[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
            if i + 1 < len && bytes[i] == b'$' && bytes[i + 1] == b'$' {
                out.push_str("$$");
                i += 2;
            }
        } else if bytes[i] == b'$' {
            // Inline math $...$ — copy verbatim
            out.push('$');
            i += 1;
            while i < len && bytes[i] != b'$' {
                let ch = input[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
            if i < len && bytes[i] == b'$' {
                out.push('$');
                i += 1;
            }
        } else if bytes[i] == b'<' {
            // Potential HTML tag — skip <...>
            i += 1;
            let mut found_close = false;
            while i < len {
                if bytes[i] == b'>' {
                    found_close = true;
                    i += 1;
                    break;
                }
                i += 1;
            }
            if !found_close {
                // Unclosed `<` at end of input — drop it
            }
        } else if bytes[i] == b'>' {
            // Stray > outside a tag — keep it
            out.push('>');
            i += 1;
        } else {
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

#[cfg(test)]
mod sanitize_tests {
    use super::{fix_katex_commands, sanitize_markdown, strip_html_tags_preserve_math};

    #[test]
    fn removes_figure_block() {
        let s = "<figure id=\"fig:concept\">\n<embed src=\"figures/latent_cot.pdf\"/>\n<figcaption>text</figcaption>\n</figure>\n\n# Title\nBody";
        let out = sanitize_markdown(s);
        assert!(out.starts_with("# Title"));
        assert!(!out.contains("<figure"));
    }

    #[test]
    fn removes_trailing_html_tags() {
        let s = "<p>Hello <strong>world</strong></p>";
        let out = sanitize_markdown(s);
        assert_eq!(out, "Hello world");
    }

    #[test]
    fn strip_html_preserves_inner_text() {
        let s = "<span class=\"note\">Note</span>: <em>important</em>";
        assert_eq!(strip_html_tags_preserve_math(s), "Note: important");
    }

    #[test]
    fn fixes_mathcal_missing_subscript() {
        let input = r"$\mathcal{X}{Y}$";
        let out = fix_katex_commands(input);
        assert_eq!(out, r"$\mathcal{X}_{Y}$");
    }

    #[test]
    fn fixes_textsc_to_textbf() {
        let input = r"$\textsc{Algorithm}$";
        let out = fix_katex_commands(input);
        assert_eq!(out, r"$\textbf{Algorithm}$");
    }

    #[test]
    fn fixes_call_macro() {
        let input = r"$\Call{Solve}{x, y}$";
        let out = fix_katex_commands(input);
        assert_eq!(out, r"$\textbf{Solve}(x, y)$");
    }

    #[test]
    fn fixes_mathbbm_to_mathbb() {
        let input = r"$\mathbbm{1}$";
        let out = fix_katex_commands(input);
        assert_eq!(out, r"$\mathbb{1}$");
    }

    #[test]
    fn preserves_angle_brackets_in_inline_math() {
        let input = r"text $a < b$ more text";
        let out = strip_html_tags_preserve_math(input);
        assert_eq!(out, r"text $a < b$ more text");
    }

    #[test]
    fn preserves_angle_brackets_in_display_math() {
        let input = r"text $$a < b > c$$ more";
        let out = strip_html_tags_preserve_math(input);
        assert_eq!(out, r"text $$a < b > c$$ more");
    }

    #[test]
    fn preserves_texttt_with_angle_brackets_in_math() {
        let input = r"$$\texttt{<functional\_area> / <category>}$$";
        let out = strip_html_tags_preserve_math(input);
        assert_eq!(out, input);
    }

    #[test]
    fn strips_html_outside_math_preserves_math() {
        let input = "text <em>bold</em> more $a < b$ end <p>para</p>";
        let out = strip_html_tags_preserve_math(input);
        assert_eq!(out, "text bold more $a < b$ end para");
    }

    #[test]
    fn no_false_positive_on_frac() {
        let input = r"$\frac{a}{b}$";
        let out = fix_katex_commands(input);
        assert_eq!(out, input);
    }

    #[test]
    fn html_tags_outside_math_stripped() {
        let input = "text <em>hello</em> more";
        let out = strip_html_tags_preserve_math(input);
        assert_eq!(out, "text hello more");
    }
}

pub mod test_helpers {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    pub struct MockConverter {
        pub latex_result: Result<String, ConvertError>,
        pub latex_nomacro_result: Option<Result<String, ConvertError>>,
        pub pdf_result: Result<String, ConvertError>,
        pub latex_calls: Arc<AtomicUsize>,
        pub latex_nomacro_calls: Arc<AtomicUsize>,
        pub pdf_calls: Arc<AtomicUsize>,
    }

    impl MockConverter {
        pub fn new(
            latex_result: Result<String, ConvertError>,
            pdf_result: Result<String, ConvertError>,
        ) -> Self {
            Self {
                latex_result,
                latex_nomacro_result: None,
                pdf_result,
                latex_calls: Arc::new(AtomicUsize::new(0)),
                latex_nomacro_calls: Arc::new(AtomicUsize::new(0)),
                pdf_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl Converter for MockConverter {
        async fn latex_tar_to_markdown(&self, _tar_bytes: &[u8]) -> Result<String, ConvertError> {
            self.latex_calls.fetch_add(1, Ordering::SeqCst);
            self.latex_result.clone()
        }

        async fn latex_tar_to_markdown_without_macros(
            &self,
            _tar_bytes: &[u8],
        ) -> Result<String, ConvertError> {
            self.latex_nomacro_calls.fetch_add(1, Ordering::SeqCst);
            self.latex_nomacro_result
                .clone()
                .unwrap_or_else(|| self.latex_result.clone())
        }

        async fn pdf_to_markdown(&self, _pdf_bytes: &[u8]) -> Result<String, ConvertError> {
            self.pdf_calls.fetch_add(1, Ordering::SeqCst);
            self.pdf_result.clone()
        }
    }
}
