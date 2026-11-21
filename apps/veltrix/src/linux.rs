use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;
use std::{
    fs::{self, File, OpenOptions, Permissions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use tar::{Archive, EntryType};
use veltrix_core::{
    CONFIRMED_STARTUP_PROTOCOL, Channel, LEGACY_PROTOCOL, Policy, STARTUP_ACK_ENV, UpdateEvent,
    UpdateRequest, VerifiedManifest,
};

const PACKAGE_FILENAME: &str = "package.tar.gz";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    app_id: String,
    name: String,
    executable: String,
    channel: Channel,
    public_key: String,
    #[serde(default)]
    additional_public_keys: Vec<String>,
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
    confirm_restart: bool,
    max_extracted_size: u64,
    _lock: File,
    _directories: [File; 3],
    installed_hash: String,
    log: File,
}

fn current_uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn check_protected(meta: &fs::Metadata) -> Result<()> {
    ensure!(
        meta.uid() == current_uid(),
        "el recurso no pertenece al usuario actual"
    );
    ensure!(
        meta.mode() & 0o022 == 0,
        "permisos inseguros: escritura permitida para grupo u otros"
    );
    Ok(())
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

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| c == '/' || c == '\0' || c.is_control())
}

fn safe_path(path: &Path, folder: bool) -> Result<()> {
    ensure!(path.is_absolute(), "se requiere ruta absoluta");
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
                !meta.file_type().is_symlink(),
                "no se permiten enlaces simbólicos"
            );
            ensure!(
                meta.is_dir() || cursor == path,
                "la ruta intermedia no es un directorio"
            );
        }
    }
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "no se permiten enlaces simbólicos"
    );
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

fn open_regular_nofollow(path: &Path) -> Result<File> {
    safe_path(path, false)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    ensure!(file.metadata()?.is_file(), "el recurso no es un archivo");
    Ok(file)
}

fn retained_directory(path: &Path) -> Result<File> {
    safe_path(path, true)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_dir(), "directorio protegido no válido");
    check_protected(&metadata)?;
    Ok(file)
}

fn version_at(path: &Path) -> Result<Version> {
    let file = open_regular_nofollow(path)?;
    Ok(std::str::from_utf8(&read_limited(file, 256)?)?
        .trim()
        .parse()?)
}

struct Parent(OwnedFd);

impl Parent {
    fn open(pid: u32, expected: &Path) -> Result<Self> {
        ensure!(
            pid != 0 && pid != std::process::id() && pid <= libc::c_int::MAX as u32,
            "PID inválido"
        );
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::c_int, 0u32) };
        if raw < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let handle = Self(unsafe { OwnedFd::from_raw_fd(raw as libc::c_int) });
        let proc_dir = PathBuf::from(format!("/proc/{pid}"));
        ensure!(
            fs::metadata(&proc_dir)?.uid() == current_uid(),
            "el proceso no pertenece al usuario actual"
        );
        let actual = fs::read_link(proc_dir.join("exe"))?;
        ensure!(
            actual == expected.canonicalize()?,
            "el proceso no pertenece a esta instalación"
        );
        Ok(handle)
    }

    fn wait(&self, millis: u32) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(u64::from(millis));
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout_ms = remaining.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
            let mut pfd = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            if ready > 0 {
                return Ok(());
            }
            if ready == 0 {
                bail!("la aplicación no se cerró dentro del plazo");
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.into());
        }
    }
}

pub fn prepare() -> Result<Job> {
    let input = read_limited(io::stdin().lock(), 16 * 1024)?;
    let request: UpdateRequest = serde_json::from_slice(&input)?;
    ensure!(
        matches!(
            request.protocol,
            LEGACY_PROTOCOL | CONFIRMED_STARTUP_PROTOCOL
        ),
        "protocolo incompatible"
    );

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
    let directories = [
        retained_directory(&root)?,
        retained_directory(control)?,
        retained_directory(&request.job_dir)?,
    ];
    let config_file = control.join("app.json");
    let config: Config = serde_json::from_slice(&read_limited(
        open_regular_nofollow(&config_file)?,
        16 * 1024,
    )?)?;
    ensure!(
        !config.app_id.is_empty() && config.app_id == request.app_id,
        "identidad no reconocida"
    );
    ensure!(
        safe_name(&config.executable) && config.executable != ".updater",
        "ejecutable local no admitido"
    );
    let executable = root.join(config.executable);
    safe_path(&executable, false)?;
    let lock_path = control.join("update.lock");
    safe_path(&lock_path, false)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(lock_path)?;
    let lock_meta = lock.metadata()?;
    ensure!(lock_meta.is_file(), "bloqueo no válido");
    check_protected(&lock_meta)?;
    lock.try_lock()
        .context("ya hay otra actualización en curso")?;

    let parent = Parent::open(request.parent_pid, &executable)?;
    safe_path(&request.job_dir, true)?;
    let version_file = root.join("version.txt");
    let current = version_at(&version_file)?;
    ensure!(
        config.additional_public_keys.len() <= 8,
        "demasiadas claves confiables"
    );
    let mut trusted_keys = vec![
        hex::decode(&config.public_key)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("clave Ed25519 inválida"))?,
    ];
    for key in &config.additional_public_keys {
        trusted_keys.push(
            hex::decode(key)?
                .try_into()
                .map_err(|_| anyhow::anyhow!("clave Ed25519 inválida"))?,
        );
    }
    let policy = Policy {
        app_id: config.app_id,
        channel: config.channel,
        current_version: current.clone(),
        platform: "linux".into(),
        architecture: std::env::consts::ARCH.into(),
        public_key: trusted_keys[0],
        max_package_size: config.max_package_size,
    };
    let job_file =
        |name: &str| -> Result<File> { open_regular_nofollow(&request.job_dir.join(name)) };
    let manifest = verify_with_keys(
        &policy,
        &trusted_keys,
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

    let source = job_file(PACKAGE_FILENAME)?;
    let (rollback, rollback_warning) = match read_rollback(control, &policy, &trusted_keys) {
        Ok(backup) => (backup, None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    let log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(request.job_dir.join("events.jsonl"))?;
    let icon = control.join("icon.png");
    let icon = safe_path(&icon, false).is_ok().then_some(icon);
    let max_extracted_size = config
        .max_package_size
        .saturating_mul(8)
        .max(64 * 1024 * 1024);
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
        confirm_restart: request.protocol == CONFIRMED_STARTUP_PROTOCOL,
        max_extracted_size,
        _lock: lock,
        _directories: directories,
        installed_hash,
        log,
    })
}

fn verify_with_keys(
    policy: &Policy,
    keys: &[[u8; 32]],
    bytes: &[u8],
    signature: &[u8],
) -> Result<VerifiedManifest> {
    for key in keys {
        let mut candidate = policy.clone();
        candidate.public_key = *key;
        if let Ok(manifest) = candidate.verify_manifest(bytes, signature) {
            return Ok(manifest);
        }
    }
    bail!("el manifiesto no cumple la política o su firma no coincide con las claves confiables")
}

fn read_rollback(
    control: &Path,
    policy: &Policy,
    keys: &[[u8; 32]],
) -> Result<Option<(File, VerifiedManifest)>> {
    let dir = control.join("rollback");
    match fs::symlink_metadata(&dir) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        other => {
            other?;
        }
    }
    let _dir_guard = retained_directory(&dir)?;
    let open = |name: &str| -> Result<File> { open_regular_nofollow(&dir.join(name)) };
    let mut previous = policy.clone();
    previous.current_version = "0.0.0-0".parse()?;
    let manifest = verify_with_keys(
        &previous,
        keys,
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
    Ok(Some((open(PACKAGE_FILENAME)?, manifest)))
}

struct Staged {
    guard: File,
    _dir: tempfile::TempDir,
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
    let package = dir.path().join(PACKAGE_FILENAME);
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&package)?;
    let size = manifest.manifest().size;
    let mut bytes = 0;
    let mut buf = [0; 64 * 1024];
    loop {
        let count = source.read(&mut buf)?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        ensure!(bytes <= size, "tamaño del paquete incorrecto");
        output.write_all(&buf[..count])?;
        progress(bytes, size);
    }
    output.sync_all()?;
    drop(output);
    let guard = open_regular_nofollow(&package)?;
    veltrix_core::verify_package_reader(&guard, manifest)?;
    Ok(Staged { guard, _dir: dir })
}

fn sanitize_archive_path(raw: &Path) -> Result<Option<PathBuf>> {
    let mut clean = PathBuf::new();
    let mut saw_normal = false;
    for component in raw.components() {
        match component {
            Component::CurDir if !saw_normal => {}
            Component::Normal(part) => {
                let name = part.to_str().context("nombre de archivo no admitido")?;
                ensure!(safe_name(name), "componente de ruta no admitido en paquete");
                if !saw_normal {
                    ensure!(name != ".updater", "el paquete no puede modificar .updater");
                    saw_normal = true;
                }
                clean.push(name);
            }
            _ => bail!("ruta no permitida en el paquete"),
        }
    }
    Ok(saw_normal.then_some(clean))
}

fn ensure_safe_subdirs(root: &Path, rel_dir: &Path) -> Result<()> {
    let mut cursor = root.to_owned();
    for component in rel_dir.components() {
        let Component::Normal(name) = component else {
            bail!("ruta de directorio no permitida");
        };
        cursor.push(name);
        match fs::symlink_metadata(&cursor) {
            Ok(meta) => {
                ensure!(
                    !meta.file_type().is_symlink() && meta.is_dir(),
                    "ruta destino inválida al extraer"
                );
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&cursor)?;
                fs::set_permissions(&cursor, Permissions::from_mode(0o755))?;
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn write_atomic(
    parent: &Path,
    target: &Path,
    mode: u32,
    write_fn: impl FnOnce(&mut File) -> Result<()>,
) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(target) {
        ensure!(
            !meta.file_type().is_symlink() && meta.is_file(),
            "el destino existente no es un archivo regular"
        );
    }
    let mut temp = tempfile::Builder::new()
        .prefix(".veltrix-tmp-")
        .tempfile_in(parent)?;
    write_fn(temp.as_file_mut())?;
    temp.as_file()
        .set_permissions(Permissions::from_mode(mode))?;
    temp.as_file().sync_all()?;
    temp.persist(target)
        .map_err(|err| io::Error::other(err.error))?;
    Ok(())
}

fn install(staged: &mut Staged, root: &Path, max_extracted_size: u64) -> Result<()> {
    safe_path(root, true)?;
    staged.guard.seek(SeekFrom::Start(0))?;
    let mut archive = Archive::new(GzDecoder::new(&staged.guard));
    let mut total_extracted: u64 = 0;
    let mut deferred_version: Option<Vec<u8>> = None;
    let mut extracted_files: usize = 0;

    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_type = entry.header().entry_type();
        ensure!(
            matches!(entry_type, EntryType::Regular | EntryType::Directory),
            "el paquete contiene tipos de entrada no permitidos"
        );
        let raw_path = entry.path()?.into_owned();
        let Some(rel_path) = sanitize_archive_path(&raw_path)? else {
            continue;
        };

        if entry_type == EntryType::Directory {
            ensure_safe_subdirs(root, &rel_path)?;
            continue;
        }

        if let Some(parent) = rel_path.parent()
            && !parent.as_os_str().is_empty()
        {
            ensure_safe_subdirs(root, parent)?;
        }

        if rel_path == Path::new("version.txt") {
            let bytes = read_limited(&mut entry, 256)?;
            total_extracted = total_extracted
                .checked_add(bytes.len() as u64)
                .context("desbordamiento de tamaño extraído")?;
            ensure!(
                total_extracted <= max_extracted_size,
                "el paquete descomprimido excede el límite permitido"
            );
            deferred_version = Some(bytes);
            extracted_files += 1;
            continue;
        }

        let raw_mode = entry.header().mode().unwrap_or(0o644);
        let safe_mode = if raw_mode & 0o111 != 0 { 0o755 } else { 0o644 };
        let target = root.join(&rel_path);
        let parent = target.parent().context("falta directorio padre")?;
        write_atomic(parent, &target, safe_mode, |file| {
            let mut buf = [0u8; 64 * 1024];
            loop {
                let count = entry.read(&mut buf)?;
                if count == 0 {
                    break;
                }
                total_extracted = total_extracted
                    .checked_add(count as u64)
                    .context("desbordamiento de tamaño extraído")?;
                ensure!(
                    total_extracted <= max_extracted_size,
                    "el paquete descomprimido excede el límite permitido"
                );
                file.write_all(&buf[..count])?;
            }
            Ok(())
        })?;
        extracted_files += 1;
    }

    ensure!(extracted_files > 0, "el paquete está vacío");
    let version_bytes = deferred_version.context("el paquete no incluye version.txt")?;
    write_atomic(root, &root.join("version.txt"), 0o644, |file| {
        file.write_all(&version_bytes)?;
        Ok(())
    })?;
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
        "el paquete no instaló la versión esperada"
    );
    let guard = open_regular_nofollow(executable)?;
    let meta = guard.metadata()?;
    check_protected(&meta)?;
    ensure!(
        meta.mode() & 0o100 != 0,
        "el ejecutable instalado no tiene permisos de ejecución"
    );
    veltrix_core::verify_reader_sha256(&guard, digest)?;
    Ok(guard)
}

fn wait_for_startup(child: &mut std::process::Child, ack: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("la aplicación se cerró antes de confirmar el arranque: {status}");
        }
        match fs::symlink_metadata(ack) {
            Ok(meta) => {
                ensure!(
                    !meta.file_type().is_symlink() && meta.is_file() && meta.len() == 0,
                    "confirmación de arranque no válida"
                );
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        ensure!(
            Instant::now() < deadline,
            "la aplicación no confirmó el arranque dentro del plazo"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn restart(
    executable: &Path,
    root: &Path,
    ack: Option<&Path>,
    can_restore: &mut bool,
) -> Result<()> {
    let mut command = Command::new(executable);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .current_dir(root);
    command.env_remove(STARTUP_ACK_ENV);
    if let Some(ack) = ack {
        command.env(STARTUP_ACK_ENV, ack);
    }
    let mut child = command.spawn()?;
    if let Some(ack) = ack
        && let Err(error) = wait_for_startup(&mut child, ack, Duration::from_secs(30))
    {
        // Never run recovery while the new binary may still be running.
        *can_restore = false;
        let stopped = child.try_wait().and_then(|status| {
            if status.is_none() {
                child.kill()?;
                child.wait()?;
            }
            Ok(())
        });
        *can_restore = stopped.is_ok();
        if let Err(stop) = stopped {
            bail!("{error:#}. No se pudo detener el proceso para recuperar la instalación: {stop}");
        }
        return Err(error);
    }
    Ok(())
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
                "Preparando una copia local del paquete.".into(),
                false,
                false,
            )?;
            let mut staged = stage(&self.control, self.source, &self.manifest, |done, total| {
                show(&UpdateEvent {
                    stage: "Verificando integridad".into(),
                    message: if done < total {
                        "Copiando el paquete."
                    } else {
                        "Comprobando SHA-256."
                    }
                    .into(),
                    terminal: false,
                    success: false,
                    progress: (done < total).then_some((done, total)),
                });
            })?;
            let backup = if let Some((source, manifest)) = self.rollback {
                let verified = stage(&self.control, source, &manifest, |_, _| {});
                match verified {
                    Ok(backup) => Some((backup, manifest)),
                    Err(error) => {
                        emit("Respaldo no disponible", format!("{error:#}"), false, false)?;
                        None
                    }
                }
            } else {
                None
            };
            let startup_dir = self
                .confirm_restart
                .then(|| {
                    tempfile::Builder::new()
                        .prefix("startup-")
                        .tempdir_in(&self.control)
                })
                .transpose()?;
            let _startup_guard = startup_dir
                .as_ref()
                .map(|dir| retained_directory(dir.path()))
                .transpose()?;
            let ack = startup_dir.as_ref().map(|dir| dir.path().join("ready"));
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
                "Extrayendo y aplicando la nueva versión.".into(),
                false,
                false,
            )?;
            let mut can_restore = true;
            let installation = (|| -> Result<()> {
                install(&mut staged, &self.root, self.max_extracted_size)?;
                emit(
                    "Validando instalación",
                    "Comprobando la versión y el ejecutable instalado.".into(),
                    false,
                    false,
                )?;
                let _restart_guard = validate(
                    &self.version_file,
                    &self.executable,
                    &self.target,
                    &self.installed_hash,
                )?;
                emit(
                    "Reiniciando aplicación",
                    if self.confirm_restart {
                        "Esperando la confirmación de arranque."
                    } else {
                        "Abriendo la versión actualizada."
                    }
                    .into(),
                    false,
                    false,
                )?;
                restart(
                    &self.executable,
                    &self.root,
                    ack.as_deref(),
                    &mut can_restore,
                )
            })();
            if let Err(error) = installation {
                if can_restore && let Some((mut old, backup)) = backup {
                    let restore = (|| -> Result<()> {
                        emit(
                            "Restaurando instalación",
                            "Verificando y aplicando el respaldo firmado.".into(),
                            false,
                            false,
                        )?;
                        install(&mut old, &self.root, self.max_extracted_size)?;
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
            emit(
                "Actualización completada",
                if self.confirm_restart {
                    format!(
                        "La versión {} está instalada y confirmó el arranque.",
                        self.target
                    )
                } else {
                    format!(
                        "La versión {} está instalada. El protocolo anterior verifica solo el lanzamiento.",
                        self.target
                    )
                },
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
    use ed25519_dalek::{Signer, SigningKey};
    use flate2::{Compression, write::GzEncoder};
    use sha2::{Digest, Sha256};

    fn make_tar_gz(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut builder = tar::Builder::new(&mut encoder);
            for (path, data, mode) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(*mode);
                header.set_cksum();
                builder.append_data(&mut header, path, *data).unwrap();
            }
            builder.finish().unwrap();
        }
        encoder.finish().unwrap()
    }

    #[test]
    fn rejects_unsafe_names_and_archive_paths() {
        for bad in ["", ".", "..", "a/b", "app\0name", "bad\nname"] {
            assert!(!safe_name(bad), "{bad}");
        }
        assert!(safe_name("dylan"));
        assert!(safe_name("My App"));

        assert_eq!(
            sanitize_archive_path(Path::new("./app")).unwrap(),
            Some(PathBuf::from("app"))
        );
        assert_eq!(sanitize_archive_path(Path::new(".")).unwrap(), None);
        assert!(sanitize_archive_path(Path::new("../escape")).is_err());
        assert!(sanitize_archive_path(Path::new("/etc/passwd")).is_err());
        assert!(sanitize_archive_path(Path::new(".updater/veltrix")).is_err());
        assert!(sanitize_archive_path(Path::new("./.updater/app.json")).is_err());
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
        let sh = PathBuf::from("/bin/sh");
        let mut child = Command::new(&sh)
            .args(["-c", "sleep 10"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let handle = Parent::open(child.id(), &sh).unwrap();
        assert!(handle.wait(0).is_err());
        child.kill().unwrap();
        handle.wait(5000).unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn signed_tar_gz_update_preserves_user_data_and_validates() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().join("Application With Spaces");
        let control = root.join(".updater");
        fs::create_dir_all(&control).unwrap();
        fs::set_permissions(&root, Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&control, Permissions::from_mode(0o700)).unwrap();

        let user_data = root.join("userdata.txt");
        fs::write(&user_data, "preserve me").unwrap();
        let exe_bytes = b"#!/bin/sh\nexit 0\n";
        let archive_bytes = make_tar_gz(&[
            ("./myapp", exe_bytes, 0o755),
            ("./version.txt", b"2.0.0", 0o644),
        ]);
        let package = workspace.path().join(PACKAGE_FILENAME);
        fs::write(&package, &archive_bytes).unwrap();

        let key = SigningKey::from_bytes(&[61; 32]);
        let policy = Policy {
            app_id: "test".into(),
            channel: Channel::Stable,
            current_version: "1.0.0".parse().unwrap(),
            platform: "linux".into(),
            architecture: std::env::consts::ARCH.into(),
            public_key: key.verifying_key().to_bytes(),
            max_package_size: 8 * 1024 * 1024,
        };
        let target = "2.0.0".parse().unwrap();
        let installed_hash = hex::encode(Sha256::digest(exe_bytes));
        let manifest = serde_json::to_vec(&veltrix_core::Manifest {
            app_id: policy.app_id.clone(),
            version: Version::clone(&target),
            channel: policy.channel,
            platform: policy.platform.clone(),
            architecture: policy.architecture.clone(),
            size: archive_bytes.len() as u64,
            url: "https://example.com/package.tar.gz".into(),
            sha256: hex::encode(Sha256::digest(&archive_bytes)),
            installed_sha256: Some(installed_hash.clone()),
        })
        .unwrap();
        let signed = policy
            .verify_manifest(&manifest, &key.sign(&manifest).to_bytes())
            .unwrap();
        let mut staged = stage(&control, File::open(package).unwrap(), &signed, |_, _| {}).unwrap();
        install(&mut staged, &root, 64 * 1024 * 1024).unwrap();
        let application = root.join("myapp");
        let guard = validate(
            &root.join("version.txt"),
            &application,
            &target,
            &installed_hash,
        )
        .unwrap();
        assert_eq!(fs::read_to_string(user_data).unwrap(), "preserve me");
        drop(guard);

        let marker = root.join("version.txt");
        fs::write(&marker, "1.0.0").unwrap();
        assert!(validate(&marker, &application, &target, &installed_hash).is_err());
        fs::write(&marker, "2.0.0").unwrap();
        assert!(validate(&marker, &application, &target, &"00".repeat(32)).is_err());
    }

    #[test]
    fn rejects_symlinks_and_updater_entries_in_archive() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().join("app");
        let control = root.join(".updater");
        fs::create_dir_all(&control).unwrap();
        fs::set_permissions(&root, Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&control, Permissions::from_mode(0o700)).unwrap();

        let bad_updater = make_tar_gz(&[
            (".updater/veltrix", b"pwned", 0o755),
            ("version.txt", b"2.0.0", 0o644),
        ]);
        let path = workspace.path().join("bad.tar.gz");
        fs::write(&path, &bad_updater).unwrap();
        let mut staged = Staged {
            guard: File::open(&path).unwrap(),
            _dir: tempfile::tempdir().unwrap(),
        };
        assert!(install(&mut staged, &root, 64 * 1024 * 1024).is_err());
        assert!(!control.join("veltrix").exists());

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        {
            let mut builder = tar::Builder::new(&mut encoder);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(EntryType::Symlink);
            header.set_size(0);
            header.set_mode(0o777);
            header.set_cksum();
            builder
                .append_link(&mut header, "evil-link", "/etc/passwd")
                .unwrap();
            builder.finish().unwrap();
        }
        let symlink_tar = encoder.finish().unwrap();
        let symlink_path = workspace.path().join("symlink.tar.gz");
        fs::write(&symlink_path, &symlink_tar).unwrap();
        let mut staged = Staged {
            guard: File::open(&symlink_path).unwrap(),
            _dir: tempfile::tempdir().unwrap(),
        };
        assert!(install(&mut staged, &root, 64 * 1024 * 1024).is_err());
        assert!(!root.join("evil-link").exists());
    }
}
