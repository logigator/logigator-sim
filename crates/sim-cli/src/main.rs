//! `sim` — the Logigator simulation CLI (plan §7.5).
//!
//! Subcommands: `run` (advance a board), `bench` (throughput), `trace` (per-tick golden dump), and
//! `verify` (check a fixture's final state). The CLI links `sim-core` directly — no FFI (plan D9).
//!
//! Exit codes: `0` success, `1` a `verify` mismatch, `2` a usage/IO/parse error.

mod bench;
mod load;
mod run;
mod trace;
mod verify;

use clap::{Parser, Subcommand};
use std::process::ExitCode;

/// Subcommand handlers return the process exit code on success, or a message rendered as
/// `error: <msg>` (exit 2) on failure.
pub type CliResult = Result<ExitCode, String>;

#[derive(Parser)]
#[command(
    name = "sim",
    version,
    about = "Logigator logic-circuit simulation CLI (plan §7.5)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a board for a tick/time budget; optionally dump the final state.
    Run(run::RunArgs),
    /// Dump a per-tick trace in the golden-corpus format (this engine, not the C++ oracle).
    Trace(trace::TraceArgs),
    /// Check fixtures' final state against an expected snapshot (exit 1 on mismatch).
    Verify(verify::VerifyArgs),
    /// Measure tick throughput (best/mean ticks per second).
    Bench(bench::BenchArgs),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Run(args) => run::run(args),
        Cmd::Trace(args) => trace::trace(args),
        Cmd::Verify(args) => verify::verify(args),
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
