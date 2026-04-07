use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use crate::app;

#[derive(Parser, Debug)]
#[command(
    name = "rebyua",
    version,
    about = "Minimal local diff reviewer for agent loops"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Review(ReviewArgs),
    Export(ExportArgs),
}

#[derive(clap::Args, Debug, Clone)]
pub struct ReviewArgs {
    #[arg(long, default_value = "HEAD")]
    pub base: String,
    #[arg(long)]
    pub path: Vec<String>,
    #[arg(long)]
    pub staged: bool,
}

impl Default for ReviewArgs {
    fn default() -> Self {
        Self {
            base: "HEAD".to_string(),
            path: Vec::new(),
            staged: false,
        }
    }
}

#[derive(clap::Args, Debug)]
struct ExportArgs {
    #[arg(long)]
    stdout: bool,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Review(args)) => app::run(args),
        Some(Commands::Export(_)) => {
            bail!("export is available from inside `rebyua review` in the current version")
        }
        None => app::run(ReviewArgs::default()),
    }
}
