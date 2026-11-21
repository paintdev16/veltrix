#[cfg(target_os = "linux")]
mod linux;
#[cfg(any(windows, target_os = "linux"))]
mod ui;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(windows)]
use windows as platform;

#[cfg(target_os = "linux")]
fn has_graphical_display() -> bool {
    if std::env::var_os("VELTRIX_HEADLESS").is_some_and(|v| v == "1") {
        return false;
    }
    let present = |key: &str| std::env::var_os(key).is_some_and(|v| !v.is_empty());
    present("DISPLAY") || present("WAYLAND_DISPLAY")
}

#[cfg(any(windows, target_os = "linux"))]
fn main() -> std::process::ExitCode {
    use std::process::ExitCode;
    let job = match platform::prepare() {
        Ok(job) => job,
        Err(error) => {
            use std::io::Write;
            let _ = writeln!(std::io::stdout().lock(), "ERROR");
            eprintln!("veltrix: {error:#}");
            #[cfg(windows)]
            ui::show_preparation_error(format!("{error:#}"));
            #[cfg(target_os = "linux")]
            if has_graphical_display() {
                ui::show_preparation_error(format!("{error:#}"));
            }
            return ExitCode::FAILURE;
        }
    };
    #[cfg(target_os = "linux")]
    if !has_graphical_display() {
        let ok = job.run(|status| {
            eprintln!("veltrix: {} - {}", status.stage, status.message);
        });
        return if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    match ui::run(job) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("veltrix: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
fn main() -> std::process::ExitCode {
    eprintln!("veltrix requiere Windows o Linux.");
    std::process::ExitCode::FAILURE
}
