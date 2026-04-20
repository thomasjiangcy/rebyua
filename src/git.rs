use std::path::PathBuf;
use std::process::Command;
use std::{fs, path::Path};

use anyhow::{Context, Result, bail};

use crate::cli::ReviewArgs;
use crate::model::{
    ChangeKind, DiffKind, FilePatch, FileSummary, PatchHunk, PatchLine, ReviewEdge,
};

#[derive(Debug, Clone)]
pub struct ResolvedReview {
    pub repo: GitRepo,
    pub stack: Option<StackReview>,
}

#[derive(Debug, Clone)]
pub struct StackReview {
    pub base_branch: String,
    pub leaf_branch: String,
    pub chain: Vec<String>,
    pub edges: Vec<ReviewEdge>,
}

#[derive(Debug, Clone)]
pub struct GitRepo {
    pub root: PathBuf,
    pub base: String,
    pub head: Option<String>,
    pub staged: bool,
    pub pathspecs: Vec<String>,
}

impl ResolvedReview {
    pub fn discover(args: &ReviewArgs) -> Result<Self> {
        let root = PathBuf::from(
            run_git(
                &std::env::current_dir().context("failed to get current directory")?,
                ["rev-parse", "--show-toplevel"].as_slice(),
            )?
            .trim(),
        );
        Self::discover_in_root(root, args)
    }

    fn discover_in_root(root: PathBuf, args: &ReviewArgs) -> Result<Self> {
        if let Some(leaf_branch) = &args.stack {
            let base_branch = resolve_stack_base(&root, &args.base)?;
            let stack = resolve_stack_review(&root, leaf_branch, &base_branch)?;
            let current_edge = stack
                .edges
                .first()
                .cloned()
                .context("resolved stack contains no review edges")?;

            return Ok(Self {
                repo: GitRepo::for_edge(root, current_edge, args.path.clone()),
                stack: Some(stack),
            });
        }

        Ok(Self {
            repo: GitRepo::for_worktree(root, args.base.clone(), args.staged, args.path.clone()),
            stack: None,
        })
    }
}

impl GitRepo {
    pub fn for_worktree(root: PathBuf, base: String, staged: bool, pathspecs: Vec<String>) -> Self {
        Self {
            root,
            base,
            head: None,
            staged,
            pathspecs,
        }
    }

    pub fn for_edge(root: PathBuf, edge: ReviewEdge, pathspecs: Vec<String>) -> Self {
        Self {
            root,
            base: edge.base,
            head: Some(edge.head),
            staged: false,
            pathspecs,
        }
    }

    pub fn current_edge(&self) -> Option<ReviewEdge> {
        self.head.as_ref().map(|head| ReviewEdge {
            base: self.base.clone(),
            head: head.clone(),
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

        if let Some(head) = &self.head {
            let spec = format!("{head}:{}", summary.path);
            let output = run_git(&self.root, ["show", "--no-color", spec.as_str()].as_slice())?;
            return Ok(Some(output));
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
        if self.head.is_none() && self.staged {
            args.push("--cached".to_string());
        }
        args.push(self.diff_target());
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
        if self.head.is_none() && self.staged {
            args.push("--cached".to_string());
        }
        args.push(self.diff_target());
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

    fn diff_target(&self) -> String {
        match &self.head {
            Some(head) => format!("{}...{}", self.base, head),
            None => self.base.clone(),
        }
    }
}

fn resolve_stack_base(root: &Path, requested_base: &str) -> Result<String> {
    if requested_base != "HEAD" {
        ensure_ref_exists(root, requested_base)?;
        return Ok(requested_base.to_string());
    }

    if let Some(default_branch) = resolve_default_branch(root)? {
        return Ok(default_branch);
    }

    bail!("failed to resolve default base branch; pass --base explicitly");
}

fn resolve_default_branch(root: &Path) -> Result<Option<String>> {
    if let Ok(symbolic_ref) = run_git(
        root,
        ["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"].as_slice(),
    ) && let Some(branch) = symbolic_ref.trim().rsplit('/').next()
        && local_branch_exists(root, branch)?
    {
        return Ok(Some(branch.to_string()));
    }

    for fallback in ["main", "master"] {
        if local_branch_exists(root, fallback)? {
            return Ok(Some(fallback.to_string()));
        }
    }

    Ok(None)
}

fn resolve_stack_review(root: &Path, leaf_branch: &str, base_branch: &str) -> Result<StackReview> {
    ensure_local_branch(root, leaf_branch)?;
    ensure_ref_exists(root, base_branch)?;

    let mut current = leaf_branch.to_string();
    let mut reversed_chain = vec![current.clone()];
    let mut reversed_edges = Vec::new();

    while current != base_branch {
        let parent = infer_parent_branch(root, &current, base_branch, &reversed_chain)?;
        reversed_edges.push(ReviewEdge {
            base: parent.clone(),
            head: current.clone(),
        });
        current = parent;
        if reversed_chain.contains(&current) {
            bail!("detected a loop while resolving stack at branch {current}");
        }
        reversed_chain.push(current.clone());
    }

    reversed_chain.reverse();
    reversed_edges.reverse();

    if reversed_edges.is_empty() {
        bail!("stack leaf {leaf_branch} is already at base {base_branch}");
    }

    Ok(StackReview {
        base_branch: base_branch.to_string(),
        leaf_branch: leaf_branch.to_string(),
        chain: reversed_chain,
        edges: reversed_edges,
    })
}

fn infer_parent_branch(
    root: &Path,
    head_branch: &str,
    base_branch: &str,
    visited: &[String],
) -> Result<String> {
    let head_sha = rev_parse(root, head_branch)?;
    let base_branch_alias = base_branch
        .rsplit('/')
        .next()
        .filter(|alias| *alias != base_branch);
    let mut scored = Vec::new();
    for candidate in local_branches(root)? {
        if candidate == head_branch {
            continue;
        }

        if visited.iter().any(|branch| branch == &candidate) {
            continue;
        }

        let candidate_sha = rev_parse(root, &candidate)?;
        if base_branch_alias.is_some_and(|alias| alias == candidate) {
            continue;
        }
        if candidate_sha != head_sha && is_ancestor(root, head_branch, &candidate)? {
            continue;
        }

        if !is_ancestor(root, &candidate, head_branch)? {
            continue;
        }

        let head_ahead = rev_list_count(root, &format!("{candidate}..{head_branch}"))?;

        let parent_ahead = rev_list_count(root, &format!("{head_branch}..{candidate}"))?;
        let merge_base = run_git(
            root,
            ["merge-base", candidate.as_str(), head_branch].as_slice(),
        )?;
        let merge_base = merge_base.trim();
        if merge_base != candidate_sha {
            continue;
        }

        scored.push((candidate, head_ahead, parent_ahead));
    }

    scored.sort_by(|left, right| {
        (right.1 == 0)
            .cmp(&(left.1 == 0))
            .then(left.1.cmp(&right.1))
            .then(left.2.cmp(&right.2))
            .then(left.0.cmp(&right.0))
    });

    let Some(best) = scored.first() else {
        if base_branch != head_branch && !visited.iter().any(|branch| branch == base_branch) {
            if local_branch_exists(root, base_branch)?
                || is_ancestor(root, base_branch, head_branch)?
            {
                return Ok(base_branch.to_string());
            }
        }

        bail!("could not infer a parent branch for {head_branch} before reaching {base_branch}");
    };

    let tied_best_candidates = scored
        .iter()
        .filter(|candidate| candidate.1 == best.1 && candidate.2 == best.2)
        .collect::<Vec<_>>();
    if tied_best_candidates.len() > 1 {
        if let Some(base_candidate) = tied_best_candidates
            .iter()
            .find(|candidate| candidate.0 == base_branch)
        {
            return Ok(base_candidate.0.clone());
        }

        bail!("could not infer a unique parent branch for {head_branch}");
    }

    Ok(best.0.clone())
}

fn local_branches(root: &Path) -> Result<Vec<String>> {
    Ok(run_git(
        root,
        ["for-each-ref", "--format=%(refname:short)", "refs/heads"].as_slice(),
    )?
    .lines()
    .map(str::trim)
    .filter(|line| !line.is_empty())
    .map(ToString::to_string)
    .collect())
}

fn ensure_local_branch(root: &Path, branch: &str) -> Result<()> {
    if local_branch_exists(root, branch)? {
        return Ok(());
    }

    bail!("branch {branch} does not exist locally");
}

fn ensure_ref_exists(root: &Path, rev: &str) -> Result<()> {
    if ref_exists(root, rev)? {
        return Ok(());
    }

    bail!("ref {rev} does not exist");
}

fn local_branch_exists(root: &Path, branch: &str) -> Result<bool> {
    let output = Command::new("git")
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .current_dir(root)
        .output()
        .with_context(|| format!("failed to check branch {branch}"))?;
    Ok(output.status.success())
}

fn ref_exists(root: &Path, rev: &str) -> Result<bool> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", rev])
        .current_dir(root)
        .output()
        .with_context(|| format!("failed to check ref {rev}"))?;
    Ok(output.status.success())
}

fn is_ancestor(root: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let output = Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .current_dir(root)
        .output()
        .with_context(|| format!("failed to compare ancestry for {ancestor} and {descendant}"))?;

    Ok(output.status.success())
}

fn rev_list_count(root: &Path, range: &str) -> Result<u64> {
    let output = run_git(root, ["rev-list", "--count", range].as_slice())?;
    output
        .trim()
        .parse()
        .with_context(|| format!("failed to parse rev-list count for {range}"))
}

fn rev_parse(root: &Path, rev: &str) -> Result<String> {
    Ok(run_git(root, ["rev-parse", rev].as_slice())?
        .trim()
        .to_string())
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
    use tempfile::TempDir;

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

    fn git(temp: &TempDir, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(temp.path())
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn commit_file(temp: &TempDir, contents: &str, message: &str) {
        fs::write(temp.path().join("stack.txt"), contents).expect("file should be written");
        git(temp, &["add", "stack.txt"]);
        git(temp, &["commit", "-m", message]);
    }

    #[test]
    fn resolves_linear_stack_from_leaf_branch() {
        let temp = TempDir::new().expect("tempdir should be created");
        git(&temp, &["init", "-b", "main"]);
        git(&temp, &["config", "user.name", "Test User"]);
        git(&temp, &["config", "user.email", "test@example.com"]);

        commit_file(&temp, "base\n", "base");
        git(&temp, &["checkout", "-b", "feat/a"]);
        commit_file(&temp, "base\na\n", "feat a");
        git(&temp, &["checkout", "-b", "feat/b"]);
        commit_file(&temp, "base\na\nb\n", "feat b");
        git(&temp, &["checkout", "-b", "feat/c"]);
        commit_file(&temp, "base\na\nb\nc\n", "feat c");

        let stack =
            resolve_stack_review(temp.path(), "feat/c", "main").expect("stack should resolve");

        assert_eq!(
            stack.chain,
            vec![
                "main".to_string(),
                "feat/a".to_string(),
                "feat/b".to_string(),
                "feat/c".to_string()
            ]
        );
        assert_eq!(
            stack
                .edges
                .iter()
                .map(ReviewEdge::label)
                .collect::<Vec<_>>(),
            vec![
                "main...feat/a".to_string(),
                "feat/a...feat/b".to_string(),
                "feat/b...feat/c".to_string()
            ]
        );
    }

    #[test]
    fn resolves_stack_when_leaf_matches_parent_tip() {
        let temp = TempDir::new().expect("tempdir should be created");
        git(&temp, &["init", "-b", "main"]);
        git(&temp, &["config", "user.name", "Test User"]);
        git(&temp, &["config", "user.email", "test@example.com"]);

        commit_file(&temp, "base\n", "base");
        git(&temp, &["checkout", "-b", "feat/a"]);
        commit_file(&temp, "base\na\n", "feat a");
        git(&temp, &["checkout", "-b", "feat/b"]);

        let stack = resolve_stack_review(temp.path(), "feat/b", "main")
            .expect("stack should resolve with same-tip parent");

        assert_eq!(
            stack.chain,
            vec![
                "main".to_string(),
                "feat/a".to_string(),
                "feat/b".to_string()
            ]
        );
        assert_eq!(
            stack
                .edges
                .iter()
                .map(ReviewEdge::label)
                .collect::<Vec<_>>(),
            vec!["main...feat/a".to_string(), "feat/a...feat/b".to_string()]
        );
    }

    #[test]
    fn resolves_stack_when_base_branch_has_moved_on() {
        let temp = TempDir::new().expect("tempdir should be created");
        git(&temp, &["init", "-b", "main"]);
        git(&temp, &["config", "user.name", "Test User"]);
        git(&temp, &["config", "user.email", "test@example.com"]);

        commit_file(&temp, "base\n", "base");
        git(&temp, &["checkout", "-b", "feat/a"]);
        commit_file(&temp, "base\na\n", "feat a");
        git(&temp, &["checkout", "-b", "feat/b"]);
        commit_file(&temp, "base\na\nb\n", "feat b");
        git(&temp, &["checkout", "main"]);
        commit_file(&temp, "base\nmain-followup\n", "main followup");

        let stack = resolve_stack_review(temp.path(), "feat/b", "main")
            .expect("stack should resolve even when base has advanced");

        assert_eq!(
            stack.chain,
            vec![
                "main".to_string(),
                "feat/a".to_string(),
                "feat/b".to_string()
            ]
        );
        assert_eq!(
            stack
                .edges
                .iter()
                .map(ReviewEdge::label)
                .collect::<Vec<_>>(),
            vec!["main...feat/a".to_string(), "feat/a...feat/b".to_string()]
        );
    }

    #[test]
    fn resolves_stack_by_preferring_explicit_base_when_parent_candidates_tie() {
        let temp = TempDir::new().expect("tempdir should be created");
        git(&temp, &["init", "-b", "main"]);
        git(&temp, &["config", "user.name", "Test User"]);
        git(&temp, &["config", "user.email", "test@example.com"]);

        commit_file(&temp, "base\n", "base");
        git(&temp, &["checkout", "-b", "feat/aws-observability"]);
        git(&temp, &["checkout", "main"]);
        git(&temp, &["checkout", "-b", "feat/a"]);
        commit_file(&temp, "base\na\n", "feat a");

        let stack =
            resolve_stack_review(temp.path(), "feat/a", "main").expect("stack should resolve");

        assert_eq!(stack.chain, vec!["main".to_string(), "feat/a".to_string()]);
        assert_eq!(
            stack
                .edges
                .iter()
                .map(ReviewEdge::label)
                .collect::<Vec<_>>(),
            vec!["main...feat/a".to_string()]
        );
    }

    #[test]
    fn resolves_stack_against_remote_tracking_base_without_inserting_local_main_edge() {
        let temp = TempDir::new().expect("tempdir should be created");
        git(&temp, &["init", "-b", "main"]);
        git(&temp, &["config", "user.name", "Test User"]);
        git(&temp, &["config", "user.email", "test@example.com"]);

        commit_file(&temp, "base\n", "base");
        git(
            &temp,
            &["update-ref", "refs/remotes/origin/main", "refs/heads/main"],
        );
        git(&temp, &["checkout", "-b", "feat/a"]);
        commit_file(&temp, "base\na\n", "feat a");
        git(&temp, &["checkout", "-b", "feat/b"]);
        commit_file(&temp, "base\na\nb\n", "feat b");

        let stack = resolve_stack_review(temp.path(), "feat/b", "origin/main")
            .expect("stack should resolve against a remote-tracking base");

        assert_eq!(
            stack.chain,
            vec![
                "origin/main".to_string(),
                "feat/a".to_string(),
                "feat/b".to_string()
            ]
        );
        assert_eq!(
            stack
                .edges
                .iter()
                .map(ReviewEdge::label)
                .collect::<Vec<_>>(),
            vec![
                "origin/main...feat/a".to_string(),
                "feat/a...feat/b".to_string()
            ]
        );
    }

    #[test]
    fn stack_review_starts_on_first_edge() {
        let temp = TempDir::new().expect("tempdir should be created");
        git(&temp, &["init", "-b", "main"]);
        git(&temp, &["config", "user.name", "Test User"]);
        git(&temp, &["config", "user.email", "test@example.com"]);

        commit_file(&temp, "base\n", "base");
        git(&temp, &["checkout", "-b", "feat/a"]);
        commit_file(&temp, "base\na\n", "feat a");
        git(&temp, &["checkout", "-b", "feat/b"]);
        commit_file(&temp, "base\na\nb\n", "feat b");
        git(&temp, &["checkout", "-b", "feat/c"]);
        commit_file(&temp, "base\na\nb\nc\n", "feat c");

        let resolved = ResolvedReview::discover_in_root(
            temp.path().to_path_buf(),
            &ReviewArgs {
                base: "main".to_string(),
                stack: Some("feat/c".to_string()),
                ..ReviewArgs::default()
            },
        )
        .expect("stack review should resolve");

        assert_eq!(
            resolved.repo.current_edge().map(|edge| edge.label()),
            Some("main...feat/a".to_string())
        );
        assert_eq!(
            resolved
                .stack
                .expect("stack review should be present")
                .edges
                .first()
                .map(ReviewEdge::label),
            Some("main...feat/a".to_string())
        );
    }
}
