use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    reset_sigpipe();
    let cli = unporter::cli::Cli::parse();
    match unporter::cli::run(cli) {
        Ok(code) => code,
        Err(e) => {
            unporter::output::error(format!("{e:#}"));
            ExitCode::FAILURE
        }
    }
}

/// Exit quietly on `unporter | head` instead of panicking on a broken pipe.
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
