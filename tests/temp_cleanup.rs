//! Failed conversions must not leak scratch dirs: each one used to leave an
//! extracted source tree in the temp dir until the disk filled (ENOSPC).
//! Shells out to real `tar`, so it needs only coreutils + tar, not pandoc.

use std::path::Path;
use std::time::Duration;

use markxiv::convert::{sweep_stale_temp_dirs, Converter, PandocConverter};

fn markxiv_dirs(base: &Path) -> Vec<String> {
    std::fs::read_dir(base)
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("markxiv-"))
        .collect()
}

// One test so the process-wide TMPDIR / MARKXIV_PANDOC_PATH are set once.
#[tokio::test]
async fn conversions_and_sweeps_leave_no_temp_dirs() {
    let base = std::env::temp_dir().join(format!("markxiv-test-root-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    std::env::set_var("TMPDIR", &base);
    std::env::set_var("MARKXIV_PANDOC_PATH", "/nonexistent/pandoc");
    let conv = PandocConverter::new();

    // Not an archive: extraction fails.
    assert!(conv.latex_tar_to_markdown(b"not a tarball").await.is_err());
    assert_eq!(
        markxiv_dirs(&base),
        Vec::<String>::new(),
        "after bad archive"
    );

    // Real source archive, but pandoc cannot run: fails after extraction.
    let tar = std::fs::read("tests/data/2509.17765.tar").unwrap();
    assert!(conv.latex_tar_to_markdown(&tar).await.is_err());
    assert_eq!(
        markxiv_dirs(&base),
        Vec::<String>::new(),
        "after pandoc failure"
    );

    // pdftotext missing or failing on junk: still cleaned up.
    let _ = conv.pdf_to_markdown(b"%PDF-junk").await;
    assert_eq!(
        markxiv_dirs(&base),
        Vec::<String>::new(),
        "after pdf attempt"
    );

    // A crashed process's dir is swept; an unrelated dir is not.
    std::fs::create_dir_all(base.join("markxiv-1-2-3/sub")).unwrap();
    std::fs::write(base.join("markxiv-1-2-3/sub/source.tar"), b"x").unwrap();
    std::fs::create_dir_all(base.join("other")).unwrap();
    assert_eq!(sweep_stale_temp_dirs(Duration::from_secs(900)), 1);
    assert_eq!(markxiv_dirs(&base), Vec::<String>::new(), "after sweep");
    assert!(base.join("other").exists());

    let _ = std::fs::remove_dir_all(&base);
}
