use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

pub const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(200);
const WATCH_DEBOUNCE: Duration = Duration::from_millis(300);

pub struct RepoWatcher {
    _watcher: RecommendedWatcher,
    receiver: Receiver<notify::Result<Event>>,
    root: PathBuf,
    git_dir: PathBuf,
    refresh_at: Option<Instant>,
}

#[derive(Default)]
pub struct WatchUpdate {
    pub refresh_due: bool,
    pub error: Option<String>,
}

impl RepoWatcher {
    pub fn new(root: &Path) -> Result<Self> {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let (sender, receiver) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = sender.send(event);
        })
        .context("failed to create filesystem watcher")?;

        watcher
            .watch(&root, RecursiveMode::Recursive)
            .with_context(|| format!("failed to watch {}", root.display()))?;

        let git_dir = resolve_git_dir(&root).unwrap_or_else(|| root.join(".git"));
        if !git_dir.starts_with(&root) {
            watcher
                .watch(&git_dir, RecursiveMode::Recursive)
                .with_context(|| format!("failed to watch {}", git_dir.display()))?;
        }

        Ok(Self {
            _watcher: watcher,
            receiver,
            root,
            git_dir,
            refresh_at: None,
        })
    }

    pub fn poll(&mut self, now: Instant) -> WatchUpdate {
        let mut update = WatchUpdate::default();

        loop {
            match self.receiver.try_recv() {
                Ok(Ok(event)) => {
                    if event
                        .paths
                        .iter()
                        .any(|path| is_relevant_path(&self.root, &self.git_dir, path))
                    {
                        self.refresh_at = Some(now + WATCH_DEBOUNCE);
                    }
                }
                Ok(Err(err)) => update.error = Some(format!("File watching failed: {err}")),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    update.error = Some("File watcher stopped unexpectedly".to_string());
                    break;
                }
            }
        }

        if self.refresh_at.is_some_and(|deadline| deadline <= now) {
            self.refresh_at = None;
            update.refresh_due = true;
        }

        update
    }
}

fn resolve_git_dir(root: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--absolute-git-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    Some(PathBuf::from(path.trim()))
}

fn is_relevant_path(root: &Path, git_dir: &Path, path: &Path) -> bool {
    if let Ok(relative) = path.strip_prefix(git_dir) {
        return is_relevant_git_path(relative);
    }

    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let mut components = relative.components();
    match components.next() {
        Some(Component::Normal(name)) if name == "target" => false,
        Some(Component::Normal(name)) if name == ".git" => {
            is_relevant_git_path(components.as_path())
        }
        _ => true,
    }
}

fn is_relevant_git_path(path: &Path) -> bool {
    matches!(
        path.components().next(),
        Some(Component::Normal(name))
            if name == "index"
                || name == "HEAD"
                || name == "refs"
                || name == "packed-refs"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::thread;

    #[test]
    fn filters_build_and_git_internal_events() {
        let root = Path::new("/repo");
        let git_dir = root.join(".git");

        assert!(is_relevant_path(root, &git_dir, &root.join("src/main.rs")));
        assert!(!is_relevant_path(root, &git_dir, &root.join("target/reb")));
        assert!(is_relevant_path(root, &git_dir, &git_dir.join("index")));
        assert!(is_relevant_path(
            root,
            &git_dir,
            &git_dir.join("refs/heads/main")
        ));
        assert!(!is_relevant_path(
            root,
            &git_dir,
            &git_dir.join("objects/aa/bb")
        ));
    }

    #[test]
    fn accepts_an_external_worktree_git_directory() {
        let root = Path::new("/repo-worktree");
        let git_dir = Path::new("/repo/.git/worktrees/repo-worktree");

        assert!(is_relevant_path(root, git_dir, &git_dir.join("index")));
        assert!(!is_relevant_path(root, git_dir, &git_dir.join("logs/HEAD")));
    }

    #[test]
    fn reports_a_debounced_file_change() {
        let temp = tempfile::tempdir().expect("temp directory should be created");
        let mut watcher = RepoWatcher::new(temp.path()).expect("watcher should start");
        thread::sleep(Duration::from_millis(250));
        fs::write(temp.path().join("changed.txt"), "changed")
            .expect("watched file should be written");

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let update = watcher.poll(Instant::now());
            assert!(update.error.is_none(), "watching should not fail");
            if update.refresh_due {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }

        panic!("watcher did not report the file change");
    }
}
