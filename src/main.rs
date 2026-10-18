mod runtime;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

/// Semantic file search, run on the NPU.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// ovfetch prefix holding the OpenVINO runtime [default: ~/.local/lib/vinosearch/openvino]
    #[arg(long, global = true, env = "VINOSEARCH_OPENVINO_DIR")]
    openvino_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check the NPU, its driver, and the OpenVINO runtime, then compile and run a probe model.
    Doctor {
        /// Device to probe.
        #[arg(long, default_value = "NPU")]
        device: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Doctor { device } => runtime::default_prefix(cli.openvino_dir)
            .and_then(|prefix| runtime::doctor(&prefix, &device)),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
