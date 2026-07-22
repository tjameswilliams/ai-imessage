use std::process::ExitCode;

fn main() -> ExitCode {
    match ai_imessage::cli::run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
