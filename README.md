# Veltrix

Workspace Rust 2024 para actualizaciones completas de escritorio. La biblioteca puede usarse desde cualquier aplicación Rust. El ejecutable independiente para Windows usa GPUI Kit 0.10 y ejecuta un instalador NSIS autenticado. Compile con Rust estable para Windows y las herramientas de C++ de Visual Studio con Windows SDK.

## Compilar y probar

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo check --workspace --locked
cargo test --workspace --locked
cargo build --release --locked --bin veltrix
```

Si GPUI no encuentra el compilador de shaders del SDK, configure `GPUI_FXC_PATH` con la ruta al archivo `x64\fxc.exe` del Windows SDK. El binario queda en `target/release/veltrix.exe`. CI ejecuta las mismas comprobaciones en Windows. La biblioteca no depende de GPUI.

Para probar un instalador NSIS real con un paquete firmado de prueba, indique dónde está MakeNSIS:

```powershell
$env:MAKENSIS = 'C:\Program Files (x86)\NSIS\makensis.exe'
cargo test --locked -p veltrix signed_nsis_update_writes_only_to_chosen_installation -- --ignored
```

## Distribuir la aplicación

La instalación inicial la realiza NSIS. Incluya esta estructura dentro de una instalación por usuario con permisos que impidan escrituras de otras cuentas:

```text
<instalación>/
  MyApp.exe
  version.txt
  .updater/
    veltrix.exe
    app.json
    update.lock
    icon.png             (opcional)
```

`version.txt` contiene la versión SemVer instalada. `update.lock` es un archivo vacío que permanece en su sitio en todas las versiones. `app.json` es configuración de confianza distribuida con la instalación. Ejemplo de forma:

```json
{
  "app_id": "org.example.myapp",
  "name": "My App",
  "executable": "MyApp.exe",
  "channel": "stable",
  "public_key": "<64 caracteres hexadecimales de la clave pública Ed25519>",
  "max_package_size": 536870912
}
```

El instalador debe admitir `/S` y respetar el `/D` que recibe, devolver un código distinto de cero ante errores, esperar a que terminen sus procesos auxiliares, actualizar `version.txt` al final y evitar iniciar por su cuenta la aplicación. Use `RequestExecutionLevel user` para instalaciones por usuario. Mantenga la configuración y los datos personales fuera del conjunto de archivos reemplazados. No sobrescriba los archivos activos dentro de `.updater` durante el proceso. [Parámetros de NSIS](https://nsis.sourceforge.io/Docs/Chapter3.html).

## Publicar un release

Etiquete con SemVer, con prefijo `v` opcional. Stable admite versiones finales. Beta admite identificadores `beta`, `beta.*` y `beta-*`. Nightly usa las mismas reglas con `nightly`. Marque beta y nightly como prereleases en GitHub. La selección ignora borradores y usa precedencia SemVer, incluidos los números de prerelease.

Publique el instalador y dos assets por plataforma y arquitectura. Para Windows x64, use `update-windows-x86_64.json` y `update-windows-x86_64.json.sig`. El JSON incluye `app_id`, `version`, `channel`, `platform`, `architecture`, `size` en bytes, `url` HTTPS, `sha256` del instalador e `installed_sha256` del ejecutable que se relanzará después de instalar. Los hashes se expresan en hexadecimal. Firme los bytes exactos del JSON con Ed25519 y publique la firma binaria de 64 bytes. Mantenga la clave privada fuera del cliente y del repositorio.

Para firmar sin volver a codificar el manifiesto, use `cargo run --locked -p veltrix-sign -- <clave-privada> <manifiesto.json> <instalador.exe> <manifiesto.json.sig>`. La clave debe ser una semilla Ed25519 de exactamente 32 bytes, almacenada en un equipo y directorio protegidos. La herramienta valida el esquema, el hash y el tamaño del instalador y exige el hash del ejecutable instalado antes de crear una firma nueva. Muestra la clave pública para compararla con la configuración que se entregará a la aplicación. La herramienta no comprueba la identidad de la persona que firma ni protege una clave guardada en un sistema comprometido.

Realice una copia de seguridad de la clave fuera del entorno de compilación y limite el acceso al proceso de firma. Para una rotación planificada, incluya la nueva clave pública en `additional_public_keys` dentro de `app.json` mediante una actualización autorizada por la clave anterior. Después de verificar que esa configuración llegó a los clientes, publique paquetes firmados con la nueva clave; mantenga la anterior hasta que ya no sea necesaria para las versiones y respaldos admitidos. El integrador debe usar `release_manifest_with_keys` con las mismas claves locales. Se aceptan hasta ocho claves adicionales. Si una clave se compromete, una actualización firmada con ella ya no constituye una vía confiable de recuperación: sustituya la configuración o reinstale mediante un canal independiente confiable. Authenticode, la gestión de certificados y la publicación de assets requieren infraestructura y credenciales del distribuidor.

## Integrar desde Rust

El proyecto [examples/integration](examples/integration) contiene una ventana GPUI mínima y una función que consulta, descarga, verifica e inicia el actualizador en un hilo de trabajo. Para probar el flujo, compile el ejemplo, instale `veltrix-integration-example.exe` como el ejecutable configurado en `app.json`, y prepare la estructura de instalación descrita arriba. Añada `.updater/example-repository.json` con `{"owner":"propietario","repo":"repositorio"}` y ejecute el ejemplo instalado con `--update`. La ventana muestra los bytes descargados y sale después de `READY`; una ejecución posterior consulta el último resultado en `events.jsonl`. Los trabajos se guardan en `.updater/jobs` y el ejemplo no borra su historial. Una aplicación real debe integrar también el guardado de datos, el cierre de procesos auxiliares y su propia política de arranque.

El llamador crea un directorio privado nuevo por intento. Descarga antes de pedir que la aplicación salga. Configure la política desde la misma identidad, clave, canal y versión local que usa el ejecutable. Ejecute las operaciones HTTP bloqueantes en un hilo de trabajo.

```rust
use std::{fs, path::Path, sync::atomic::AtomicBool};
use veltrix_core::{
    Policy, UpdateRequest, download, github_releases, http_client,
    release_manifest, select_release, wait_for_ready,
};

fn prepare_update(
    policy: &Policy,
    owner: &str,
    repo: &str,
    job: &Path,          // absoluto y nuevo, sin puntos de reanálisis
    updater: &Path,      // .updater/veltrix.exe de la instalación actual
    cancel: &AtomicBool,
) -> Result<bool, Box<dyn std::error::Error>> {
    let client = http_client()?;
    let releases = github_releases(&client, owner, repo)?;
    let Some(release) = select_release(&releases, &policy.current_version, policy.channel) else {
        return Ok(false);
    };
    let (manifest, bytes, signature) = release_manifest(&client, policy, release)?;
    fs::write(job.join("manifest.json"), bytes)?;
    fs::write(job.join("manifest.sig"), signature)?;
    download(&client, &manifest, &job.join("package.exe"), cancel, |done, total| {
        eprintln!("{done} / {total} bytes");
    })?;

    let request = UpdateRequest {
        protocol: 1,
        app_id: policy.app_id.clone(),
        parent_pid: std::process::id(),
        job_dir: job.to_owned(),
    };
    let mut child = request.launch(updater)?;
    wait_for_ready(&mut child)?;
    // Tras devolver true, guarde los datos pendientes y salga ordenadamente.
    Ok(true)
}
```

El actualizador valida la identidad del proceso y conserva un handle hasta que se cierra. Permite hasta dos minutos después de responder `READY`. `wait_for_ready` espera hasta tres minutos y termina el actualizador si no responde; `wait_for_ready_timeout` permite especificar otro plazo. La aplicación integradora debe mantener vivo el proceso hasta que termine esta llamada, cerrar también sus procesos auxiliares y coordinar su arranque para evitar nuevas instancias durante la instalación. Use `read_update_events` y `update_outcome` con el directorio del intento para consultar los eventos JSONL. Un resultado `Succeeded` confirma la validación y que Windows aceptó el lanzamiento del nuevo binario; no confirma que la aplicación haya terminado de arrancar correctamente. Los errores anteriores a la apertura del registro salen por stderr.

Puede aprovisionar un instalador anterior y sus archivos `manifest.json`, `manifest.sig` y `package.exe` en `.updater/rollback`. Deben pertenecer a la versión instalada y pasar las mismas comprobaciones criptográficas. Si NSIS falla o el ejecutable instalado no coincide con la versión y el hash esperados, el actualizador intenta restaurar esa versión y sigue informando un fallo de actualización. El instalador de respaldo debe poder reparar una instalación parcial y dejar los datos del usuario intactos.

## Límites de la versión inicial

- La interfaz muestra progreso medible durante la copia local. Cierre, hash, ejecución de NSIS y lanzamiento se muestran como indeterminados. La descarga inicial ofrece progreso y cancelación a la aplicación integradora. La cancelación se comprueba entre lecturas.
- GitHub Releases se consulta hasta 1.000 elementos recientes de un repositorio público, en páginas acotadas. La selección considera los releases recibidos.
- Se valida el ejecutable principal y el marcador local de versión. La consistencia de los demás archivos y de los datos de usuario depende del instalador de cada aplicación. Sin respaldo no hay reversión de cambios parciales.
- El control de rutas y la copia protegida dependen de que el árbol de instalación y el directorio de trabajo estén protegidos contra otras cuentas. El diseño no aísla aplicaciones maliciosas con los mismos permisos ni instaladores firmados que incumplan el contrato.
- Los parches incrementales y los ejecutables para Linux y macOS quedan pendientes.
- El ejemplo GPUI inicia la búsqueda mediante `--update`, sin controles de selección ni cancelación. No hay selección de parches ni publicación automatizada de releases. El workflow manual de artefactos produce un ejecutable sin firma Authenticode; la firma y la publicación quedan a cargo del proceso de distribución.



