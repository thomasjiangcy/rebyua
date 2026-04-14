use std::env;
use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};

use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, USER_AGENT};
use serde::Deserialize;
use tar::{Archive, EntryType};

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
        .ok_or_else(|| {
            anyhow!(
                "release {} is missing asset {}",
                release.tag_name,
                target_name
            )
        })?;

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

    binary_from_release_archive(&bytes).with_context(|| format!("failed to unpack {}", asset.name))
}

fn binary_from_release_archive(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut archive = Archive::new(GzDecoder::new(bytes));
    let mut binary = None;

    for entry_result in archive.entries().context("failed to read archive entries")? {
        let mut entry = entry_result.context("failed to read archive entry")?;
        let path = entry.path().context("failed to read archive entry path")?;
        let normalized_path = archive_entry_path(&path)?;

        if normalized_path.as_path() != Path::new("reb") {
            continue;
        }

        if entry.header().entry_type() != EntryType::Regular {
            bail!("release archive contains a non-regular reb entry");
        }

        if binary.is_some() {
            bail!("release archive contains multiple reb binaries");
        }

        let mut extracted = Vec::new();
        entry
            .read_to_end(&mut extracted)
            .context("failed to read reb from archive")?;
        binary = Some(extracted);
    }

    binary.ok_or_else(|| anyhow!("release archive does not contain a reb binary"))
}

fn archive_entry_path(path: &Path) -> Result<std::path::PathBuf> {
    let mut normalized = std::path::PathBuf::new();

    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("release archive contains an invalid entry path")
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        bail!("release archive contains an empty entry path");
    }

    Ok(normalized)
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
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Cursor;
    use tar::{Builder, Header};

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

    #[test]
    fn extracts_reb_binary_without_unpacking_to_disk() {
        let archive = gzip_tar_archive(&[("reb", b"binary-bytes", EntryType::Regular)]);

        let binary = binary_from_release_archive(&archive).expect("archive should contain reb");

        assert_eq!(binary, b"binary-bytes");
    }

    #[test]
    fn rejects_parent_dir_archive_paths() {
        let err =
            archive_entry_path(Path::new("../reb")).expect_err("invalid path should fail");

        assert!(err.to_string().contains("invalid entry path"));
    }

    #[test]
    fn rejects_non_regular_reb_entries() {
        let archive = gzip_tar_archive(&[("reb", &[][..], EntryType::Symlink)]);

        let err = binary_from_release_archive(&archive).expect_err("symlink reb should fail");

        assert!(err.to_string().contains("non-regular reb entry"));
    }

    fn gzip_tar_archive(entries: &[(&str, &[u8], EntryType)]) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = Builder::new(encoder);

        for (path, contents, entry_type) in entries {
            let mut header = Header::new_gnu();
            header.set_entry_type(*entry_type);
            header.set_mode(0o755);
            header.set_mtime(0);

            match entry_type {
                EntryType::Regular => {
                    header.set_size(contents.len() as u64);
                    header.set_cksum();
                    builder
                        .append_data(&mut header, *path, Cursor::new(*contents))
                        .expect("regular entry should append");
                }
                _ => {
                    header.set_size(0);
                    header.set_cksum();
                    builder
                        .append_data(&mut header, *path, Cursor::new(Vec::<u8>::new()))
                        .expect("non-regular entry should append");
                }
            }
        }

        builder.finish().expect("archive should finish");
        builder
            .into_inner()
            .expect("encoder should be returned")
            .finish()
            .expect("gzip stream should finish")
    }
}
