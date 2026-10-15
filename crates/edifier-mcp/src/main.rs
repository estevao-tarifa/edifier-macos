//! `edifier-mcp` binary: an MCP stdio server for Edifier headsets.
//!
//! Usage: `edifier-mcp [--device <name-or-uuid>]`. The headset selector may also come from the
//! `EDIFIER_DEVICE` environment variable. All diagnostics go to stderr so stdout stays a clean
//! JSON-RPC channel.

use std::process::ExitCode;

use edifier_mcp::serve_stdio;

fn main() -> ExitCode {
    let mut selector = std::env::var("EDIFIER_DEVICE").ok();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--device" => match args.next() {
                Some(value) => selector = Some(value),
                None => {
                    eprintln!("error: --device needs a name fragment or CoreBluetooth identifier");
                    return ExitCode::FAILURE;
                }
            },
            "--help" | "-h" => {
                println!(
                    "edifier-mcp {} — MCP stdio server controlling an Edifier headset\n\n\
                     USAGE: edifier-mcp [--device <name-or-uuid>]\n\n\
                     --device   name fragment (EDIFIER W820NB) or 36-character CoreBluetooth identifier;\n\
                     \x20          defaults to the EDIFIER_DEVICE environment variable, then to the\n\
                     \x20          first Edifier endpoint found at connect time\n\
                     --version  print the version\n\
                     --help     print this help",
                    env!("CARGO_PKG_VERSION")
                );
                return ExitCode::SUCCESS;
            }
            "--version" => {
                println!("{}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("error: unknown argument {other:?} (try --help)");
                return ExitCode::FAILURE;
            }
        }
    }

    init_tracing();
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

    match runtime.block_on(serve_stdio(selector)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let filter =
        std::env::var("RUST_LOG").unwrap_or_else(|_| "edifier_mcp=info,edifier_link=info".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
