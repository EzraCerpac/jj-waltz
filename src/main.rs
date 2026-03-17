use std::process::ExitCode;

fn main() -> ExitCode {
    match jj_waltz::cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("jw: {err}");
            ExitCode::FAILURE
        }
    }
}
