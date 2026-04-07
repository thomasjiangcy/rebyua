mod app;
mod cli;
mod clipboard;
mod export;
mod git;
mod model;

fn main() {
    if let Err(err) = cli::run() {
        eprintln!("{err:?}");
        std::process::exit(1);
    }
}
