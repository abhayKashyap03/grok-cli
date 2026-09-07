//! Binary entry point. Everything real lives in the library so it can be
//! tested without spawning a process.

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(error) = grok_cli::cli::main().await {
        // The terminal is already restored by the time this runs, so writing to
        // stderr is safe. `{error:#}` prints the whole anyhow context chain,
        // which is where the actionable detail usually is.
        eprintln!("error: {error:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
