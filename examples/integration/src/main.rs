//! Minimal GPUI host. Run the network and verification work outside the UI thread.
//! The trusted policy and paths must be provisioned by the host application.
#[cfg(windows)]
fn main() {
    use gpui_kit::{
        App, Bounds, Context, Render, Window, WindowBounds, WindowOptions, div, prelude::*, px,
        size,
    };
    struct Example;
    impl Render for Example {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .p_5()
                .child("Veltrix integration: configure and call prepare_update on a worker thread.")
        }
    }
    gpui_kit::application().run(|cx: &mut App| {
        gpui_kit::init(cx);
        let bounds = Bounds::centered(None, size(px(650.), px(150.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Example),
        )
        .expect("open window");
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
}

#[cfg(not(windows))]
fn main() {
    eprintln!("This GPUI integration example currently targets Windows.");
}

/// Downloads and verifies a release, then starts Veltrix. After `Ok(true)`,
/// the host must save its work, close helper processes, and exit promptly.
#[allow(dead_code)]
fn prepare_update(
    policy: &veltrix_core::Policy,
    owner: &str,
    repo: &str,
    job: &std::path::Path,
    updater: &std::path::Path,
    cancel: &std::sync::atomic::AtomicBool,
    progress: impl FnMut(u64, u64),
) -> Result<bool, Box<dyn std::error::Error>> {
    use veltrix_core::{
        UpdateRequest, download, github_releases, http_client, release_manifest, select_release,
        wait_for_ready,
    };
    let client = http_client()?;
    let releases = github_releases(&client, owner, repo)?;
    let Some(release) = select_release(&releases, &policy.current_version, policy.channel) else {
        return Ok(false);
    };
    let (manifest, bytes, signature) = release_manifest(&client, policy, release)?;
    std::fs::write(job.join("manifest.json"), bytes)?;
    std::fs::write(job.join("manifest.sig"), signature)?;
    download(
        &client,
        &manifest,
        &job.join("package.exe"),
        cancel,
        progress,
    )?;
    let request = UpdateRequest {
        protocol: 1,
        app_id: policy.app_id.clone(),
        parent_pid: std::process::id(),
        job_dir: job.to_owned(),
    };
    let mut child = request.launch(updater)?;
    wait_for_ready(&mut child)?;
    Ok(true)
}
