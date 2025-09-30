use crate::tex_main::select_main_tex;
use async_trait::async_trait;
use std::{
    path::{Path, PathBuf},
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
        let md_bytes = run_pandoc(&pandoc, main_parent, main_file).await?;

        // cleanup best-effort
        cleanup(&workdir).await;

        let mut md = String::from_utf8_lossy(&md_bytes).into_owned();
        md = sanitize_markdown(&md);
        Ok(md)
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

async fn run_pandoc(pandoc: &str, cwd: &Path, main_file: &str) -> Result<Vec<u8>, ConvertError> {
    let mut cmd = Command::new(pandoc);
    cmd.current_dir(cwd)
        .arg("-f")
        .arg("latex")
        .arg("-t")
        .arg("gfm")
        .arg(main_file);
    let out = timeout(Duration::from_secs(120), cmd.output())
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
    // 2) Strip any remaining HTML tags but keep their inner text
    strip_html_tags(out.trim_start())
}

fn strip_html_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                if !in_tag {
                    out.push('>');
                } else {
                    in_tag = false;
                }
            }
            _ => {
                if !in_tag {
                    out.push(ch)
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod sanitize_tests {
    use super::sanitize_markdown;

    #[test]
    fn removes_figure_block() {
        let s = "<figure id=\"fig:concept\">\n<embed src=\"figures/latent_cot.pdf\"/>\n<figcaption>text</figcaption>\n</figure>\n\n# Title\nBody";
        let out = sanitize_markdown(s);
        assert!(out.starts_with("# Title"));
        assert!(!out.contains("<figure"));
    }
}

#[cfg(test)]
pub mod test_helpers {
    use super::*;

    pub struct MockConverter {
        pub latex_result: Result<String, ConvertError>,
        pub pdf_result: Result<String, ConvertError>,
    }

    impl MockConverter {
        pub fn new(
            latex_result: Result<String, ConvertError>,
            pdf_result: Result<String, ConvertError>,
        ) -> Self {
            Self {
                latex_result,
                pdf_result,
            }
        }
    }

    #[async_trait]
    impl Converter for MockConverter {
        async fn latex_tar_to_markdown(&self, _tar_bytes: &[u8]) -> Result<String, ConvertError> {
            self.latex_result.clone()
        }

        async fn pdf_to_markdown(&self, _pdf_bytes: &[u8]) -> Result<String, ConvertError> {
            self.pdf_result.clone()
        }
    }
}
