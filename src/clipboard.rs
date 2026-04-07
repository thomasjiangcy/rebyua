use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

pub fn copy_to_clipboard(text: &str) -> Result<()> {
    if cfg!(target_os = "macos") {
        pipe_to_command("pbcopy", &[], text)?;
        return Ok(());
    }

    if cfg!(target_os = "linux") {
        if command_exists("wl-copy") {
            pipe_to_command("wl-copy", &[], text)?;
            return Ok(());
        }
        if command_exists("xclip") {
            pipe_to_command("xclip", &["-selection", "clipboard"], text)?;
            return Ok(());
        }
        bail!("clipboard integration requires `wl-copy` or `xclip` on Linux");
    }

    bail!("clipboard integration is only supported on macOS and Linux");
}

fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .arg("-lc")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn pipe_to_command(binary: &str, args: &[&str], text: &str) -> Result<()> {
    let mut child = Command::new(binary)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {binary}"))?;

    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(text.as_bytes())
            .with_context(|| format!("failed to write to {binary} stdin"))?;
    }

    let status = child
        .wait()
        .with_context(|| format!("failed to wait for {binary}"))?;

    if !status.success() {
        bail!("{binary} exited with status {status}");
    }

    Ok(())
}
