//! `edifierctl` binary: parse arguments, run one command, render the result.

use std::process::ExitCode;

use clap::Parser;
use edifierctl::{Cli, Reply, render_status, run};

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("error: cannot start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(&cli)) {
        Ok(reply) => {
            render(&reply, cli.json);
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing(verbosity: u8) {
    let level = match verbosity {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let filter = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| format!("edifierctl={level},edifier_link={level}"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn render(reply: &Reply, json: bool) {
    if json {
        match serde_json::to_string_pretty(reply) {
            Ok(text) => println!("{text}"),
            Err(err) => eprintln!("error: cannot render JSON: {err}"),
        }
        return;
    }

    match reply {
        Reply::Devices { devices } if devices.is_empty() => {
            println!("no Edifier endpoint found — power the headset on and keep it in range");
        }
        Reply::Devices { devices } => {
            for device in devices {
                println!(
                    "{:<38} {:<22} {:>6} {}",
                    device.identifier,
                    device.name.as_deref().unwrap_or("?"),
                    device
                        .rssi
                        .map_or_else(|| "-".to_string(), |rssi| format!("{rssi} dBm")),
                    device.model.display_name()
                );
            }
        }
        Reply::Status { status } => {
            let rendered = render_status(status);
            if rendered.is_empty() {
                println!("no values reported (is the headset in range and switched on?)");
            } else {
                println!("{rendered}");
            }
        }
        Reply::Event { event, status } => {
            println!("{event}");
            let rendered = render_status(status);
            if !rendered.is_empty() {
                println!("{rendered}");
            }
        }
        Reply::Message { message } => println!("{message}"),
    }
}
