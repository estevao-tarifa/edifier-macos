//! `edifier-menubar`: parse arguments, then either manage the LaunchAgent or run the menu bar app.

use std::process::ExitCode;

use clap::Parser;
use edifier_menubar::{Cli, run};

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// `RUST_LOG` wins; `-v` raises the level for this app and the BLE stack.
///
/// Traces go to stderr so that nothing on stdout reaches the LaunchAgent's log as if it were app
/// output.
fn init_tracing(verbosity: u8) {
    let level = match verbosity {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let filter = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| format!("edifier_menubar={level},edifier_link={level}"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
