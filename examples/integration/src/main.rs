//! Minimal GPUI host. Run the network and verification work outside the UI thread.
//! The trusted policy and paths must be provisioned by the host application.
#[cfg(windows)]
fn main() {
    use gpui_kit::{
        App, Bounds, Context, Render, Window, WindowBounds, WindowOptions, div, prelude::*, px,
        size,
    };
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;
    struct Example {
        status: Arc<Mutex<String>>,
        ready: Arc<AtomicBool>,
    }
    impl Render for Example {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .p_5()
                .child(self.status.lock().expect("status lock").clone())
        }
    }
    let status = Arc::new(Mutex::new(previous_status()));
    let ready = Arc::new(AtomicBool::new(false));
    let busy = Arc::new(AtomicBool::new(false));
    if std::env::args().any(|arg| arg == "--update") {
        busy.store(true, Ordering::Release);
        let worker_status = status.clone();
        let worker_ready = ready.clone();
        let worker_busy = busy.clone();
        std::thread::spawn(move || {
            *worker_status.lock().unwrap() = "Buscando actualizaciones...".into();
            let result = run_update(|done, total| {
                *worker_status.lock().unwrap() = format!("Descargando: {done} / {total} bytes");
            });
            match result {
                Ok(true) => {
                    worker_ready.store(true, Ordering::Release);
                }
                Ok(false) => {
                    *worker_status.lock().unwrap() = "No hay actualizaciones disponibles.".into();
                    worker_busy.store(false, Ordering::Release);
                }
                Err(error) => {
                    *worker_status.lock().unwrap() = format!("No se pudo actualizar: {error}");
                    worker_busy.store(false, Ordering::Release);
                }
            }
        });
    } else if status
        .lock()
        .unwrap()
        .contains("no tiene resultado terminal")
    {
        // The updater can finish writing its terminal event after launching us.
        let watch_status = status.clone();
        std::thread::spawn(move || {
            for _ in 0..120 {
                std::thread::sleep(Duration::from_secs(1));
                let next = previous_status();
                if !next.contains("no tiene resultado terminal") {
                    *watch_status.lock().unwrap() = next;
                    break;
                }
            }
        });
    }
    gpui_kit::application().run(move |cx: &mut App| {
        gpui_kit::init(cx);
        let bounds = Bounds::centered(None, size(px(650.), px(150.)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, cx| {
                    cx.new(|cx| {
                        cx.spawn(async move |entity, cx| {
                            loop {
                                cx.background_executor()
                                    .timer(Duration::from_millis(100))
                                    .await;
                                if entity
                                    .update(cx, |view: &mut Example, cx| {
                                        cx.notify();
                                        if view.ready.load(Ordering::Acquire) {
                                            cx.quit();
                                        }
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        })
                        .detach();
                        Example { status, ready }
                    })
                },
            )
            .expect("open window");
        let _ = window.update(cx, |_, window, cx| {
            window.on_window_should_close(cx, move |_, _| !busy.load(Ordering::Acquire));
        });
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
        if let Err(error) = veltrix_core::confirm_startup() {
            eprintln!("No se pudo confirmar el arranque: {error}");
            cx.quit();
        }
    });
}

#[cfg(windows)]
fn previous_status() -> String {
    let result = (|| -> Result<_, Box<dyn std::error::Error>> {
        let exe = std::env::current_exe()?;
        let root = exe.parent().ok_or("missing installation directory")?;
        let job = std::fs::read_to_string(root.join(".updater/example-last-job.txt"))?;
        Ok(veltrix_core::update_outcome(
            &veltrix_core::read_update_events(std::path::Path::new(&job))?,
        ))
    })();
    match result {
        Ok(veltrix_core::UpdateOutcome::Succeeded) => {
            "La instalación anterior se completó y la aplicación se inició.".into()
        }
        Ok(veltrix_core::UpdateOutcome::Failed) => {
            "La actualización anterior falló; consulta events.jsonl.".into()
        }
        Ok(veltrix_core::UpdateOutcome::InProgress) => {
            "La actualización anterior no tiene resultado terminal.".into()
        }
        Err(_) => "Ejecuta con --update para buscar una actualización.".into(),
    }
}

#[cfg(windows)]
fn run_update(progress: impl FnMut(u64, u64)) -> Result<bool, Box<dyn std::error::Error>> {
    #[derive(serde::Deserialize)]
    struct Config {
        app_id: String,
        channel: veltrix_core::Channel,
        public_key: String,
        #[serde(default)]
        additional_public_keys: Vec<String>,
        max_package_size: u64,
    }
    #[derive(serde::Deserialize)]
    struct Repo {
        owner: String,
        repo: String,
    }
    let executable = std::env::current_exe()?;
    let root = executable
        .parent()
        .ok_or("missing installation directory")?;
    let control = root.join(".updater");
    let config: Config = serde_json::from_slice(&std::fs::read(control.join("app.json"))?)?;
    let repo: Repo =
        serde_json::from_slice(&std::fs::read(control.join("example-repository.json"))?)?;
    let public_key: [u8; 32] = hex::decode(config.public_key)?
        .try_into()
        .map_err(|_| "invalid public key")?;
    if config.additional_public_keys.len() > 8 {
        return Err("too many trusted keys".into());
    }
    let keys: Vec<[u8; 32]> = config
        .additional_public_keys
        .iter()
        .map(|key| {
            hex::decode(key)
                .map_err(|_| "invalid public key")?
                .try_into()
                .map_err(|_| "invalid public key")
        })
        .collect::<Result<_, _>>()?;
    let policy = veltrix_core::Policy {
        app_id: config.app_id,
        channel: config.channel,
        current_version: std::fs::read_to_string(root.join("version.txt"))?
            .trim()
            .parse()?,
        platform: "windows".into(),
        architecture: std::env::consts::ARCH.into(),
        public_key,
        max_package_size: config.max_package_size,
    };
    let jobs = control.join("jobs");
    std::fs::create_dir_all(&jobs)?;
    let job = tempfile::Builder::new()
        .prefix("job-")
        .tempdir_in(jobs)?
        .keep();
    std::fs::write(
        control.join("example-last-job.txt"),
        job.to_string_lossy().as_bytes(),
    )?;
    prepare_update(
        (&policy, &keys),
        &repo.owner,
        &repo.repo,
        &job,
        &control.join("veltrix.exe"),
        &std::sync::atomic::AtomicBool::new(false),
        progress,
    )
}

#[cfg(not(windows))]
fn main() {
    eprintln!("This GPUI integration example currently targets Windows.");
}

/// Downloads and verifies a release, then starts Veltrix. After `Ok(true)`,
/// the host must save its work, close helper processes, and exit promptly.
#[cfg_attr(not(windows), allow(dead_code))]
fn prepare_update(
    trust: (&veltrix_core::Policy, &[[u8; 32]]),
    owner: &str,
    repo: &str,
    job: &std::path::Path,
    updater: &std::path::Path,
    cancel: &std::sync::atomic::AtomicBool,
    progress: impl FnMut(u64, u64),
) -> Result<bool, Box<dyn std::error::Error>> {
    use veltrix_core::{
        UpdateRequest, download, github_releases, http_client, release_manifest_with_keys,
        select_release, wait_for_ready,
    };
    let (policy, keys) = trust;
    let client = http_client()?;
    let releases = github_releases(&client, owner, repo)?;
    let Some(release) = select_release(&releases, &policy.current_version, policy.channel) else {
        return Ok(false);
    };
    let (manifest, bytes, signature) = release_manifest_with_keys(&client, policy, release, keys)?;
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
        protocol: veltrix_core::CONFIRMED_STARTUP_PROTOCOL,
        app_id: policy.app_id.clone(),
        parent_pid: std::process::id(),
        job_dir: job.to_owned(),
    };
    let mut child = request.launch(updater)?;
    wait_for_ready(&mut child)?;
    Ok(true)
}
