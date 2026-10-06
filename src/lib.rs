//! fuckinggen — generate images through a ChatGPT subscription from the terminal.
//!
//! Talks to the Codex backend (`chatgpt.com/backend-api/codex/responses`) with the
//! OAuth credentials an existing CLI login already stored, and attaches the hosted
//! `image_generation` tool to a single-turn request.

pub mod api;
pub mod args;
pub mod auth;
pub mod files;
pub mod http;
pub mod images;
pub mod run;
pub mod tui;

use std::process::ExitCode;

/// Shared entry point for the `fuckinggen` and `fgen` binaries.
pub fn bin_main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args::parse(&args).and_then(run::run) {
        Ok(code) => ExitCode::from(code as u8),
        Err(err) => {
            eprintln!("fuckinggen: {err:#}");
            ExitCode::from(1)
        }
    }
}
