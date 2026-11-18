#[cfg(windows)]
mod ui;
#[cfg(windows)]
mod windows;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use std::process::ExitCode;
    let job = match windows::prepare() {
        Ok(job) => job,
        Err(error) => {
            use std::io::Write;
            let _ = writeln!(std::io::stdout().lock(), "ERROR");
            eprintln!("veltrix: {error:#}");
            ui::show_preparation_error(format!("{error:#}"));
            return ExitCode::FAILURE;
        }
    };
    match ui::run(job) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("veltrix: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("veltrix requiere Windows.");
    std::process::ExitCode::FAILURE
}
