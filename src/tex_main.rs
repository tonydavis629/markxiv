use std::path::PathBuf;

// Heuristic: choose main .tex among (path, contents)
pub fn select_main_tex(files: &[(PathBuf, String)]) -> Option<PathBuf> {
    if files.is_empty() {
        return None;
    }
    let mut tex_files: Vec<(PathBuf, String)> = files
        .iter()
        .filter(|(p, _)| p.extension().map(|e| e == "tex").unwrap_or(false))
        .map(|(p, c)| (p.clone(), c.clone()))
        .collect();
    if tex_files.is_empty() {
        return None;
    }
    if tex_files.len() == 1 {
        return Some(tex_files.remove(0).0);
    }
    // Prefer files containing \documentclass and not matching supplementary pattern
    tex_files.sort_by_key(|(p, c)| {
        let has_dc = c.contains("\\documentclass");
        let is_supp = is_supplementary_name(p);
        (
            !has_dc,                    // prefer has documentclass
            is_supp,                    // avoid supplementary
            std::cmp::Reverse(c.len()), // longer content last key to reverse preference
        )
    });
    tex_files.into_iter().next().map(|(p, _)| p)
}

fn is_supplementary_name(p: &PathBuf) -> bool {
    let name = p
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let bad = ["supp", "supplement", "appendix", "si", "supplementary"];
    bad.iter().any(|b| name.contains(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_single_tex() {
        let files = vec![(
            PathBuf::from("main.tex"),
            String::from("\\documentclass{article}"),
        )];
        let pick = select_main_tex(&files).unwrap();
        assert_eq!(pick, PathBuf::from("main.tex"));
    }

    #[test]
    fn picks_with_documentclass_over_supplement() {
        let files = vec![
            (PathBuf::from("supp.tex"), String::from("some appendix")),
            (
                PathBuf::from("paper.tex"),
                String::from("% preamble\n\\documentclass{article}\n\\begin{document}"),
            ),
        ];
        let pick = select_main_tex(&files).unwrap();
        assert_eq!(pick, PathBuf::from("paper.tex"));
    }

    #[test]
    fn avoids_supplementary_names() {
        let files = vec![
            (
                PathBuf::from("appendix.tex"),
                String::from("\\documentclass{article}"),
            ),
            (
                PathBuf::from("main.tex"),
                String::from("\\documentclass{article}"),
            ),
        ];
        let pick = select_main_tex(&files).unwrap();
        assert_eq!(pick, PathBuf::from("main.tex"));
    }

    #[test]
    fn returns_none_when_no_tex() {
        let files = vec![(PathBuf::from("readme.md"), String::from("hello"))];
        assert!(select_main_tex(&files).is_none());
    }
}
