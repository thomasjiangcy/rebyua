use crate::model::{Annotation, FileSummary, LineReference};

pub fn markdown(base: &str, files: &[FileSummary], annotations: &[Annotation]) -> String {
    let mut out = String::new();
    out.push_str(&format!("- Base: `{base}`\n"));
    out.push_str(&format!("- Comments: {}\n\n", annotations.len()));

    if annotations.is_empty() {
        out.push_str("No comments.\n");
        return out;
    }

    for file in files {
        let file_annotations: Vec<_> = annotations
            .iter()
            .filter(|annotation| annotation.file_path == file.path)
            .collect();

        if file_annotations.is_empty() {
            continue;
        }

        out.push_str(&format!("## {}\n\n", file.path));
        for annotation in file_annotations {
            out.push_str("- ");
            out.push_str(&range_label(annotation));
            if let Some(hunk_header) = annotation
                .hunk_header
                .as_ref()
                .filter(|_| !annotation.is_file_level())
            {
                out.push_str(&format!(" (`{}`)", trim_hunk_header(hunk_header)));
            }
            out.push_str(": ");
            out.push_str(annotation.body.trim());
            out.push('\n');
        }
        out.push('\n');
    }

    out
}

fn range_label(annotation: &Annotation) -> String {
    let Some((start_ref, end_ref)) = annotation.line_refs() else {
        return "file".to_string();
    };

    let old = range_part("old", start_ref, end_ref, true);
    let new = range_part("new", start_ref, end_ref, false);

    match (old, new) {
        (Some(old), Some(new)) => format!("{old}; {new}"),
        (Some(old), None) => old,
        (None, Some(new)) => new,
        (None, None) => annotation
            .line_range()
            .map(|(start_line_idx, end_line_idx)| {
                format!("lines {}-{}", start_line_idx + 1, end_line_idx + 1)
            })
            .unwrap_or_else(|| "file".to_string()),
    }
}

fn range_part(
    label: &str,
    start: &LineReference,
    end: &LineReference,
    use_old: bool,
) -> Option<String> {
    let start_value = if use_old {
        start.old_lineno
    } else {
        start.new_lineno
    }?;
    let end_value = if use_old {
        end.old_lineno
    } else {
        end.new_lineno
    }
    .unwrap_or(start_value);

    if start_value == end_value {
        Some(format!("{label} {start_value}"))
    } else {
        Some(format!("{label} {start_value}-{end_value}"))
    }
}

fn trim_hunk_header(header: &str) -> &str {
    header.trim().trim_matches('@').trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Annotation, AnnotationLineRange, ChangeKind, FileSummary};

    fn file(path: &str) -> FileSummary {
        FileSummary {
            path: path.to_string(),
            old_path: None,
            added: Some(0),
            deleted: Some(0),
            change: ChangeKind::Modified,
        }
    }

    #[test]
    fn renders_empty_review_markdown() {
        let output = markdown("HEAD", &[file("src/app.rs")], &[]);

        assert!(!output.contains("# rebyua review"));
        assert!(output.contains("- Base: `HEAD`"));
        assert!(output.contains("- Comments: 0"));
        assert!(output.contains("No comments."));
    }

    #[test]
    fn renders_file_and_line_comments_grouped_by_file() {
        let files = vec![file("src/app.rs"), file("src/cli.rs")];
        let annotations = vec![
            Annotation::created_for_file(1, "src/app.rs".to_string(), "Needs more context".into()),
            Annotation::created_for_lines(
                2,
                "src/app.rs".to_string(),
                Some("@@ -10,2 +10,3 @@ fn run() {".to_string()),
                AnnotationLineRange {
                    start_line_idx: 4,
                    end_line_idx: 5,
                    start_ref: LineReference {
                        old_lineno: Some(10),
                        new_lineno: Some(10),
                    },
                    end_ref: LineReference {
                        old_lineno: Some(11),
                        new_lineno: Some(12),
                    },
                },
                "Split this branch".into(),
            ),
            Annotation::created_for_lines(
                3,
                "src/cli.rs".to_string(),
                None,
                AnnotationLineRange {
                    start_line_idx: 0,
                    end_line_idx: 0,
                    start_ref: LineReference {
                        old_lineno: None,
                        new_lineno: Some(1),
                    },
                    end_ref: LineReference {
                        old_lineno: None,
                        new_lineno: Some(1),
                    },
                },
                "Looks good".into(),
            ),
        ];

        let output = markdown("HEAD~1", &files, &annotations);

        assert!(output.contains("## src/app.rs"));
        assert!(output.contains("- file: Needs more context"));
        assert!(
            output.contains(
                "- old 10-11; new 10-12 (`-10,2 +10,3 @@ fn run() {`): Split this branch"
            )
        );
        assert!(output.contains("## src/cli.rs"));
        assert!(output.contains("- new 1: Looks good"));
    }
}
