#![cfg(target_os = "linux")]
//! Isolated end-to-end tests for Linux (headless via VELTRIX_HEADLESS=1).
use ed25519_dalek::{Signer, SigningKey};
use flate2::{Compression, write::GzEncoder};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, Permissions},
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use veltrix_core::{UpdateEvent, UpdateRequest};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    root: PathBuf,
    job: PathBuf,
    updater: PathBuf,
    app_bytes: Vec<u8>,
    key: SigningKey,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Test Application");
        let control = root.join(".updater");
        let job = dir.path().join("job");
        fs::create_dir_all(&control).unwrap();
        fs::create_dir(&job).unwrap();
        fs::set_permissions(&root, Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&control, Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&job, Permissions::from_mode(0o700)).unwrap();

        let source = dir.path().join("host.rs");
        fs::write(
            &source,
            r#"
fn main() {
    if std::env::args().any(|arg| arg == "--hold") {
        loop { std::thread::sleep(std::time::Duration::from_secs(1)); }
    }
    let exe = std::env::current_exe().unwrap();
    let root = exe.parent().unwrap();
    if root.join("crash-on-restart").exists() { std::process::exit(7); }
    if root.join("stall-on-restart").exists() {
        loop { std::thread::sleep(std::time::Duration::from_secs(1)); }
    }
    std::fs::write(root.join("restarted.txt"), b"started").unwrap();
    if let Some(path) = std::env::var_os("VELTRIX_STARTUP_ACK") {
        std::fs::OpenOptions::new().write(true).create_new(true).open(path).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
"#,
        )
        .unwrap();
        let app = root.join("app");
        let result = Command::new("rustc")
            .args(["--edition", "2024"])
            .arg(&source)
            .arg("-o")
            .arg(&app)
            .status()
            .unwrap();
        assert!(result.success());
        fs::set_permissions(&app, Permissions::from_mode(0o755)).unwrap();
        let app_bytes = fs::read(&app).unwrap();
        let updater = control.join("veltrix");
        fs::copy(env!("CARGO_BIN_EXE_veltrix"), &updater).unwrap();
        fs::set_permissions(&updater, Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("version.txt"), "1.0.0").unwrap();
        fs::write(root.join("user-data.txt"), "keep this").unwrap();
        let lock = control.join("update.lock");
        fs::write(&lock, []).unwrap();
        fs::set_permissions(&lock, Permissions::from_mode(0o600)).unwrap();
        let key = SigningKey::from_bytes(&[51; 32]);
        fs::write(
            control.join("app.json"),
            serde_json::to_vec(&serde_json::json!({
                "app_id":"org.example.e2e",
                "name":"Test app",
                "executable":"app",
                "channel":"stable",
                "public_key":hex::encode(key.verifying_key().to_bytes()),
                "max_package_size": 100_000_000
            }))
            .unwrap(),
        )
        .unwrap();
        Self {
            dir,
            root,
            job,
            updater,
            app_bytes,
            key,
        }
    }

    fn package(&self, dir: &Path, version: &str, fail: bool) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let out = dir.join("package.tar.gz");
        let file = fs::File::create(&out).unwrap();
        let mut encoder = GzEncoder::new(file, Compression::default());
        {
            let mut builder = tar::Builder::new(&mut encoder);
            let mut append = |name: &str, bytes: &[u8], mode: u32| {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(mode);
                header.set_cksum();
                builder.append_data(&mut header, name, bytes).unwrap();
            };
            if fail {
                // Triggers validation failure (missing version.txt in archive).
                append("app", &self.app_bytes, 0o755);
            } else {
                append("app", &self.app_bytes, 0o755);
                append("version.txt", version.as_bytes(), 0o644);
            }
            builder.finish().unwrap();
        }
        encoder.finish().unwrap();
        out
    }

    fn signed_package(&self, dest: &Path, package: &Path, version: &str) {
        fs::create_dir_all(dest).unwrap();
        fs::set_permissions(dest, Permissions::from_mode(0o700)).unwrap();
        let bytes = fs::read(package).unwrap();
        let manifest = serde_json::to_vec(&serde_json::json!({
            "app_id":"org.example.e2e",
            "version":version,
            "channel":"stable",
            "platform":"linux",
            "architecture":std::env::consts::ARCH,
            "size":bytes.len(),
            "url":"https://example.com/package.tar.gz",
            "sha256":hex::encode(Sha256::digest(&bytes)),
            "installed_sha256":hex::encode(Sha256::digest(&self.app_bytes))
        }))
        .unwrap();
        fs::write(
            dest.join("manifest.sig"),
            self.key.sign(&manifest).to_bytes(),
        )
        .unwrap();
        fs::write(dest.join("manifest.json"), manifest).unwrap();
        fs::write(dest.join("package.tar.gz"), bytes).unwrap();
    }

    fn launch(&self) -> (Process, Process, mpsc::Receiver<String>) {
        self.launch_with_protocol(veltrix_core::CONFIRMED_STARTUP_PROTOCOL)
    }

    fn launch_with_protocol(&self, protocol: u32) -> (Process, Process, mpsc::Receiver<String>) {
        let host = Process(
            Command::new(self.root.join("app"))
                .arg("--hold")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let mut updater = Process(
            Command::new(&self.updater)
                .env("VELTRIX_HEADLESS", "1")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let request = UpdateRequest {
            protocol,
            app_id: "org.example.e2e".into(),
            parent_pid: host.0.id(),
            job_dir: self.job.clone(),
        };
        serde_json::to_writer(updater.0.stdin.take().unwrap(), &request).unwrap();
        let stdout = updater.0.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        });
        (host, updater, rx)
    }

    fn terminal(&self) -> UpdateEvent {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Ok(events) = veltrix_core::read_update_events(&self.job)
                && let Some(event) = events.into_iter().last()
                && event.terminal
            {
                return event;
            }
            assert!(
                Instant::now() < deadline,
                "update timed out; log: {:?}",
                fs::read_to_string(self.job.join("events.jsonl"))
            );
            thread::sleep(Duration::from_millis(100));
        }
    }
}

#[test]
fn full_update_waits_installs_and_restarts() {
    let f = Fixture::new();
    let package = f.package(&f.dir.path().join("build"), "2.0.0", false);
    f.signed_package(&f.job, &package, "2.0.0");
    let (mut host, _updater, ready) = f.launch();
    assert_eq!(
        ready.recv_timeout(Duration::from_secs(30)).unwrap().trim(),
        "READY"
    );
    assert_eq!(
        fs::read_to_string(f.root.join("version.txt")).unwrap(),
        "1.0.0"
    );
    host.0.kill().unwrap();
    host.0.wait().unwrap();
    let event = f.terminal();
    assert!(event.success, "{}", event.message);
    assert!(event.message.contains("confirmó el arranque"));
    assert_eq!(
        fs::read_to_string(f.root.join("version.txt")).unwrap(),
        "2.0.0"
    );
    assert_eq!(
        fs::read_to_string(f.root.join("user-data.txt")).unwrap(),
        "keep this"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f.root.join("restarted.txt").exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(f.root.join("restarted.txt").exists());
}

#[test]
fn legacy_host_can_still_update_without_startup_ack() {
    let f = Fixture::new();
    let package = f.package(&f.dir.path().join("build"), "2.0.0", false);
    f.signed_package(&f.job, &package, "2.0.0");
    let (mut host, _updater, ready) = f.launch_with_protocol(veltrix_core::LEGACY_PROTOCOL);
    assert_eq!(
        ready.recv_timeout(Duration::from_secs(30)).unwrap().trim(),
        "READY"
    );
    host.0.kill().unwrap();
    host.0.wait().unwrap();
    let event = f.terminal();
    assert!(event.success, "{}", event.message);
    assert!(event.message.contains("verifica solo el lanzamiento"));
}

#[test]
fn failed_install_restores_signed_backup_and_reports_failure() {
    let f = Fixture::new();
    let bad = f.package(&f.dir.path().join("bad"), "2.0.0", true);
    let old = f.package(&f.dir.path().join("old"), "1.0.0", false);
    f.signed_package(&f.job, &bad, "2.0.0");
    f.signed_package(&f.root.join(".updater/rollback"), &old, "1.0.0");
    let (mut host, _updater, ready) = f.launch();
    assert_eq!(
        ready.recv_timeout(Duration::from_secs(30)).unwrap().trim(),
        "READY"
    );
    host.0.kill().unwrap();
    host.0.wait().unwrap();
    let event = f.terminal();
    assert!(!event.success);
    assert!(event.message.contains("Se restauró"), "{}", event.message);
    assert_eq!(
        fs::read_to_string(f.root.join("version.txt")).unwrap(),
        "1.0.0"
    );
    assert_eq!(
        fs::read_to_string(f.root.join("user-data.txt")).unwrap(),
        "keep this"
    );
}

fn failed_startup_restores_backup(marker: &str, message: &str) {
    let f = Fixture::new();
    fs::write(f.root.join(marker), []).unwrap();
    let update = f.package(&f.dir.path().join("build"), "2.0.0", false);
    let backup = f.package(&f.dir.path().join("old"), "1.0.0", false);
    f.signed_package(&f.job, &update, "2.0.0");
    f.signed_package(&f.root.join(".updater/rollback"), &backup, "1.0.0");
    let (mut host, _updater, ready) = f.launch();
    assert_eq!(
        ready.recv_timeout(Duration::from_secs(30)).unwrap().trim(),
        "READY"
    );
    host.0.kill().unwrap();
    host.0.wait().unwrap();
    let event = f.terminal();
    assert!(!event.success);
    assert!(event.message.contains(message), "{}", event.message);
    assert!(event.message.contains("Se restauró"), "{}", event.message);
    assert_eq!(
        fs::read_to_string(f.root.join("version.txt")).unwrap(),
        "1.0.0"
    );
}

#[test]
fn restart_crash_restores_signed_backup() {
    failed_startup_restores_backup("crash-on-restart", "se cerró antes de confirmar");
}

#[test]
#[ignore = "waits 30 seconds for startup acknowledgement timeout"]
fn restart_timeout_stops_child_and_restores_signed_backup() {
    failed_startup_restores_backup(
        "stall-on-restart",
        "no confirmó el arranque dentro del plazo",
    );
}

#[test]
fn corrupted_package_is_rejected_before_parent_exits() {
    let f = Fixture::new();
    let package = f.package(&f.dir.path().join("build"), "2.0.0", false);
    f.signed_package(&f.job, &package, "2.0.0");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(f.job.join("package.tar.gz"))
        .unwrap();
    file.write_all(b"tampered").unwrap();
    drop(file);
    let (_host, _updater, ready) = f.launch();
    assert_eq!(
        ready.recv_timeout(Duration::from_secs(30)).unwrap().trim(),
        "ERROR"
    );
    let event = f.terminal();
    assert!(!event.success);
    assert_eq!(
        fs::read_to_string(f.root.join("version.txt")).unwrap(),
        "1.0.0"
    );
}

#[test]
fn invalid_signature_is_rejected_without_installation() {
    let f = Fixture::new();
    let package = f.package(&f.dir.path().join("build"), "2.0.0", false);
    f.signed_package(&f.job, &package, "2.0.0");
    fs::write(f.job.join("manifest.sig"), [0u8; 64]).unwrap();
    let (mut host, _updater, ready) = f.launch();
    assert_eq!(
        ready.recv_timeout(Duration::from_secs(30)).unwrap().trim(),
        "ERROR"
    );
    assert!(host.0.try_wait().unwrap().is_none());
    assert_eq!(
        fs::read_to_string(f.root.join("version.txt")).unwrap(),
        "1.0.0"
    );
    assert!(!f.root.join("restarted.txt").exists());
}

#[test]
fn concurrent_updater_is_rejected() {
    let f = Fixture::new();
    let package = f.package(&f.dir.path().join("build"), "2.0.0", false);
    f.signed_package(&f.job, &package, "2.0.0");
    let (_host, _updater, ready) = f.launch();
    assert_eq!(
        ready.recv_timeout(Duration::from_secs(30)).unwrap().trim(),
        "READY"
    );
    let (_other_host, _other_updater, second) = f.launch();
    assert_eq!(
        second.recv_timeout(Duration::from_secs(30)).unwrap().trim(),
        "ERROR"
    );
    assert_eq!(
        fs::read_to_string(f.root.join("version.txt")).unwrap(),
        "1.0.0"
    );
}
