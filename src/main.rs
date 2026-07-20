//! Thin binary adapter from lev's anyhow result to a process exit status.

use std::process::ExitCode;

fn main() -> ExitCode {
    match lev::run() {
        Ok(code) => ExitCode::from(code.clamp(0, u8::MAX as i32) as u8),
        Err(error) => {
            eprintln!("lev: error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
