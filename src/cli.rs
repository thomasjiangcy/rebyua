use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::app;
use crate::updater;

#[derive(Parser, Debug)]
#[command(
    name = "reb",
    version,
    about = "Lightweight diff reviewer",
    after_help = "Running `reb` without a subcommand is the same as `reb review`."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    #[command(about = "Start review mode explicitly")]
    Review(ReviewArgs),
    #[command(about = "Download and install the latest GitHub release")]
    Update,
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

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Review(args)) => app::run(args),
        Some(Commands::Update) => updater::run(),
        None => app::run(ReviewArgs::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_empty_invocation_as_default_review() {
        let cli = Cli::try_parse_from(["reb"]).expect("cli should parse");

        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_review_with_default_flags() {
        let cli = Cli::try_parse_from(["reb", "review"]).expect("cli should parse");

        let Some(Commands::Review(args)) = cli.command else {
            panic!("expected review command");
        };

        assert_eq!(args.base, "HEAD");
        assert!(args.path.is_empty());
        assert!(!args.staged);
    }

    #[test]
    fn parses_review_flags() {
        let cli = Cli::try_parse_from([
            "reb",
            "review",
            "--base",
            "HEAD~2",
            "--path",
            "src/app.rs",
            "--path",
            "src/cli.rs",
            "--staged",
        ])
        .expect("cli should parse");

        let Some(Commands::Review(args)) = cli.command else {
            panic!("expected review command");
        };

        assert_eq!(args.base, "HEAD~2");
        assert_eq!(args.path, vec!["src/app.rs", "src/cli.rs"]);
        assert!(args.staged);
    }

    #[test]
    fn parses_update_command() {
        let cli = Cli::try_parse_from(["reb", "update"]).expect("cli should parse");

        assert!(matches!(cli.command, Some(Commands::Update)));
    }
}
