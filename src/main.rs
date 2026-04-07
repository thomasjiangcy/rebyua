mod app;
mod cli;
mod clipboard;
mod export;
mod git;
mod model;
mod updater;

fn main() {
    if let Err(err) = cli::run() {
        eprintln!("{err:?}");
        std::process::exit(1);
    }
}
