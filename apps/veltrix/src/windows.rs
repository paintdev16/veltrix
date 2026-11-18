use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use serde::Deserialize;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        process::CommandExt,
    },
    path::{Component, Path, PathBuf, Prefix},
    process::{Command, Stdio},
};
use veltrix_core::{Channel, Policy, UpdateEvent, UpdateRequest, VerifiedManifest};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        QueryFullProcessImageNameW, WaitForSingleObject,
    },
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    app_id: String,
    name: String,
    executable: String,
    channel: Channel,
    public_key: String,
    max_package_size: u64,
}

pub struct Job {
    pub name: String,
    pub current: Version,
    pub target: Version,
    pub icon: Option<PathBuf>,
    root: PathBuf,
    executable: PathBuf,
    version_file: PathBuf,
    parent: Parent,
    control: PathBuf,
    source: File,
    manifest: VerifiedManifest,
    rollback: Option<(File, VerifiedManifest)>,
    rollback_warning: Option<String>,
    _lock: File,
    installed_hash: String,
    log: File,
}

fn read_limited(reader: impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    reader.take(limit + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= limit,
        "archivo o solicitud demasiado grande"
    );
    Ok(data)
}

fn safe_path(path: &Path, folder: bool) -> Result<()> {
    let mut parts = path.components();
    ensure!(
        matches!(parts.next(), Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_))),
        "se requiere ruta absoluta en una unidad local"
    );
    ensure!(
        matches!(parts.next(), Some(Component::RootDir)),
        "se requiere la raíz de la unidad"
    );
    let mut cursor = PathBuf::new();
    for part in path.components() {
        ensure!(
            !matches!(part, Component::ParentDir | Component::CurDir),
            "ruta con navegación relativa"
        );
        if let Component::Normal(name) = part {
            let name = name.to_str().context("nombre de archivo no admitido")?;
            ensure!(safe_name(name), "componente de ruta no admitido");
        }
        cursor.push(part);
        if matches!(part, Component::Normal(_)) {
            let meta = fs::symlink_metadata(&cursor)?;
            ensure!(
                meta.file_attributes() & 0x400 == 0,
                "no se permiten puntos de reanálisis"
            );
            ensure!(
                meta.is_dir() || cursor == path,
                "la ruta intermedia no es un directorio"
            );
        }
    }
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        if folder {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        "tipo de ruta incorrecto"
    );
    Ok(())
}

fn safe_name(name: &str) -> bool {
    if name.is_empty()
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
    {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    !matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) && !["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}

fn version_at(path: &Path) -> Result<Version> {
    safe_path(path, false)?;
    Ok(std::str::from_utf8(&read_limited(File::open(path)?, 256)?)?
        .trim()
        .parse()?)
}

struct Parent(HANDLE);

// The handle is owned, used only for queries and waits, and closed once by Drop.
unsafe impl Send for Parent {}

impl Parent {
    fn open(pid: u32, expected: &Path) -> Result<Self> {
        ensure!(pid != 0 && pid != std::process::id(), "PID inválido");
        let raw = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if raw.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        let handle = Self(raw);
        let mut buf = vec![0u16; 32768];
        let mut len = buf.len() as u32;
        if unsafe { QueryFullProcessImageNameW(raw, 0, buf.as_mut_ptr(), &mut len) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let actual = PathBuf::from(String::from_utf16(&buf[..len as usize])?);
        ensure!(
            actual.canonicalize()? == expected.canonicalize()?,
            "el proceso no pertenece a esta instalación"
        );
        Ok(handle)
    }

    fn wait(&self, millis: u32) -> Result<()> {
        match unsafe { WaitForSingleObject(self.0, millis) } {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_TIMEOUT => bail!("la aplicación no se cerró dentro del plazo"),
            _ => Err(io::Error::last_os_error().into()),
        }
    }
}

impl Drop for Parent {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

pub fn prepare() -> Result<Job> {
    let input = read_limited(io::stdin().lock(), 16 * 1024)?;
    let request: UpdateRequest = serde_json::from_slice(&input)?;
    ensure!(request.protocol == 1, "protocolo incompatible");

    let updater = std::env::current_exe()?;
    safe_path(&updater, false)?;
    let control = updater
        .parent()
        .context("falta el directorio del actualizador")?;
    ensure!(
        control.file_name().is_some_and(|name| name == ".updater"),
        "el actualizador debe estar en .updater"
    );
    let root = control
        .parent()
        .context("falta directorio de instalación")?
        .to_owned();
    ensure!(
        root.parent().is_some(),
        "la instalación no puede estar en la raíz"
    );
    let config_file = control.join("app.json");
    safe_path(&config_file, false)?;
    let config: Config =
        serde_json::from_slice(&read_limited(File::open(config_file)?, 16 * 1024)?)?;
    ensure!(
        !config.app_id.is_empty() && config.app_id == request.app_id,
        "identidad no reconocida"
    );
    ensure!(
        safe_name(&config.executable) && config.executable.to_ascii_lowercase().ends_with(".exe"),
        "ejecutable local no admitido"
    );
    let executable = root.join(config.executable);
    safe_path(&executable, false)?;
    let lock_path = control.join("update.lock");
    safe_path(&lock_path, false)?;
    let lock = OpenOptions::new().read(true).write(true).open(lock_path)?;
    lock.try_lock()
        .context("ya hay otra actualización en curso")?;

    let parent = Parent::open(request.parent_pid, &executable)?;
    safe_path(&request.job_dir, true)?;
    let version_file = root.join("version.txt");
    let current = version_at(&version_file)?;
    let policy = Policy {
        app_id: config.app_id,
        channel: config.channel,
        current_version: current.clone(),
        platform: "windows".into(),
        architecture: std::env::consts::ARCH.into(),
        public_key: hex::decode(config.public_key)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("clave Ed25519 inválida"))?,
        max_package_size: config.max_package_size,
    };
    let job_file = |name: &str| -> Result<File> {
        let path = request.job_dir.join(name);
        safe_path(&path, false)?;
        Ok(File::open(path)?)
    };
    let manifest = policy.verify_manifest(
        &read_limited(job_file("manifest.json")?, 1024 * 1024)?,
        &read_limited(job_file("manifest.sig")?, 64)?,
    )?;
    let target = manifest.manifest().version.clone();
    let installed_hash = manifest
        .manifest()
        .installed_sha256
        .as_ref()
        .filter(|hash| hex::decode(hash).is_ok_and(|bytes| bytes.len() == 32))
        .context("falta el SHA-256 firmado del ejecutable instalado")?
        .to_owned();

    let source = job_file("package.exe")?;
    let (rollback, rollback_warning) = match read_rollback(control, &policy) {
        Ok(backup) => (backup, None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    let log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(request.job_dir.join("events.jsonl"))?;
    let icon = control.join("icon.png");
    let icon = safe_path(&icon, false).is_ok().then_some(icon);
    Ok(Job {
        name: config.name,
        current,
        target,
        icon,
        root,
        executable,
        version_file,
        parent,
        control: control.to_owned(),
        source,
        manifest,
        rollback,
        rollback_warning,
        _lock: lock,
        installed_hash,
        log,
    })
}

fn read_rollback(control: &Path, policy: &Policy) -> Result<Option<(File, VerifiedManifest)>> {
    let dir = control.join("rollback");
    match fs::symlink_metadata(&dir) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        other => {
            other?;
        }
    }
    safe_path(&dir, true)?;
    let open = |name: &str| -> Result<File> {
        let path = dir.join(name);
        safe_path(&path, false)?;
        Ok(File::open(path)?)
    };
    let mut previous = policy.clone();
    previous.current_version = "0.0.0-0".parse()?;
    let manifest = previous.verify_manifest(
        &read_limited(open("manifest.json")?, 1024 * 1024)?,
        &read_limited(open("manifest.sig")?, 64)?,
    )?;
    ensure!(
        manifest.manifest().version == policy.current_version,
        "el respaldo no corresponde a la versión instalada"
    );
    ensure!(
        manifest
            .manifest()
            .installed_sha256
            .as_ref()
            .is_some_and(|hash| hex::decode(hash).is_ok_and(|value| value.len() == 32)),
        "el respaldo no contiene el hash del ejecutable"
    );
    Ok(Some((open("package.exe")?, manifest)))
}

struct Staged {
    installer: PathBuf,
    _guard: File,
    dir: tempfile::TempDir,
}

fn stage(
    control: &Path,
    mut source: File,
    manifest: &VerifiedManifest,
    progress: impl Fn(u64, u64),
) -> Result<Staged> {
    safe_path(control, true)?;
    let dir = tempfile::Builder::new()
        .prefix("stage-")
        .tempdir_in(control)?;
    let installer = dir.path().join("update.exe");
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&installer)?;
    let size = manifest.manifest().size;
    let mut bytes = 0;
    let mut buf = [0; 64 * 1024];
    loop {
        let count = source.read(&mut buf)?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        ensure!(bytes <= size, "tamaño del instalador incorrecto");
        output.write_all(&buf[..count])?;
        progress(bytes, size);
    }
    output.sync_all()?;
    drop(output);
    let guard = OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&installer)?;
    veltrix_core::verify_package_reader(&guard, manifest)?;
    Ok(Staged {
        installer,
        _guard: guard,
        dir,
    })
}

fn install(staged: &Staged, root: &Path) -> Result<()> {
    safe_path(root, true)?;
    let mut install_dir = std::ffi::OsString::from("/D=");
    install_dir.push(root);
    let mut command = Command::new(&staged.installer);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .arg("/S")
        .raw_arg(install_dir)
        .current_dir(staged.dir.path());
    wait_installer(&mut command)
}
fn wait_installer(command: &mut Command) -> Result<()> {
    let status = command.status()?;
    ensure!(status.success(), "NSIS terminó con {status}");
    Ok(())
}

fn validate(
    version_file: &Path,
    executable: &Path,
    version: &Version,
    digest: &str,
) -> Result<File> {
    ensure!(
        version_at(version_file)? == *version,
        "NSIS no instaló la versión esperada"
    );
    safe_path(executable, false)?;
    let guard = OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(executable)?;
    veltrix_core::verify_reader_sha256(&guard, digest)?;
    Ok(guard)
}

impl Job {
    pub fn run(mut self, show: impl Fn(&UpdateEvent)) -> bool {
        let mut emit =
            |stage: &str, message: String, terminal: bool, success: bool| -> Result<()> {
                let event = UpdateEvent {
                    stage: stage.into(),
                    message,
                    terminal,
                    success,
                    progress: None,
                };
                serde_json::to_writer(&mut self.log, &event)?;
                writeln!(self.log)?;
                self.log.sync_data()?;
                show(&event);
                Ok(())
            };
        let mut acknowledged = false;
        let result = (|| -> Result<()> {
            if let Some(warning) = self.rollback_warning {
                emit("Respaldo no disponible", warning, false, false)?;
            }
            emit(
                "Verificando integridad",
                "Preparando una copia local del instalador.".into(),
                false,
                false,
            )?;
            let staged = stage(&self.control, self.source, &self.manifest, |done, total| {
                show(&UpdateEvent {
                    stage: "Verificando integridad".into(),
                    message: if done < total {
                        "Copiando el instalador."
                    } else {
                        "Comprobando SHA-256."
                    }
                    .into(),
                    terminal: false,
                    success: false,
                    progress: (done < total).then_some((done, total)),
                });
            })?;
            emit(
                "Esperando cierre de la aplicación",
                "Cierra la aplicación para continuar.".into(),
                false,
                false,
            )?;
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "READY")?;
            stdout.flush()?;
            drop(stdout);
            acknowledged = true;
            self.parent.wait(120_000)?;
            emit(
                "Instalando actualización",
                "Ejecutando el instalador silencioso.".into(),
                false,
                false,
            )?;
            let installation = install(&staged, &self.root).and_then(|()| {
                emit(
                    "Validando instalación",
                    "Comprobando la versión y el ejecutable instalado.".into(),
                    false,
                    false,
                )?;
                validate(
                    &self.version_file,
                    &self.executable,
                    &self.target,
                    &self.installed_hash,
                )
            });
            let restart_guard = match installation {
                Ok(guard) => guard,
                Err(error) => {
                    if let Some((source, backup)) = self.rollback {
                        let restore = (|| -> Result<()> {
                            emit(
                                "Restaurando instalación",
                                "Verificando y aplicando el respaldo firmado.".into(),
                                false,
                                false,
                            )?;
                            let old = stage(&self.control, source, &backup, |_, _| {})?;
                            install(&old, &self.root)?;
                            validate(
                                &self.version_file,
                                &self.executable,
                                &self.current,
                                backup
                                    .manifest()
                                    .installed_sha256
                                    .as_deref()
                                    .context("falta el hash firmado del respaldo")?,
                            )
                            .map(drop)
                        })();
                        match restore {
                            Ok(()) => bail!("{error:#}. Se restauró la versión {}.", self.current),
                            Err(restore) => bail!("{error:#}. La recuperación falló: {restore:#}"),
                        }
                    }
                    return Err(error);
                }
            };
            emit(
                "Reiniciando aplicación",
                "Abriendo la versión actualizada.".into(),
                false,
                false,
            )?;
            Command::new(&self.executable)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .current_dir(&self.root)
                .spawn()?;
            drop(restart_guard);
            emit(
                "Actualización completada",
                format!("La versión {} está instalada.", self.target),
                true,
                true,
            )?;
            Ok(())
        })();
        if let Err(error) = result {
            if !acknowledged {
                let _ = writeln!(io::stdout().lock(), "ERROR");
            }
            let message = format!("{error:#}");
            eprintln!("{message}");
            if emit("Actualización fallida", message.clone(), true, false).is_err() {
                show(&UpdateEvent {
                    stage: "Actualización fallida".into(),
                    message,
                    terminal: true,
                    success: false,
                    progress: None,
                });
            }
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires the MAKENSIS environment variable"]
    fn signed_nsis_update_writes_only_to_chosen_installation() {
        use ed25519_dalek::{Signer, SigningKey};
        use sha2::{Digest, Sha256};
        let makensis = std::env::var_os("MAKENSIS").expect("absolute MakeNSIS path");
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().join("Application With Spaces");
        let control = root.join(".updater");
        fs::create_dir_all(&control).unwrap();
        let user_data = root.join("userdata.txt");
        fs::write(&user_data, "preserve me").unwrap();
        let native =
            PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/where.exe");
        let exe_bytes = fs::read(&native).unwrap();
        fs::write(workspace.path().join("build-app.exe"), &exe_bytes).unwrap();
        fs::write(workspace.path().join("build-version.txt"), "2.0.0").unwrap();
        fs::write(
            workspace.path().join("test.nsi"),
            r#"
Unicode true
Name "Updater test"
OutFile "package.exe"
InstallDir "$TEMP\veltrix-must-not-use"
RequestExecutionLevel user
Section
    SetOutPath "$INSTDIR"
    ClearErrors
    File /oname=App.exe "build-app.exe"
    File /oname=version.txt "build-version.txt"
    IfErrors 0 done
    SetErrorLevel 7
    Abort
done:
SectionEnd
"#,
        )
        .unwrap();
        assert!(
            Command::new(makensis)
                .current_dir(workspace.path())
                .args(["/V2", "test.nsi"])
                .status()
                .unwrap()
                .success()
        );
        let package = workspace.path().join("package.exe");
        let bytes = fs::read(&package).unwrap();
        let key = SigningKey::from_bytes(&[61; 32]);
        let policy = Policy {
            app_id: "test".into(),
            channel: Channel::Stable,
            current_version: "1.0.0".parse().unwrap(),
            platform: "windows".into(),
            architecture: std::env::consts::ARCH.into(),
            public_key: key.verifying_key().to_bytes(),
            max_package_size: 8 * 1024 * 1024,
        };
        let target = "2.0.0".parse().unwrap();
        let installed_hash = hex::encode(Sha256::digest(&exe_bytes));
        let manifest = serde_json::to_vec(&veltrix_core::Manifest {
            app_id: policy.app_id.clone(),
            version: Version::clone(&target),
            channel: policy.channel,
            platform: policy.platform.clone(),
            architecture: policy.architecture.clone(),
            size: bytes.len() as u64,
            url: "https://example.com/update.exe".into(),
            sha256: hex::encode(Sha256::digest(&bytes)),
            installed_sha256: Some(installed_hash.clone()),
        })
        .unwrap();
        let signed = policy
            .verify_manifest(&manifest, &key.sign(&manifest).to_bytes())
            .unwrap();
        let staged = stage(&control, File::open(package).unwrap(), &signed, |_, _| {}).unwrap();
        install(&staged, &root).unwrap();
        let application = root.join("App.exe");
        let guard = validate(
            &root.join("version.txt"),
            &application,
            &target,
            &installed_hash,
        )
        .unwrap();
        assert_eq!(fs::read_to_string(user_data).unwrap(), "preserve me");
        let result = Command::new(application)
            .arg("/?")
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(result.success());
        drop(guard);
    }

    #[test]
    fn installer_failure_is_not_success() {
        let executable = std::env::current_exe().unwrap();
        assert!(
            wait_installer(
                Command::new(&executable)
                    .arg("--list")
                    .stdout(Stdio::null())
            )
            .is_ok()
        );
        let error = wait_installer(
            Command::new(&executable)
                .arg("--not-a-libtest-option")
                .stderr(Stdio::null()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("NSIS terminó con"));
    }

    #[test]
    fn rejects_unsafe_executable_names() {
        for name in [
            "../other.exe",
            "a.exe:stream",
            "NUL.exe",
            "COM¹.exe",
            "a.exe.",
            "a.exe ",
            "a\\b.exe",
        ] {
            assert!(!safe_name(name), "{name}");
        }
        assert!(safe_name("My App.exe"));
    }
    #[test]
    fn rejects_concurrent_update_locks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.lock");
        fs::write(&path, []).unwrap();
        let first = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let second = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        first.try_lock().unwrap();
        assert!(second.try_lock().is_err());
        drop(first);
        second.try_lock().unwrap();
    }
    #[test]
    fn waits_for_the_exact_process_handle() {
        let cmd = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/cmd.exe");
        let mut child = Command::new(&cmd)
            .args(["/D", "/Q", "/K"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let handle = Parent::open(child.id(), &cmd).unwrap();
        assert!(handle.wait(0).is_err());
        child.kill().unwrap();
        handle.wait(5000).unwrap();
        child.wait().unwrap();
    }
}
