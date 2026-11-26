use std::io::Write;
use std::panic::{self, AssertUnwindSafe};
use std::process::ExitCode;
use termway::log::stderr_line;
use termway::{app, cli, log, log_error};

fn main() -> ExitCode {
    let opts = match cli::parse(std::env::args().skip(1)) {
        Ok(cli::Action::Run(o)) => *o,
        Ok(cli::Action::Help) => {
            let _ = std::io::stdout().write_all(cli::USAGE.as_bytes());
            return ExitCode::SUCCESS;
        }
        Ok(cli::Action::Version) => {
            let _ = writeln!(std::io::stdout(), "termway {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            stderr_line(&format!("termway: {e}"));
            return ExitCode::from(2);
        }
    };
    if let Err(e) = log::init(opts.log.as_deref(), rustix::termios::isatty(std::io::stderr()), opts.verbose) {
        stderr_line(&format!("termway: cannot open log file: {e}"));
        return ExitCode::from(2);
    }
    // The default hook would print into the alternate screen; log instead and print
    // after the terminal has been restored by unwinding.
    panic::set_hook(Box::new(|info| log_error!("panic: {info}")));
    let result = panic::catch_unwind(AssertUnwindSafe(|| app::run(opts)));
    log::flush_buffered();
    match result {
        Ok(Ok((exit, summary))) => {
            stderr_line(&summary);
            ExitCode::from(exit.code())
        }
        Ok(Err(e)) => {
            stderr_line(&format!("termway: {e}"));
            ExitCode::FAILURE
        }
        Err(_) => {
            stderr_line("termway: internal error (panic), terminal restored");
            ExitCode::from(101)
        }
    }
}
