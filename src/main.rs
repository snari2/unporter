use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    reset_sigpipe();
    let cli = porthog::cli::Cli::parse();
    match porthog::cli::run(cli) {
        Ok(code) => code,
        Err(e) => {
            porthog::output::error(format!("{e:#}"));
            ExitCode::FAILURE
        }
    }
}

/// Exit quietly on `porthog | head` instead of panicking on a broken pipe.
fn reset_sigpipe() {
    #[cfg(unix)]
    // SAFETY: called once at startup before any other threads exist.
    unsafe {
        let _ = nix::sys::signal::signal(
            nix::sys::signal::Signal::SIGPIPE,
            nix::sys::signal::SigHandler::SigDfl,
        );
    }
}
