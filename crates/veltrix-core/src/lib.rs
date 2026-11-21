//! Reusable update discovery and package verification primitives.
//! Trust anchors and installation policy must come from local, protected configuration.
use ed25519_dalek::{Signature, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid update: {0}")]
    Invalid(&'static str),
    #[error("update cancelled")]
    Cancelled,
    #[error("updater did not become ready before the deadline")]
    ReadinessTimeout,
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Stable,
    Beta,
    Nightly,
}

/// A signed description of one full installer. The signature covers the exact JSON bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub app_id: String,
    pub version: Version,
    pub channel: Channel,
    pub platform: String,
    pub architecture: String,
    pub size: u64,
    pub url: String,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_sha256: Option<String>,
}

/// A manifest authenticated and accepted by a local policy.
#[derive(Debug, Clone)]
pub struct VerifiedManifest(Manifest);
impl VerifiedManifest {
    pub fn manifest(&self) -> &Manifest {
        &self.0
    }
}

/// Locally provisioned trust policy. Never populate this from a remote manifest.
#[derive(Debug, Clone)]
pub struct Policy {
    pub app_id: String,
    pub channel: Channel,
    pub current_version: Version,
    pub platform: String,
    pub architecture: String,
    pub public_key: [u8; 32],
    pub max_package_size: u64,
}
impl Policy {
    /// Verify against this policy's key or additional locally trusted keys.
    /// Keys must never come from a remote manifest.
    pub fn verify_manifest_with_keys(
        &self,
        bytes: &[u8],
        signature: &[u8],
        additional_keys: &[[u8; 32]],
    ) -> Result<VerifiedManifest> {
        if let Ok(manifest) = self.verify_manifest(bytes, signature) {
            return Ok(manifest);
        }
        for key in additional_keys {
            let mut candidate = self.clone();
            candidate.public_key = *key;
            if let Ok(manifest) = candidate.verify_manifest(bytes, signature) {
                return Ok(manifest);
            }
        }
        Err(Error::Invalid("manifest signature or policy rejected"))
    }
    /// Verify Ed25519 first, then parse and enforce identity, version and platform.
    pub fn verify_manifest(&self, bytes: &[u8], signature: &[u8]) -> Result<VerifiedManifest> {
        if bytes.len() > 1024 * 1024 {
            return Err(Error::Invalid("manifest too large"));
        }
        let key = VerifyingKey::from_bytes(&self.public_key)
            .map_err(|_| Error::Invalid("invalid public key"))?;
        let sig =
            Signature::from_slice(signature).map_err(|_| Error::Invalid("invalid signature"))?;
        key.verify_strict(bytes, &sig)
            .map_err(|_| Error::Invalid("signature verification failed"))?;
        let m: Manifest = serde_json::from_slice(bytes)?;
        if m.app_id != self.app_id
            || m.platform != self.platform
            || m.architecture != self.architecture
        {
            return Err(Error::Invalid("incompatible package"));
        }
        if !m.version.cmp_precedence(&self.current_version).is_gt()
            || m.channel != self.channel
            || !version_in_channel(&m.version, m.channel)
        {
            return Err(Error::Invalid("version or channel rejected"));
        }
        if m.size == 0 || m.size > self.max_package_size {
            return Err(Error::Invalid("invalid package size"));
        }
        if hex::decode(&m.sha256).map_or(true, |v| v.len() != 32) {
            return Err(Error::Invalid("invalid SHA-256"));
        }
        let url = Url::parse(&m.url).map_err(|_| Error::Invalid("invalid URL"))?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(Error::Invalid("only HTTPS package URLs are allowed"));
        }
        Ok(VerifiedManifest(m))
    }
}

fn version_in_channel(version: &Version, channel: Channel) -> bool {
    let pre = version.pre.as_str();
    let starts = |label: &str| {
        pre == label
            || pre
                .strip_prefix(label)
                .is_some_and(|tail| tail.starts_with(['.', '-']))
    };
    match channel {
        Channel::Stable => pre.is_empty(),
        Channel::Beta => starts("beta"),
        Channel::Nightly => starts("nightly"),
    }
}

/// HTTP transport with finite timeouts and HTTPS-only redirects.
pub struct UpdateClient(reqwest::blocking::Client);

pub fn http_client() -> Result<UpdateClient> {
    Ok(UpdateClient(
        reqwest::blocking::Client::builder()
            .user_agent("veltrix/0.1")
            .https_only(true)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()?,
    ))
}

fn get(client: &UpdateClient, url: &str) -> Result<reqwest::blocking::Response> {
    let response = client.0.get(url).send()?.error_for_status()?;
    if !response.status().is_success() {
        return Err(Error::Invalid("unexpected HTTP response"));
    }
    Ok(response)
}

/// Fetch and authenticate the platform-specific manifest and detached signature.
pub fn release_manifest(
    client: &UpdateClient,
    policy: &Policy,
    release: &Release,
) -> Result<(VerifiedManifest, Vec<u8>, Vec<u8>)> {
    release_manifest_with_keys(client, policy, release, &[])
}

/// Fetch a release manifest and accept any locally provisioned rotation key.
pub fn release_manifest_with_keys(
    client: &UpdateClient,
    policy: &Policy,
    release: &Release,
    additional_keys: &[[u8; 32]],
) -> Result<(VerifiedManifest, Vec<u8>, Vec<u8>)> {
    let name = format!("update-{}-{}.json", policy.platform, policy.architecture);
    let fetch = |name: &str, limit: u64| -> Result<Vec<u8>> {
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == name)
            .ok_or(Error::Invalid("missing release asset"))?;
        let mut bytes = Vec::new();
        get(client, &asset.browser_download_url)?
            .take(limit + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(Error::Invalid("release asset exceeds size limit"));
        }
        Ok(bytes)
    };
    let bytes = fetch(&name, 1024 * 1024)?;
    let signature = fetch(&format!("{name}.sig"), 64)?;
    let verified = policy.verify_manifest_with_keys(&bytes, &signature, additional_keys)?;
    if Version::parse(
        release
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&release.tag_name),
    )
    .ok()
    .as_ref()
        != Some(&verified.manifest().version)
    {
        return Err(Error::Invalid("signed version differs from release tag"));
    }
    Ok((verified, bytes, signature))
}

/// GitHub release metadata; use asset URLs to retrieve signed manifests separately.
#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub prerelease: bool,
    pub draft: bool,
    pub assets: Vec<Asset>,
}
#[derive(Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}
/// Fetch up to 1,000 releases from GitHub, with a bounded response per page.
/// Repository components are restricted to safe characters.
pub fn github_releases(client: &UpdateClient, owner: &str, repo: &str) -> Result<Vec<Release>> {
    let valid = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'.')
            && s != "."
            && s != ".."
    };
    if !valid(owner) || !valid(repo) {
        return Err(Error::Invalid("invalid repository"));
    }
    let mut releases = Vec::new();
    for page in 1..=10 {
        let response = get(
            client,
            &format!(
                "https://api.github.com/repos/{owner}/{repo}/releases?per_page=100&page={page}"
            ),
        )?;
        let mut bytes = Vec::new();
        const MAX_PAGE_BYTES: u64 = 8 * 1024 * 1024;
        response.take(MAX_PAGE_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_PAGE_BYTES {
            return Err(Error::Invalid("release metadata exceeds size limit"));
        }
        let page: Vec<Release> = serde_json::from_slice(&bytes)?;
        let last = page.len() < 100;
        releases.extend(page);
        if last {
            break;
        }
    }
    Ok(releases)
}
/// Select the highest SemVer tag satisfying the channel; beta accepts beta prereleases,
/// nightly accepts nightly prereleases, stable accepts only final versions.
pub fn select_release<'a>(
    releases: &'a [Release],
    current: &Version,
    channel: Channel,
) -> Option<&'a Release> {
    releases
        .iter()
        .filter_map(|r| {
            let v = Version::parse(r.tag_name.strip_prefix('v').unwrap_or(&r.tag_name)).ok()?;
            let allowed =
                r.prerelease == (channel != Channel::Stable) && version_in_channel(&v, channel);
            (!r.draft && allowed && v.cmp_precedence(current).is_gt()).then_some((v, r))
        })
        .max_by(|a, b| a.0.cmp_precedence(&b.0))
        .map(|(_, r)| r)
}

/// Download to a new file, enforce the signed size and digest, and remove partial files on failure.
/// Progress reports bytes read and the signed total. Cancellation is cooperative between reads.
pub fn download(
    client: &UpdateClient,
    verified: &VerifiedManifest,
    destination: &Path,
    cancelled: &AtomicBool,
    progress: impl FnMut(u64, u64),
) -> Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    let response = get(client, &verified.manifest().url)?;
    save_download(response, verified, destination, cancelled, progress)
}

fn save_download(
    mut response: impl Read,
    verified: &VerifiedManifest,
    destination: &Path,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<()> {
    let manifest = verified.manifest();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let result = (|| {
        let mut hash = Sha256::new();
        let mut total = 0u64;
        let mut buf = [0u8; 65536];
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let n = response.read(&mut buf)?;
            if n == 0 {
                break;
            }
            total = total
                .checked_add(n as u64)
                .ok_or(Error::Invalid("size overflow"))?;
            if total > manifest.size {
                return Err(Error::Invalid("package exceeds signed size"));
            }
            file.write_all(&buf[..n])?;
            hash.update(&buf[..n]);
            progress(total, manifest.size);
        }
        file.sync_all()?;
        if total != manifest.size
            || hex::encode(hash.finalize()) != manifest.sha256.to_ascii_lowercase()
        {
            return Err(Error::Invalid("package size or SHA-256 mismatch"));
        }
        Ok(())
    })();
    drop(file);
    if result.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    result
}
/// Recheck the bytes on disk immediately before use.
pub fn verify_package(path: &Path, verified: &VerifiedManifest) -> Result<()> {
    verify_package_reader(File::open(path)?, verified)
}

/// Verify an already opened installer without reopening its path.
pub fn verify_package_reader(mut f: impl Read, verified: &VerifiedManifest) -> Result<()> {
    let manifest = verified.manifest();
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size = size
            .checked_add(n as u64)
            .ok_or(Error::Invalid("size overflow"))?;
        if size > manifest.size {
            return Err(Error::Invalid("package too large"));
        }
        hash.update(&buf[..n]);
    }
    if size != manifest.size || hex::encode(hash.finalize()) != manifest.sha256.to_ascii_lowercase()
    {
        return Err(Error::Invalid("package size or SHA-256 mismatch"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    fn fixture() -> (Policy, Vec<u8>, Vec<u8>) {
        let key = SigningKey::from_bytes(&[42; 32]);
        let policy = Policy {
            app_id: "example".into(),
            channel: Channel::Stable,
            current_version: Version::parse("1.0.0").unwrap(),
            platform: "windows".into(),
            architecture: "x86_64".into(),
            public_key: key.verifying_key().to_bytes(),
            max_package_size: 100,
        };
        let m = Manifest {
            app_id: "example".into(),
            version: Version::parse("1.1.0").unwrap(),
            channel: Channel::Stable,
            platform: "windows".into(),
            architecture: "x86_64".into(),
            size: 3,
            url: "https://example.com/update.exe".into(),
            sha256: hex::encode(Sha256::digest(b"abc")),
            installed_sha256: None,
        };
        let bytes = serde_json::to_vec(&m).unwrap();
        let sig = key.sign(&bytes).to_bytes().to_vec();
        (policy, bytes, sig)
    }
    #[test]
    fn release_selection_uses_semver_precedence_and_channel() {
        let release = |tag: &str, prerelease, draft| Release {
            tag_name: tag.into(),
            prerelease,
            draft,
            assets: vec![],
        };
        let releases = [
            release("v1.9.0", false, false),
            release("v1.10.0", false, false),
            release("v3.0.0", false, true),
            release("v2.0.0-beta.9", true, false),
            release("v2.0.0-beta.10", true, false),
            release("v2.0.0-betatest", true, false),
            release("v2.0.0-nightly.1", true, false),
        ];
        let current = Version::parse("1.0.0").unwrap();
        assert_eq!(
            select_release(&releases, &current, Channel::Stable)
                .unwrap()
                .tag_name,
            "v1.10.0"
        );
        assert_eq!(
            select_release(&releases, &current, Channel::Beta)
                .unwrap()
                .tag_name,
            "v2.0.0-beta.10"
        );
        assert_eq!(
            select_release(&releases, &current, Channel::Nightly)
                .unwrap()
                .tag_name,
            "v2.0.0-nightly.1"
        );
    }

    #[test]
    fn interrupted_download_removes_partial_file() {
        let (policy, bytes, signature) = fixture();
        let manifest = policy.verify_manifest(&bytes, &signature).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.exe");
        let response = b"a".as_slice();
        assert!(
            save_download(
                response,
                &manifest,
                &path,
                &AtomicBool::new(false),
                |_, _| {}
            )
            .is_err()
        );
        assert!(!path.exists());

        let cancelled = AtomicBool::new(false);
        let response = b"abc".as_slice();
        let result = save_download(response, &manifest, &path, &cancelled, |done, total| {
            assert_eq!((done, total), (3, 3));
            cancelled.store(true, Ordering::Relaxed);
        });
        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(!path.exists());

        save_download(
            b"abc".as_slice(),
            &manifest,
            &path,
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();
        verify_package(&path, &manifest).unwrap();
    }

    #[test]
    fn signatures_and_identity() {
        let (mut p, b, s) = fixture();
        assert!(p.verify_manifest(&b, &s).is_ok());
        let mut bad = s.clone();
        bad[0] ^= 1;
        assert!(p.verify_manifest(&b, &bad).is_err());
        p.architecture = "aarch64".into();
        assert!(p.verify_manifest(&b, &s).is_err());
        p.architecture = "x86_64".into();
        p.app_id = "other".into();
        assert!(p.verify_manifest(&b, &s).is_err());
    }
    #[test]
    fn rotation_keys_are_local_and_still_enforce_policy() {
        let (mut policy, bytes, signature) = fixture();
        let old = policy.public_key;
        policy.public_key = SigningKey::from_bytes(&[43; 32]).verifying_key().to_bytes();
        assert!(policy.verify_manifest(&bytes, &signature).is_err());
        assert!(
            policy
                .verify_manifest_with_keys(&bytes, &signature, &[old])
                .is_ok()
        );
        policy.app_id = "another-app".into();
        assert!(
            policy
                .verify_manifest_with_keys(&bytes, &signature, &[old])
                .is_err()
        );
    }
    #[test]
    #[ignore = "requires access to the public GitHub API"]
    fn live_github_release_query() {
        let repository = std::env::var("VELTRIX_TEST_REPOSITORY")
            .unwrap_or_else(|_| "paintdev16/veltrix".into());
        let (owner, name) = repository.split_once('/').expect("owner/repository");
        let releases = github_releases(&http_client().unwrap(), owner, name).unwrap();
        eprintln!(
            "GitHub returned {} release(s) for {repository}",
            releases.len()
        );
    }
    #[test]
    fn version_and_channel() {
        let (mut p, b, s) = fixture();
        p.current_version = Version::parse("2.0.0").unwrap();
        assert!(p.verify_manifest(&b, &s).is_err());
        p.current_version = Version::parse("1.0.0").unwrap();
        p.channel = Channel::Beta;
        assert!(p.verify_manifest(&b, &s).is_err());
    }
    #[test]
    fn rejects_wrong_hash() {
        let (p, b, s) = fixture();
        let m = p.verify_manifest(&b, &s).unwrap();
        let path = std::env::temp_dir().join(format!("updater-test-{}", std::process::id()));
        std::fs::write(&path, b"xyz").unwrap();
        assert!(verify_package(&path, &m).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn terminal_outcome_requires_a_complete_terminal_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, b"{\"stage\":\"install\",\"message\":\"working\",\"terminal\":false,\"success\":false}\n{\"stage\":\"unfinished\"").unwrap();
        let events = read_update_events(dir.path()).unwrap();
        assert_eq!(update_outcome(&events), UpdateOutcome::InProgress);
        std::fs::write(
            &path,
            b"{\"stage\":\"failed\",\"message\":\"error\",\"terminal\":true,\"success\":false}\n",
        )
        .unwrap();
        assert_eq!(
            update_outcome(&read_update_events(dir.path()).unwrap()),
            UpdateOutcome::Failed
        );
        std::fs::write(
            &path,
            b"{\"stage\":\"done\",\"message\":\"launched\",\"terminal\":true,\"success\":true}\n",
        )
        .unwrap();
        assert_eq!(
            update_outcome(&read_update_events(dir.path()).unwrap()),
            UpdateOutcome::Succeeded
        );
    }

    #[cfg(windows)]
    #[test]
    fn readiness_timeout_terminates_stalled_updater() {
        use std::process::{Command, Stdio};
        let mut child = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 10",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        assert!(matches!(
            wait_for_ready_timeout(&mut child, Duration::from_millis(100)),
            Err(Error::ReadinessTimeout)
        ));
        assert!(child.try_wait().unwrap().is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn readiness_timeout_terminates_stalled_updater() {
        use std::process::{Command, Stdio};
        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 10"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        assert!(matches!(
            wait_for_ready_timeout(&mut child, Duration::from_millis(100)),
            Err(Error::ReadinessTimeout)
        ));
        assert!(child.try_wait().unwrap().is_some());
    }
}

/// Versioned handoff sent over the child's standard input. Paths are confined to
/// the job directory; installation policy is loaded independently by the updater.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRequest {
    pub protocol: u32,
    pub app_id: String,
    pub parent_pid: u32,
    pub job_dir: std::path::PathBuf,
}

/// Handoff protocol supported by existing hosts. It verifies process creation only.
pub const LEGACY_PROTOCOL: u32 = 1;
/// Handoff protocol requiring the relaunched application to report readiness.
pub const CONFIRMED_STARTUP_PROTOCOL: u32 = 2;
/// Environment variable passed to the application only during a confirmed restart.
pub const STARTUP_ACK_ENV: &str = "VELTRIX_STARTUP_ACK";

/// Call once after the main window and essential application state are ready.
/// Returns false during ordinary launches. Do not call this at process entry.
pub fn confirm_startup() -> Result<bool> {
    let Some(path) = std::env::var_os(STARTUP_ACK_ENV) else {
        return Ok(false);
    };
    let path = Path::new(&path);
    if !path.is_absolute() || path.file_name().and_then(|name| name.to_str()) != Some("ready") {
        return Err(Error::Invalid("invalid startup acknowledgement path"));
    }
    OpenOptions::new().write(true).create_new(true).open(path)?;
    Ok(true)
}

impl UpdateRequest {
    /// Launch the trusted updater and send a handoff. Read one line from the
    /// returned child's stdout and exit only after receiving "READY".
    pub fn launch(&self, updater: &Path) -> Result<std::process::Child> {
        use std::process::{Command, Stdio};
        if !updater.is_absolute() {
            return Err(Error::Invalid("updater path must be absolute"));
        }
        let payload = serde_json::to_vec(self)?;
        let mut command = Command::new(updater);
        command.current_dir(
            updater
                .parent()
                .ok_or(Error::Invalid("missing updater directory"))?,
        );
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        if let Err(error) = child.stdin.take().expect("piped stdin").write_all(&payload) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }
        Ok(child)
    }
}

/// The final result is the last JSON line in the job's events.jsonl.
#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateEvent {
    pub stage: String,
    pub message: String,
    pub terminal: bool,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<(u64, u64)>,
}

/// Persisted result of an update attempt. Startup acknowledgement is required
/// for success when the host requests protocol 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOutcome {
    InProgress,
    Succeeded,
    Failed,
}

/// Interpret the latest complete event; an incomplete log is never success.
pub fn update_outcome(events: &[UpdateEvent]) -> UpdateOutcome {
    match events.last() {
        Some(event) if event.terminal && event.success => UpdateOutcome::Succeeded,
        Some(event) if event.terminal => UpdateOutcome::Failed,
        _ => UpdateOutcome::InProgress,
    }
}

/// Validate an installed file against an authenticated SHA-256 digest.
pub fn verify_file_sha256(path: &Path, expected: &str) -> Result<()> {
    verify_reader_sha256(File::open(path)?, expected)
}

/// Validate a file from a retained handle against an authenticated SHA-256 digest.
pub fn verify_reader_sha256(mut file: impl Read, expected: &str) -> Result<()> {
    let mut digest = Sha256::new();
    std::io::copy(&mut file, &mut digest)?;
    if !hex::encode(digest.finalize()).eq_ignore_ascii_case(expected) {
        return Err(Error::Invalid("installed executable SHA-256 mismatch"));
    }
    Ok(())
}

/// Wait up to three minutes for the updater to verify the request and retain the
/// parent process handle. On timeout the updater is terminated while the host is
/// still alive, so it cannot begin installation after the caller abandons it.
pub fn wait_for_ready(child: &mut std::process::Child) -> Result<()> {
    wait_for_ready_timeout(child, Duration::from_secs(180))
}

/// Like [`wait_for_ready`], with a caller-supplied deadline.
pub fn wait_for_ready_timeout(child: &mut std::process::Child, timeout: Duration) -> Result<()> {
    let stdout = child
        .stdout
        .take()
        .ok_or(Error::Invalid("missing readiness pipe"))?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        use std::io::BufRead;
        let mut line = String::new();
        let result = std::io::BufReader::new(stdout.take(16)).read_line(&mut line);
        let _ = sender.send(result.map(|_| line));
    });
    let line = match receiver.recv_timeout(timeout) {
        Ok(result) => result?,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            child.kill()?;
            child.wait()?;
            return Err(Error::ReadinessTimeout);
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            return Err(Error::Invalid("readiness reader failed"));
        }
    };
    if line.trim_end_matches(['\r', '\n']) != "READY" {
        return Err(Error::Invalid("updater did not accept the request"));
    }
    Ok(())
}

/// Return the most recent complete event without treating an in-progress write as terminal.
pub fn read_update_events(job_dir: &Path) -> Result<Vec<UpdateEvent>> {
    let mut bytes = Vec::new();
    let cap = 1024 * 1024;
    File::open(job_dir.join("events.jsonl"))?
        .take(cap + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return Err(Error::Invalid("update log exceeds size limit"));
    }
    bytes
        .split_inclusive(|byte| *byte == b'\n')
        .filter(|line| line.ends_with(b"\n"))
        .map(|line| Ok(serde_json::from_slice(line)?))
        .collect()
}
