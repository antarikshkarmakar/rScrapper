//! Robin CLI entry point.

use clap::Parser;
use robin::cli::{run_args_with_io, Args, ProductionRunner};
use std::io::{BufReader, Write};
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let input = BufReader::new(std::io::stdin());
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    match run_args_with_io(args, input, &mut stdout, &mut stderr, &ProductionRunner).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(stderr, "error: {error}");
            ExitCode::FAILURE
        }
    }
}
