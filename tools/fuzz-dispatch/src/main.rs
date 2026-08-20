use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/cargo-fuzz-nightly");
    let status = Command::new(script)
        .args(std::env::args_os().skip(1))
        .status();

    match status {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .map_or(ExitCode::FAILURE, ExitCode::from),
        Err(error) => {
            eprintln!("error: failed to launch nightly cargo-fuzz: {error}");
            ExitCode::from(2)
        }
    }
}

