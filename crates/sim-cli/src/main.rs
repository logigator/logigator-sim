//! `sim` — the Logigator simulation CLI.
//!
//! Subcommands: `run` (advance a board), `bench` (throughput), and `trace` (per-tick golden dump).
//! The CLI links `sim-core` directly — no FFI.
//!
//! Exit codes: `0` success, `2` a usage/IO/parse error.

mod bench;
mod load;
mod run;
mod trace;

use clap::{Parser, Subcommand};
use std::process::ExitCode;

/// Subcommand handlers return the process exit code on success, or a message rendered as
/// `error: <msg>` (exit 2) on failure.
pub type CliResult = Result<ExitCode, String>;

#[derive(Parser)]
#[command(
    name = "sim",
    version,
    about = "Logigator logic-circuit simulation CLI"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a board for a tick/time budget; optionally dump the final state.
    Run(run::RunArgs),
    /// Dump a per-tick trace in the golden-corpus format.
    Trace(trace::TraceArgs),
    /// Measure tick throughput (best/mean ticks per second).
    Bench(bench::BenchArgs),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Run(args) => run::run(args),
        Cmd::Trace(args) => trace::trace(args),
        Cmd::Bench(args) => bench::bench(args),
    };
    match result {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::from(2)
        }
    }
}
