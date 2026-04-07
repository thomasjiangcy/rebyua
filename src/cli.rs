use anyhow::Result;
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
        None => app::run(ReviewArgs::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_empty_invocation_as_default_review() {
        let cli = Cli::try_parse_from(["rebyua"]).expect("cli should parse");

        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_review_with_default_flags() {
        let cli = Cli::try_parse_from(["rebyua", "review"]).expect("cli should parse");

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
            "rebyua",
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
}
