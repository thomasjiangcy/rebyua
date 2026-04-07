use std::path::PathBuf;
use std::process::Command;
use std::{fs, path::Path};

use anyhow::{Context, Result, bail};

use crate::cli::ReviewArgs;
use crate::model::{ChangeKind, DiffKind, FilePatch, FileSummary, PatchHunk, PatchLine};

#[derive(Debug, Clone)]
pub struct GitRepo {
    pub root: PathBuf,
    pub base: String,
    pub staged: bool,
    pub pathspecs: Vec<String>,
}

impl GitRepo {
    pub fn discover(args: &ReviewArgs) -> Result<Self> {
        let root = run_git(
            &std::env::current_dir().context("failed to get current directory")?,
            ["rev-parse", "--show-toplevel"].as_slice(),
        )?;

        Ok(Self {
            root: PathBuf::from(root.trim()),
            base: args.base.clone(),
            staged: args.staged,
            pathspecs: args.path.clone(),
        })
    }

    pub fn load_files(&self) -> Result<Vec<FileSummary>> {
        let name_status =
            self.run_diff(["--name-status", "--find-renames", "--no-color"].as_slice())?;
        let numstat = self.run_diff(["--numstat", "--find-renames", "--no-color"].as_slice())?;

        let status_rows = parse_name_status(&name_status);
        let numstat_rows = parse_numstat(&numstat);
        let mut files = Vec::with_capacity(status_rows.len());

        for (idx, (change, old_path, path)) in status_rows.into_iter().enumerate() {
            let (added, deleted) = numstat_rows.get(idx).copied().unwrap_or((None, None));
            files.push(FileSummary {
                path,
                old_path,
                added,
                deleted,
                change,
            });
        }

        Ok(files)
    }

    pub fn load_patch(&self, summary: &FileSummary) -> Result<FilePatch> {
        let patch_text = self.run_diff_for_path(
            ["--no-color", "--find-renames", "--unified=3"].as_slice(),
            &summary.path,
        )?;
        parse_patch(summary.clone(), &patch_text)
    }

    pub fn load_file_text(&self, summary: &FileSummary) -> Result<Option<String>> {
        if matches!(summary.change, ChangeKind::Deleted) {
            return Ok(None);
        }

        if self.staged {
            let spec = format!(":{}", summary.path);
            let output = run_git(&self.root, ["show", "--no-color", spec.as_str()].as_slice())?;
            return Ok(Some(output));
        }

        let path = self.root.join(Path::new(&summary.path));
        match fs::read_to_string(&path) {
            Ok(contents) => Ok(Some(contents)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    fn run_diff(&self, extra_args: &[&str]) -> Result<String> {
        let mut args: Vec<String> = vec!["diff".to_string()];
        args.extend(extra_args.iter().map(|arg| (*arg).to_string()));
        if self.staged {
            args.push("--cached".to_string());
        }
        args.push(self.base.clone());
        if !self.pathspecs.is_empty() {
            args.push("--".to_string());
            args.extend(self.pathspecs.iter().cloned());
        }
        run_git(
            &self.root,
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        )
    }

    fn run_diff_for_path(&self, extra_args: &[&str], path: &str) -> Result<String> {
        let mut args: Vec<String> = vec!["diff".to_string()];
        args.extend(extra_args.iter().map(|arg| (*arg).to_string()));
        if self.staged {
            args.push("--cached".to_string());
        }
        args.push(self.base.clone());
        args.push("--".to_string());
        if !self.pathspecs.is_empty() {
            args.extend(self.pathspecs.iter().cloned());
        }
        args.push(path.to_string());
        run_git(
            &self.root,
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        )
    }
}

fn run_git(root: &std::path::Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_name_status(input: &str) -> Vec<(ChangeKind, Option<String>, String)> {
    input
        .lines()
        .filter_map(|line| {
            if line.trim().is_empty() {
                return None;
            }

            let mut parts = line.split('\t');
            let status = parts.next()?.trim().to_string();
            let kind = parse_change_kind(&status);

            match kind {
                ChangeKind::Renamed | ChangeKind::Copied => {
                    let old_path = parts.next()?.to_string();
                    let new_path = parts.next()?.to_string();
                    Some((kind, Some(old_path), new_path))
                }
                _ => {
                    let path = parts.next()?.to_string();
                    Some((kind, None, path))
                }
            }
        })
        .collect()
}

fn parse_numstat(input: &str) -> Vec<(Option<u64>, Option<u64>)> {
    input
        .lines()
        .filter_map(|line| {
            if line.trim().is_empty() {
                return None;
            }

            let mut parts = line.splitn(3, '\t');
            let added = parse_numstat_field(parts.next()?);
            let deleted = parse_numstat_field(parts.next()?);
            Some((added, deleted))
        })
        .collect()
}

fn parse_numstat_field(field: &str) -> Option<u64> {
    if field == "-" {
        None
    } else {
        field.parse().ok()
    }
}

fn parse_change_kind(status: &str) -> ChangeKind {
    match status.chars().next().unwrap_or('M') {
        'A' => ChangeKind::Added,
        'D' => ChangeKind::Deleted,
        'M' => ChangeKind::Modified,
        'R' => ChangeKind::Renamed,
        'T' => ChangeKind::TypeChange,
        'C' => ChangeKind::Copied,
        other => ChangeKind::Unknown(other.to_string()),
    }
}

fn parse_patch(summary: FileSummary, input: &str) -> Result<FilePatch> {
    let mut hunks = Vec::new();
    let mut metadata = Vec::new();
    let mut current_hunk: Option<PatchHunk> = None;
    let mut old_line = 0usize;
    let mut new_line = 0usize;

    for raw_line in input.lines() {
        if raw_line.starts_with("@@") {
            if let Some(hunk) = current_hunk.take() {
                hunks.push(hunk);
            }

            let (old_start, _old_len, new_start, _new_len) = parse_hunk_header(raw_line)
                .with_context(|| format!("failed to parse hunk header: {raw_line}"))?;
            old_line = old_start;
            new_line = new_start;
            current_hunk = Some(PatchHunk {
                header: raw_line.to_string(),
                new_start,
                lines: Vec::new(),
            });
            continue;
        }

        if let Some(hunk) = current_hunk.as_mut() {
            let (kind, text) = match raw_line.chars().next() {
                Some('+') => (DiffKind::Add, raw_line[1..].to_string()),
                Some('-') => (DiffKind::Delete, raw_line[1..].to_string()),
                Some(' ') => (DiffKind::Context, raw_line[1..].to_string()),
                Some('\\') => continue,
                _ => continue,
            };

            let (old_lineno, new_lineno) = match kind {
                DiffKind::Add => {
                    let new_lineno = Some(new_line);
                    new_line += 1;
                    (None, new_lineno)
                }
                DiffKind::Delete => {
                    let old_lineno = Some(old_line);
                    old_line += 1;
                    (old_lineno, None)
                }
                DiffKind::Context => {
                    let old_lineno = Some(old_line);
                    let new_lineno = Some(new_line);
                    old_line += 1;
                    new_line += 1;
                    (old_lineno, new_lineno)
                }
            };

            hunk.lines.push(PatchLine {
                kind,
                old_lineno,
                new_lineno,
                text,
            });
            continue;
        }

        if should_keep_metadata(raw_line) {
            metadata.push(raw_line.to_string());
        }
    }

    if let Some(hunk) = current_hunk.take() {
        hunks.push(hunk);
    }

    if hunks.is_empty() && metadata.is_empty() {
        match summary.change {
            ChangeKind::Renamed => {
                let old_path = summary.old_path.clone().unwrap_or_default();
                metadata.push(format!("Renamed: {old_path} -> {}", summary.path));
            }
            _ => metadata.push("No textual patch available.".to_string()),
        }
    }

    Ok(FilePatch {
        summary,
        hunks,
        metadata,
    })
}

fn parse_hunk_header(header: &str) -> Result<(usize, usize, usize, usize)> {
    let body = header
        .strip_prefix("@@ ")
        .and_then(|line| line.split(" @@").next())
        .context("missing hunk markers")?;
    let mut parts = body.split(' ');
    let old_part = parts.next().context("missing old hunk part")?;
    let new_part = parts.next().context("missing new hunk part")?;

    let (old_start, old_len) = parse_hunk_range(old_part.trim_start_matches('-'))?;
    let (new_start, new_len) = parse_hunk_range(new_part.trim_start_matches('+'))?;
    Ok((old_start, old_len, new_start, new_len))
}

fn parse_hunk_range(range: &str) -> Result<(usize, usize)> {
    let mut parts = range.split(',');
    let start = parts
        .next()
        .context("missing hunk start")?
        .parse()
        .context("invalid hunk start")?;
    let len = match parts.next() {
        Some(value) => value.parse().context("invalid hunk length")?,
        None => 1,
    };
    Ok((start, len))
}

fn should_keep_metadata(line: &str) -> bool {
    line.starts_with("rename from ")
        || line.starts_with("rename to ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("similarity index ")
        || line.starts_with("dissimilarity index ")
        || line.starts_with("Binary files ")
        || line.starts_with("GIT binary patch")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(path: &str) -> FileSummary {
        FileSummary {
            path: path.to_string(),
            old_path: None,
            added: Some(0),
            deleted: Some(0),
            change: ChangeKind::Modified,
        }
    }

    #[test]
    fn parses_name_status_rows_with_rename() {
        let rows = parse_name_status("M\tsrc/app.rs\nR100\told.rs\tnew.rs\n");

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            (ChangeKind::Modified, None, "src/app.rs".to_string())
        );
        assert_eq!(
            rows[1],
            (
                ChangeKind::Renamed,
                Some("old.rs".to_string()),
                "new.rs".to_string()
            )
        );
    }

    #[test]
    fn parses_numstat_rows_and_binary_markers() {
        let rows = parse_numstat("12\t3\tsrc/app.rs\n-\t-\tassets/logo.png\n");

        assert_eq!(rows, vec![(Some(12), Some(3)), (None, None)]);
    }

    #[test]
    fn parses_patch_hunks_and_line_numbers() {
        let patch = parse_patch(
            summary("src/app.rs"),
            "\
diff --git a/src/app.rs b/src/app.rs
index 1111111..2222222 100644
--- a/src/app.rs
+++ b/src/app.rs
@@ -2,3 +2,4 @@ fn main() {
 context
-removed
+added
 unchanged
}
",
        )
        .expect("patch should parse");

        assert_eq!(patch.hunks.len(), 1);
        let hunk = &patch.hunks[0];
        assert_eq!(hunk.header, "@@ -2,3 +2,4 @@ fn main() {");
        assert_eq!(hunk.new_start, 2);
        assert_eq!(hunk.lines.len(), 4);

        assert_eq!(hunk.lines[0].kind, DiffKind::Context);
        assert_eq!(hunk.lines[0].old_lineno, Some(2));
        assert_eq!(hunk.lines[0].new_lineno, Some(2));

        assert_eq!(hunk.lines[1].kind, DiffKind::Delete);
        assert_eq!(hunk.lines[1].old_lineno, Some(3));
        assert_eq!(hunk.lines[1].new_lineno, None);

        assert_eq!(hunk.lines[2].kind, DiffKind::Add);
        assert_eq!(hunk.lines[2].old_lineno, None);
        assert_eq!(hunk.lines[2].new_lineno, Some(3));
    }

    #[test]
    fn falls_back_to_rename_metadata_without_textual_patch() {
        let patch = parse_patch(
            FileSummary {
                path: "new.rs".to_string(),
                old_path: Some("old.rs".to_string()),
                added: Some(0),
                deleted: Some(0),
                change: ChangeKind::Renamed,
            },
            "",
        )
        .expect("patch should parse");

        assert!(patch.hunks.is_empty());
        assert_eq!(patch.metadata, vec!["Renamed: old.rs -> new.rs"]);
    }
}
