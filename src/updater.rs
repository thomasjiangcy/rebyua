use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, USER_AGENT};
use serde::Deserialize;
use tar::Archive;
use tempfile::tempdir;

const DEFAULT_REPOSITORY: &str = "thomasjiangcy/rebyua";

#[derive(Debug, Deserialize)]
struct ReleaseResponse {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

pub fn run() -> Result<()> {
    let platform = release_platform()?;
    let repo = repository();
    let client = github_client()?;
    let release = latest_release(&client, &repo)?;
    let target_name = asset_name(&release.tag_name, &platform);
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == target_name)
        .ok_or_else(|| anyhow!("release {} is missing asset {}", release.tag_name, target_name))?;

    let current_version_tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    if release.tag_name == current_version_tag {
        println!("reb is already up to date ({})", release.tag_name);
        return Ok(());
    }

    let current_exe = env::current_exe().context("failed to locate current executable")?;
    let binary = download_release_binary(&client, asset)?;
    replace_current_executable(&current_exe, &binary)?;

    println!("Updated reb to {}", release.tag_name);
    Ok(())
}

fn repository() -> String {
    env::var("REB_RELEASE_REPOSITORY")
        .or_else(|_| env::var("RBA_RELEASE_REPOSITORY"))
        .unwrap_or_else(|_| DEFAULT_REPOSITORY.to_string())
}

fn github_client() -> Result<Client> {
    Client::builder()
        .build()
        .context("failed to create HTTP client")
}

fn latest_release(client: &Client, repository: &str) -> Result<ReleaseResponse> {
    let url = format!("https://api.github.com/repos/{repository}/releases/latest");
    client
        .get(url)
        .header(ACCEPT, "application/vnd.github+json")
        .header(USER_AGENT, user_agent())
        .send()
        .context("failed to query latest release")?
        .error_for_status()
        .context("latest release request failed")?
        .json()
        .context("failed to decode release response")
}

fn download_release_binary(client: &Client, asset: &ReleaseAsset) -> Result<Vec<u8>> {
    let bytes = client
        .get(&asset.browser_download_url)
        .header(USER_AGENT, user_agent())
        .send()
        .with_context(|| format!("failed to download {}", asset.name))?
        .error_for_status()
        .with_context(|| format!("download failed for {}", asset.name))?
        .bytes()
        .with_context(|| format!("failed to read {}", asset.name))?;

    let temp = tempdir().context("failed to create temporary directory")?;
    let archive_path = temp.path().join(&asset.name);
    fs::write(&archive_path, &bytes).with_context(|| format!("failed to write {}", asset.name))?;

    let file = fs::File::open(&archive_path).with_context(|| format!("failed to open {}", asset.name))?;
    let mut archive = Archive::new(GzDecoder::new(file));
    archive
        .unpack(temp.path())
        .with_context(|| format!("failed to unpack {}", asset.name))?;

    let binary_path = find_binary(temp.path())?;
    fs::read(&binary_path).context("failed to read extracted binary")
}

fn replace_current_executable(current_exe: &Path, downloaded_binary: &[u8]) -> Result<()> {
    let parent = current_exe
        .parent()
        .context("current executable has no parent directory")?;
    let temp_target = parent.join("reb.tmp");
    fs::write(&temp_target, downloaded_binary).context("failed to write replacement binary")?;
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(&temp_target)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&temp_target, permissions)?;
    }
    fs::rename(&temp_target, current_exe).context("failed to replace current executable")?;
    Ok(())
}

fn find_binary(root: &Path) -> Result<PathBuf> {
    for entry in walk(root)? {
        if entry.file_name().is_some_and(|name| name == "reb") && entry.is_file() {
            return Ok(entry);
        }
    }
    bail!("release archive does not contain a reb binary")
}

fn walk(root: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            paths.extend(walk(&path)?);
        } else {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn user_agent() -> String {
    format!("reb/{}", env!("CARGO_PKG_VERSION"))
}

fn asset_name(tag: &str, platform: &str) -> String {
    format!("reb-{tag}-{platform}.tar.gz")
}

fn release_platform() -> Result<String> {
    match (env::consts::OS, env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin".to_string()),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin".to_string()),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu".to_string()),
        (os, arch) => bail!("unsupported platform: {os}-{arch}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_expected_asset_name() {
        assert_eq!(
            asset_name("v1.2.3", "aarch64-apple-darwin"),
            "reb-v1.2.3-aarch64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn default_repository_is_github_repo_slug() {
        assert_eq!(DEFAULT_REPOSITORY, "thomasjiangcy/rebyua");
    }
}
