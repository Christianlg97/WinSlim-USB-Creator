#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

slint::include_modules!();

#[cfg(target_os = "windows")]
#[path = "platform/windows.rs"]
mod platform;
#[cfg(target_os = "linux")]
#[path = "platform/linux.rs"]
mod platform;
use platform::*;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use slint::winit_030::winit::event::WindowEvent;
use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::{ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::{
    cell::Cell,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        mpsc, Arc, Mutex, MutexGuard, PoisonError,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone, Copy, Debug)]
enum FirmwareFeatureState {
    Enabled,
    Disabled,
    Unknown,
}

impl FirmwareFeatureState {
    fn ui_value(self) -> i32 {
        match self {
            Self::Enabled => 1,
            Self::Disabled => 0,
            Self::Unknown => -1,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct FirmwareSecurityStatus {
    tpm: FirmwareFeatureState,
    secure_boot: FirmwareFeatureState,
}

const DOWNLOAD_IDLE: u8 = 0;
const DOWNLOAD_RUNNING: u8 = 1;
const DOWNLOAD_CANCELLED: u8 = 2;
const DOWNLOAD_COMMITTING: u8 = 3;
const DOWNLOAD_CANCELLED_MESSAGE: &str = "Descarga cancelada";
const PARALLEL_CONNECTIONS: usize = 4;
const DOWNLOAD_SEGMENT_BYTES: u64 = 32 * 1024 * 1024;
const PARALLEL_MIN_BYTES: u64 = 64 * 1024 * 1024;
const MIRROR_PROBE_BYTES: u64 = 512 * 1024;
const MAX_MIRROR_PROBES: usize = 8;

fn check_download_cancellation(download_state: &AtomicU8) -> Result<(), String> {
    if download_state.load(Ordering::Acquire) == DOWNLOAD_CANCELLED {
        Err(DOWNLOAD_CANCELLED_MESSAGE.into())
    } else {
        Ok(())
    }
}

fn commit_download(download_state: &AtomicU8) -> Result<(), String> {
    download_state
        .compare_exchange(
            DOWNLOAD_RUNNING,
            DOWNLOAD_COMMITTING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .map(|_| ())
        .map_err(|_| DOWNLOAD_CANCELLED_MESSAGE.into())
}

const RSS: &str = "https://sourceforge.net/projects/winslim11-isos/rss?path=/";
const ALTERNATIVE_RELEASE_API: &str =
    "https://api.github.com/repos/Christianlg97/WinSlim_Mirroring/releases/tags/Latest_Mirror_URL";
const ALTERNATIVE_RELEASE_TAG: &str = "Latest_Mirror_URL";
const MAIN_SERVER_UNAVAILABLE_MESSAGE: &str =
    "La ISO no está disponible en el servidor principal. Prueba con la descarga alternativa.";
#[derive(Clone)]
struct Iso {
    name: String,
    url: String,
    size: u64,
    md5: Option<String>,
    published: u64,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Disk {
    number: u32,
    #[serde(default)]
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    device_path: Option<String>,
    friendly_name: String,
    serial_number: Option<String>,
    size: u64,
    bus_type: String,
    is_boot: bool,
    is_system: bool,
    is_read_only: bool,
    #[serde(default)]
    volumes: Vec<Volume>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Volume {
    letter: Option<String>,
    #[serde(default)]
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    mount_path: Option<String>,
    #[serde(default)]
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    device_path: Option<String>,
    label: Option<String>,
    file_system: Option<String>,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    partition_number: u32,
}

#[derive(Default)]
struct State {
    iso: Option<Iso>,
    local_iso: Option<PathBuf>,
    disks: Vec<Disk>,
    selected: Option<u32>,
    // Disco mostrado en el diálogo de confirmación: se prepara ese y no otro.
    confirming: Option<u32>,
    busy: bool,
}

/// Un pánico en un hilo envenena el Mutex; el estado sigue siendo coherente
/// porque cada sección solo asigna campos, así que se recupera en vez de abortar.
fn lock_state(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Si un hilo de trabajo entra en pánico, `finish` no llega a ejecutarse.
/// Este guardián libera la interfaz para que no quede bloqueada como ocupada.
struct PanicGuard {
    weak: slint::Weak<MainWindow>,
    state: Arc<Mutex<State>>,
    download_state: Option<Arc<AtomicU8>>,
}

impl PanicGuard {
    fn new(
        weak: &slint::Weak<MainWindow>,
        state: &Arc<Mutex<State>>,
        download_state: Option<&Arc<AtomicU8>>,
    ) -> Self {
        Self {
            weak: weak.clone(),
            state: state.clone(),
            download_state: download_state.cloned(),
        }
    }
}

impl Drop for PanicGuard {
    fn drop(&mut self) {
        if !thread::panicking() {
            return;
        }
        lock_state(&self.state).busy = false;
        if let Some(download_state) = &self.download_state {
            download_state.store(DOWNLOAD_IDLE, Ordering::Release);
        }
        log_event("ERROR etapa=hilo_trabajo panico=true interfaz_liberada".into());
        ui(self.weak.clone(), |window| {
            window.set_busy(false);
            window.set_usb_detecting(false);
            window.set_download_active(false);
            window.set_download_cancelling(false);
            window.set_preparing(false);
            window.set_progress(0.0);
            window.set_status_text("Error interno inesperado. Consulta el Registro.".into());
        });
    }
}

fn ui<F: FnOnce(MainWindow) + Send + 'static>(weak: slint::Weak<MainWindow>, f: F) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            f(window);
        }
    });
}

fn status(weak: slint::Weak<MainWindow>, message: impl Into<String>) {
    let message = message.into();
    ui(weak, move |window| window.set_status_text(message.into()));
}

fn finish(weak: slint::Weak<MainWindow>, result: Result<String, String>, state: Arc<Mutex<State>>) {
    lock_state(&state).busy = false;
    log_event(match &result {
        Ok(message) => format!("INFO resultado=OK detalle={message}"),
        Err(message) => format!("ERROR resultado=FALLO detalle={message}"),
    });
    ui(weak, move |window| {
        window.set_busy(false);
        window.set_preparing(false);
        window.set_status_text(match result {
            Ok(message) => message.into(),
            Err(message) => format!("Error: {message}").into(),
        });
    });
}

fn finish_download(
    weak: slint::Weak<MainWindow>,
    result: Result<String, String>,
    state: Arc<Mutex<State>>,
    download_state: Arc<AtomicU8>,
) {
    let cancelled = result
        .as_ref()
        .err()
        .is_some_and(|error| error == DOWNLOAD_CANCELLED_MESSAGE);
    download_state.store(DOWNLOAD_IDLE, Ordering::Release);
    ui(weak.clone(), move |window| {
        window.set_download_active(false);
        window.set_download_cancelling(false);
        if cancelled {
            window.set_progress(0.0);
        }
    });
    finish(
        weak,
        if cancelled {
            Ok("Descarga cancelada. Archivo parcial eliminado.".into())
        } else {
            result
        },
        state,
    );
}

// Al superar este tamaño, el registro pasa a operations.log.1 y se empieza otro.
const LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;
const LOG_VIEW_BYTES: u64 = 500_000;
// Varios hilos registran a la vez (p. ej. las conexiones paralelas); el cerrojo
// evita líneas mezcladas y que dos hilos roten el archivo al mismo tiempo.
static LOG_LOCK: Mutex<()> = Mutex::new(());

fn rotated_log_path(path: &Path) -> PathBuf {
    path.with_extension("log.1")
}

fn log_event(message: String) {
    let Some(path) = log_path() else {
        return;
    };
    let Some(dir) = path.parent() else {
        return;
    };
    // La línea completa se forma antes de escribir para enviarla en una sola escritura.
    let line = format!("{} {message}\n", httpdate::fmt_http_date(SystemTime::now()));
    let _lock = LOG_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    if fs::metadata(&path).is_ok_and(|metadata| metadata.len() >= LOG_MAX_BYTES) {
        let _ = fs::rename(&path, rotated_log_path(&path));
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn clear_log() -> Result<(), String> {
    let path = log_path().ok_or("No se encontró el directorio del registro")?;
    let _lock = LOG_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    match fs::remove_file(rotated_log_path(&path)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    fs::write(&path, "").map_err(|error| error.to_string())
}

/// Guarda el registro completo: la parte rotada (más antigua) seguida de la actual.
fn export_log(source: &Path, target: &Path) -> Result<(), String> {
    let _lock = LOG_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let mut output = File::create(target).map_err(|error| error.to_string())?;
    for path in [rotated_log_path(source), source.to_path_buf()] {
        match File::open(&path) {
            Ok(mut input) => {
                std::io::copy(&mut input, &mut output).map_err(|error| error.to_string())?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    output.sync_all().map_err(|error| error.to_string())
}

fn read_log() -> String {
    let Some(path) = log_path() else {
        return "No se encontró el directorio del registro".into();
    };
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return "El registro está vacío".into()
        }
        Err(e) => return format!("No se pudo leer el registro: {e}"),
    };
    // Solo se lee el final: el registro puede ocupar varios MB y esta función
    // se ejecuta en el hilo de la interfaz mientras el panel está abierto.
    let length = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let start = length.saturating_sub(LOG_VIEW_BYTES);
    let mut bytes = Vec::new();
    if let Err(e) = file
        .seek(SeekFrom::Start(start))
        .and_then(|_| file.read_to_end(&mut bytes))
    {
        return format!("No se pudo leer el registro: {e}");
    }
    let mut contents = String::from_utf8_lossy(&bytes).into_owned();
    if start > 0 {
        // El corte puede caer a mitad de línea o de carácter: se empieza en la siguiente línea.
        if let Some(newline) = contents.find('\n') {
            contents.drain(..=newline);
        }
        contents.insert_str(0, "[Se muestran los últimos 500 KB del registro]\n");
    }
    contents
}

fn http_error(stage: &str, error: ureq::Error) -> String {
    let (summary, technical) = match error {
        ureq::Error::Status(code, response) => (
            format!("HTTP {code} al acceder a SourceForge"),
            format!(
                "status={code} url={}",
                response.get_url().split('?').next().unwrap_or("")
            ),
        ),
        ureq::Error::Transport(transport) => {
            let mut details = format!(
                "kind={:?} message={:?} url={:?}",
                transport.kind(),
                transport.message(),
                transport
                    .url()
                    .map(|u| u.as_str().split('?').next().unwrap_or(""))
            );
            let mut source = std::error::Error::source(&transport);
            while let Some(cause) = source {
                details.push_str(&format!(" -> {cause}"));
                source = cause.source();
            }
            let lower = details.to_ascii_lowercase();
            let summary = if transport.kind() == ureq::ErrorKind::Dns {
                "Error DNS al localizar SourceForge"
            } else if transport.kind() == ureq::ErrorKind::TooManyRedirects {
                "Demasiadas redirecciones de descarga"
            } else if lower.contains("timed out") || lower.contains("timeout") {
                "Tiempo de espera agotado"
            } else if lower.contains("tls") || lower.contains("certificate") {
                "Error de seguridad HTTPS/TLS"
            } else if transport.kind() == ureq::ErrorKind::ConnectionFailed {
                "Sin conexión con el servidor"
            } else {
                "Error de red al acceder a SourceForge"
            };
            (summary.into(), details)
        }
    };
    log_event(format!("ERROR etapa={stage} detalle={technical}"));
    format!("{summary} ({stage}; consulta Registro)")
}

fn file_error(stage: &str, path: &Path, error: std::io::Error) -> String {
    let summary = if matches!(error.raw_os_error(), Some(112 | 28)) {
        "No hay espacio suficiente en el disco"
    } else if error.kind() == std::io::ErrorKind::PermissionDenied {
        "No hay permiso para escribir el archivo"
    } else {
        "Error de archivo"
    };
    log_event(format!(
        "ERROR etapa={stage} ruta={} kind={:?} os_code={:?} detalle={error}",
        path.display(),
        error.kind(),
        error.raw_os_error()
    ));
    format!("{summary} ({stage}; consulta Registro)")
}

fn begin(state: &Arc<Mutex<State>>, window: &MainWindow) -> bool {
    let mut state = lock_state(state);
    if state.busy {
        return false;
    }
    state.busy = true;
    window.set_busy(true);
    window.set_progress(0.0);
    window.set_preparation_complete(false);
    true
}

fn fetch_latest() -> Result<Iso, String> {
    log_event(format!("INFO etapa=consultar_iso url={RSS}"));
    let response = ureq::get(RSS)
        .set("User-Agent", "WinSlimUsbCreator/0.1")
        .timeout(Duration::from_secs(25))
        .call()
        .map_err(|e| http_error("consultar RSS", e))?;
    log_event(format!(
        "DEBUG etapa=consultar_iso http={} url={}",
        response.status(),
        response.get_url().split('?').next().unwrap_or("")
    ));
    let xml = response
        .into_string()
        .map_err(|e| format!("Respuesta RSS inválida: {e}"))?;
    let iso = parse_latest(&xml)?;
    log_event(format!(
        "INFO etapa=consultar_iso iso={} bytes={} md5={:?}",
        iso.name, iso.size, iso.md5
    ));
    Ok(iso)
}

#[derive(Deserialize)]
struct AlternativeRelease {
    tag_name: String,
    body: Option<String>,
}

fn alternative_link_from_notes(notes: &str) -> Result<url::Url, String> {
    let mut selected = None;
    for (start, _) in notes.match_indices("https://") {
        // match_indices devuelve la coincidencia, no el resto de la URL.
        let raw = notes[start..]
            .split(|character: char| {
                character.is_whitespace() || matches!(character, ')' | ']' | '>' | '<' | '"' | '\'')
            })
            .next()
            .unwrap_or("")
            .trim_end_matches(['.', ',', ';']);
        let decoded = raw.replace("&amp;", "&");
        if let Ok(link) = url::Url::parse(&decoded) {
            if link.scheme() == "https"
                && link.host_str().is_some()
                && link.username().is_empty()
                && link.password().is_none()
            {
                if selected.as_ref().is_some_and(|previous| previous != &link) {
                    return Err("Las notas de la release contienen varios enlaces HTTPS; no se puede identificar la descarga".into());
                }
                selected = Some(link);
            }
        }
    }
    selected.ok_or_else(|| {
        "Las notas de la release no contienen un enlace HTTPS de descarga válido".into()
    })
}

fn fetch_alternative_link() -> Result<url::Url, String> {
    log_event(format!(
        "INFO etapa=consultar_descarga_alternativa url={ALTERNATIVE_RELEASE_API}"
    ));
    let response = ureq::get(ALTERNATIVE_RELEASE_API)
        .set("User-Agent", "WinSlimUsbCreator/0.1")
        .set("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(15))
        .call()
        .map_err(|error| format!("No se pudo consultar la release de GitHub: {error}"))?;
    let release: AlternativeRelease = serde_json::from_reader(response.into_reader())
        .map_err(|error| format!("La respuesta de GitHub no es válida: {error}"))?;
    if release.tag_name != ALTERNATIVE_RELEASE_TAG {
        return Err("GitHub devolvió una release distinta de la esperada".into());
    }
    alternative_link_from_notes(release.body.as_deref().unwrap_or(""))
}

fn fetch_available_alternative_link() -> Result<url::Url, String> {
    let link = fetch_alternative_link()?;
    // Solo leemos las cabeceras: un enlace borrado o privado no cuenta como alternativa.
    let response = ureq::builder()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(10))
        .build()
        .get(link.as_str())
        .set("User-Agent", "WinSlimUsbCreator/0.1")
        .call()
        .map_err(|error| match error {
            ureq::Error::Status(code, _) => {
                format!("La descarga alternativa respondió HTTP {code}")
            }
            ureq::Error::Transport(transport) => {
                format!("No se pudo acceder a la descarga alternativa: {transport}")
            }
        })?;
    let final_host = url::Url::parse(response.get_url())
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned));
    if final_host.as_deref() == Some("accounts.google.com") {
        return Err("La descarga alternativa requiere iniciar sesión en Google".into());
    }
    Ok(link)
}

fn alternative_availability() -> i32 {
    match fetch_available_alternative_link() {
        Ok(link) => {
            log_event(format!(
                "INFO etapa=comprobar_alternativa disponible=true dominio={}",
                link.host_str().unwrap_or("")
            ));
            1
        }
        Err(error) => {
            log_event(format!(
                "WARNING etapa=comprobar_alternativa disponible=false detalle={error}"
            ));
            -1
        }
    }
}

/// Marca el servidor principal como no disponible y comprueba si queda la alternativa.
fn report_main_server_failure(weak: slint::Weak<MainWindow>) -> bool {
    status(weak.clone(), "Comprobando la descarga alternativa…");
    let alternative = alternative_availability();
    ui(weak, move |window| {
        window.set_server_availability(-1);
        window.set_alternative_availability(alternative);
    });
    alternative == 1
}

fn parse_latest(xml: &str) -> Result<Iso, String> {
    let doc = roxmltree::Document::parse(&xml).map_err(|e| format!("RSS inválido: {e}"))?;
    let mut choices = Vec::new();
    for item in doc.descendants().filter(|n| n.has_tag_name("item")) {
        let get = |name: &str| {
            item.children()
                .find(|n| n.is_element() && n.tag_name().name() == name)
                .and_then(|n| n.text())
                .map(str::trim)
        };
        // Solo se aceptan ISOs de la raíz del proyecto: la descarga y la
        // comprobación de espejos esperan /project/winslim11-isos/<nombre>, y
        // así se ignoran subcarpetas como la .rsync-partial de una subida a medias.
        let name = get("title").unwrap_or("").trim_start_matches('/');
        if !name.to_ascii_lowercase().starts_with("winslim")
            || !name.to_ascii_lowercase().ends_with(".iso")
            || name.contains(['/', '\\'])
        {
            continue;
        }
        let Some(url) = get("link") else { continue };
        let Some(file_part) = url
            .strip_prefix("https://sourceforge.net/projects/winslim11-isos/files/")
            .and_then(|rest| rest.strip_suffix("/download"))
        else {
            continue;
        };
        if file_part.contains('/') {
            continue;
        }
        let media = item
            .descendants()
            .find(|n| n.is_element() && n.tag_name().name() == "content");
        let size = media
            .and_then(|n| n.attribute("filesize"))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        if size < 100_000_000 {
            continue;
        }
        // SourceForge publica <media:hash algo="md5">; se exige el algoritmo y el
        // formato para no comparar nunca el MD5 con otro tipo de suma.
        let md5 = media
            .and_then(|n| {
                n.descendants().find(|n| {
                    n.is_element()
                        && n.tag_name().name() == "hash"
                        && n.attribute("algo")
                            .is_some_and(|algo| algo.eq_ignore_ascii_case("md5"))
                })
            })
            .and_then(|n| n.text())
            .map(str::trim)
            .filter(|hash| hash.len() == 32 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .map(str::to_owned);
        let published = get("pubDate")
            .and_then(|s| httpdate::parse_http_date(&s.replace(" UT", " GMT")).ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        choices.push(Iso {
            name: name.to_owned(),
            url: url.to_owned(),
            size,
            md5,
            published,
        });
    }
    choices
        .into_iter()
        .max_by_key(|iso| iso.published)
        .ok_or_else(|| "No se encontró ninguna ISO válida en el RSS de SourceForge".into())
}

fn disk_label(disk: &Disk) -> String {
    let letters = disk
        .volumes
        .iter()
        .filter_map(volume_display)
        .collect::<Vec<_>>()
        .join(", ");
    let letter_part = if letters.is_empty() {
        "sin montar".to_owned()
    } else {
        letters
    };
    format!(
        "Disco {} · {} · {} · {}",
        disk.number,
        disk.friendly_name,
        human(disk.size),
        letter_part
    )
}

fn disk_rows(disks: &[Disk]) -> Vec<DiskRow> {
    disks
        .iter()
        .map(|disk| DiskRow {
            label: SharedString::from(disk_label(disk)),
            detail: SharedString::from(format!(
                "USB · {}{}{}",
                disk.volumes
                    .iter()
                    .filter_map(|volume| volume.file_system.as_deref())
                    .collect::<Vec<_>>()
                    .join(" / "),
                disk.volumes
                    .iter()
                    .filter_map(|volume| volume.label.as_deref())
                    .filter(|label| !label.is_empty())
                    .map(|label| format!(" · {label}"))
                    .collect::<String>(),
                if disk.is_read_only {
                    " · solo lectura"
                } else {
                    ""
                }
            )),
            number: disk.number as i32,
        })
        .collect()
}

fn human(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64).replace('.', ",")
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64).replace('.', ",")
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64).replace('.', ",")
    } else {
        format!("{bytes} B")
    }
}

fn retry_iso_download(
    weak: slint::Weak<MainWindow>,
    attempts: &mut u32,
    downloaded: u64,
    reason: &str,
    download_state: &AtomicU8,
) -> Result<(), String> {
    if download_state.load(Ordering::Acquire) == DOWNLOAD_CANCELLED {
        return Err(DOWNLOAD_CANCELLED_MESSAGE.into());
    }
    const MAX_RETRIES: u32 = 12;
    log_event(format!(
        "WARNING etapa=descargar_iso reintento={} bytes={} motivo={reason}",
        *attempts + 1,
        downloaded
    ));
    if *attempts >= MAX_RETRIES {
        return Err(format!(
            "Descarga interrumpida tras {}. Se guardó el progreso; pulsa Descargar para reanudar. Último error: {reason}",
            human(downloaded)
        ));
    }
    *attempts += 1;
    let delay = (1u64 << (*attempts).min(4)).min(20);
    status(
        weak,
        format!(
            "Conexión interrumpida · reintentando {}/{} desde {} en {} s…",
            *attempts,
            MAX_RETRIES,
            human(downloaded),
            delay
        ),
    );
    let until = Instant::now() + Duration::from_secs(delay);
    while Instant::now() < until {
        if download_state.load(Ordering::Acquire) == DOWNLOAD_CANCELLED {
            return Err(DOWNLOAD_CANCELLED_MESSAGE.into());
        }
        thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

// AB Download Manager reparte los ficheros que admiten rangos entre varias
// conexiones. Aquí usamos lotes acotados para conservar el archivo .part como
// prefijo continuo y poder volver al descargador de una conexión sin perderlo.
fn download_range(
    url: &str,
    path: &Path,
    from: u64,
    to: u64,
    total: u64,
    progress: &AtomicU64,
    abort: &AtomicBool,
    download_state: &AtomicU8,
) -> Result<(), String> {
    let mut completed = 0;
    let mut last_error = String::new();
    for attempt in 0..3 {
        if abort.load(Ordering::Acquire) {
            return Err("Descarga por fragmentos detenida".into());
        }
        check_download_cancellation(download_state)?;
        let resume_from = from + completed;
        let expected_range = format!("bytes {resume_from}-{to}/{total}");
        let expected_length = to - resume_from + 1;
        let agent = ureq::builder()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(15))
            .build();
        let response = agent
            .get(url)
            .set("User-Agent", "WinSlimUsbCreator/0.1")
            .set("Accept-Encoding", "identity")
            .set("Range", &format!("bytes={resume_from}-{to}"))
            .call();
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                last_error = http_error("descargar fragmento", error);
                if attempt < 2 {
                    thread::sleep(Duration::from_secs(1));
                }
                continue;
            }
        };
        if response.status() != 206
            || response.header("Content-Range") != Some(expected_range.as_str())
            || response
                .header("Content-Length")
                .and_then(|value| value.parse::<u64>().ok())
                .is_some_and(|length| length != expected_length)
            || response
                .header("Content-Type")
                .is_some_and(|value| value.to_ascii_lowercase().contains("text/html"))
            || response
                .header("Content-Encoding")
                .is_some_and(|value| !value.eq_ignore_ascii_case("identity"))
        {
            return Err("El espejo no respetó el rango solicitado".into());
        }
        let mut reader = response.into_reader();
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(path)
            .map_err(|error| file_error("crear fragmento temporal", path, error))?;
        file.set_len(completed)
            .map_err(|error| file_error("ajustar fragmento temporal", path, error))?;
        file.seek(SeekFrom::Start(completed))
            .map_err(|error| file_error("reanudar fragmento temporal", path, error))?;
        let mut remaining = expected_length;
        let mut buffer = [0u8; 256 * 1024];
        let result = (|| -> Result<(), String> {
            while remaining > 0 {
                if abort.load(Ordering::Acquire) {
                    return Err("Descarga por fragmentos detenida".into());
                }
                check_download_cancellation(download_state)?;
                let limit = remaining.min(buffer.len() as u64) as usize;
                let length = reader
                    .read(&mut buffer[..limit])
                    .map_err(|error| format!("Lectura del fragmento: {error}"))?;
                if length == 0 {
                    return Err("El espejo cerró un fragmento antes de completarlo".into());
                }
                file.write_all(&buffer[..length])
                    .map_err(|error| file_error("guardar fragmento", path, error))?;
                remaining -= length as u64;
                completed += length as u64;
                progress.store(completed, Ordering::Release);
            }
            file.sync_all()
                .map_err(|error| file_error("guardar fragmento", path, error))?;
            Ok(())
        })();
        match result {
            Ok(()) => return Ok(()),
            Err(error) if error == DOWNLOAD_CANCELLED_MESSAGE => return Err(error),
            Err(error) => last_error = error,
        }
        if attempt < 2 {
            thread::sleep(Duration::from_secs(1));
        }
    }
    Err(last_error)
}

fn download_iso_parallel(
    iso: &Iso,
    preferred_url: Option<&str>,
    weak: slint::Weak<MainWindow>,
    dir: &Path,
    part: &Path,
    downloaded: &mut u64,
    hash: &mut md5::Context,
    download_state: &AtomicU8,
    initial_bytes: u64,
    start: Instant,
    last_update: &mut Instant,
) -> Result<(), String> {
    let temp_dir = dir.join(format!("{}.part-segments", iso.name));
    if temp_dir.exists() {
        fs::remove_dir_all(&temp_dir)
            .map_err(|error| file_error("limpiar fragmentos anteriores", &temp_dir, error))?;
    }
    if iso.md5.is_none() {
        log_event("INFO etapa=descarga_paralela md5_no_disponible usar_una_conexion".into());
        return Ok(());
    }
    if iso.size.saturating_sub(*downloaded) < PARALLEL_MIN_BYTES {
        return Ok(());
    }
    let url = if let Some(url) = preferred_url {
        url.to_owned()
    } else {
        match resolve_download(iso) {
            Ok(url) => url,
            Err(error) => {
                log_event(format!("WARNING etapa=descarga_paralela motivo={error}"));
                return Ok(());
            }
        }
    };
    check_download_cancellation(download_state)?;
    // Una petición mínima comprueba el soporte real de Range. No basta con
    // Accept-Ranges: algunos espejos lo anuncian pero devuelven el archivo entero.
    let probe = ureq::builder()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(15))
        .build()
        .get(&url)
        .set("User-Agent", "WinSlimUsbCreator/0.1")
        .set("Accept-Encoding", "identity")
        .set("Range", "bytes=0-0")
        .call();
    let supports_ranges = probe.is_ok_and(|response| {
        response.status() == 206
            && response.header("Content-Range") == Some(format!("bytes 0-0/{}", iso.size).as_str())
            && response.header("Content-Length") == Some("1")
    });
    if !supports_ranges {
        log_event("INFO etapa=descarga_paralela rangos_no_disponibles usar_una_conexion".into());
        return Ok(());
    }
    fs::create_dir(&temp_dir)
        .map_err(|error| file_error("crear carpeta de fragmentos", &temp_dir, error))?;
    log_event(format!(
        "INFO etapa=descarga_paralela conexiones={} fragmento_bytes={DOWNLOAD_SEGMENT_BYTES}",
        PARALLEL_CONNECTIONS
    ));
    let result = (|| -> Result<(), String> {
        while *downloaded < iso.size {
            check_download_cancellation(download_state)?;
            let batch_bytes = iso
                .size
                .saturating_sub(*downloaded)
                .min(DOWNLOAD_SEGMENT_BYTES * PARALLEL_CONNECTIONS as u64);
            if fs_free_bytes(dir)? < iso.size.saturating_sub(*downloaded) + batch_bytes {
                log_event(
                    "INFO etapa=descarga_paralela espacio_temporal_insuficiente usar_una_conexion"
                        .into(),
                );
                break;
            }
            let batch_end = *downloaded + batch_bytes;
            let mut ranges = Vec::new();
            let mut from = *downloaded;
            while from < batch_end {
                let to = (from + DOWNLOAD_SEGMENT_BYTES).min(batch_end) - 1;
                ranges.push((from, to));
                from = to + 1;
            }
            let abort = AtomicBool::new(false);
            let progress = (0..ranges.len())
                .map(|_| AtomicU64::new(0))
                .collect::<Vec<_>>();
            let failure = thread::scope(|scope| {
                let (sender, receiver) = mpsc::channel();
                for (index, &(from, to)) in ranges.iter().enumerate() {
                    let sender = sender.clone();
                    let slot = &progress[index];
                    let abort = &abort;
                    let path = temp_dir.join(format!("{index}.segment"));
                    let url = &url;
                    scope.spawn(move || {
                        let result = download_range(
                            url,
                            &path,
                            from,
                            to,
                            iso.size,
                            slot,
                            abort,
                            download_state,
                        );
                        let _ = sender.send(result);
                    });
                }
                drop(sender);
                let mut received = 0;
                let mut failure = None;
                while received < ranges.len() {
                    if download_state.load(Ordering::Acquire) == DOWNLOAD_CANCELLED {
                        abort.store(true, Ordering::Release);
                    }
                    match receiver.recv_timeout(Duration::from_millis(250)) {
                        Ok(Ok(())) => received += 1,
                        Ok(Err(error)) => {
                            received += 1;
                            abort.store(true, Ordering::Release);
                            if failure.is_none() {
                                failure = Some(error);
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            failure =
                                Some("Un fragmento terminó sin informar del resultado".into());
                            break;
                        }
                    }
                    if last_update.elapsed() >= Duration::from_millis(250) {
                        let in_flight = progress
                            .iter()
                            .map(|slot| slot.load(Ordering::Acquire))
                            .sum::<u64>();
                        let received_bytes = (*downloaded + in_flight).min(iso.size);
                        let speed = received_bytes.saturating_sub(initial_bytes) as f64
                            / start.elapsed().as_secs_f64().max(0.1);
                        let remaining = (iso.size.saturating_sub(received_bytes) as f64
                            / speed.max(1.0)) as u64;
                        let message = format!(
                            "Descargando · {} / {} · {}/s · {} restantes",
                            human(received_bytes),
                            human(iso.size),
                            human(speed as u64),
                            format_duration(remaining)
                        );
                        let fraction =
                            (received_bytes as f64 / iso.size as f64).clamp(0.0, 1.0) as f32;
                        ui(weak.clone(), move |window| {
                            window.set_status_text(message.into());
                            window.set_progress(fraction);
                        });
                        *last_update = Instant::now();
                    }
                }
                failure
            });
            check_download_cancellation(download_state)?;
            if let Some(error) = failure {
                log_event(format!(
                    "WARNING etapa=descarga_paralela motivo={error} reanudar_desde={}",
                    *downloaded
                ));
                break;
            }
            let mut output = if *downloaded == 0 {
                File::create(part)
                    .map_err(|error| file_error("crear descarga temporal", part, error))?
            } else {
                OpenOptions::new()
                    .append(true)
                    .open(part)
                    .map_err(|error| file_error("reanudar descarga temporal", part, error))?
            };
            let mut buffer = vec![0u8; 1024 * 1024];
            for (index, &(from, to)) in ranges.iter().enumerate() {
                let path = temp_dir.join(format!("{index}.segment"));
                let mut input = File::open(&path)
                    .map_err(|error| file_error("abrir fragmento temporal", &path, error))?;
                if input.metadata().map_err(|error| error.to_string())?.len() != to - from + 1 {
                    return Err("Un fragmento no tiene el tamaño esperado".into());
                }
                loop {
                    check_download_cancellation(download_state)?;
                    let length = input
                        .read(&mut buffer)
                        .map_err(|error| file_error("leer fragmento", &path, error))?;
                    if length == 0 {
                        break;
                    }
                    output
                        .write_all(&buffer[..length])
                        .map_err(|error| file_error("unir fragmentos", part, error))?;
                    hash.consume(&buffer[..length]);
                    *downloaded += length as u64;
                }
                drop(input);
                fs::remove_file(&path)
                    .map_err(|error| file_error("eliminar fragmento unido", &path, error))?;
            }
            output
                .sync_all()
                .map_err(|error| file_error("guardar descarga", part, error))?;
        }
        Ok(())
    })();
    let cleanup = fs::remove_dir_all(&temp_dir)
        .map_err(|error| file_error("eliminar fragmentos temporales", &temp_dir, error));
    result?;
    cleanup?;
    Ok(())
}

fn download_iso(
    iso: &Iso,
    weak: slint::Weak<MainWindow>,
    download_state: &AtomicU8,
) -> Result<PathBuf, String> {
    let dir = downloads_dir()?;
    download_iso_to(iso, weak, &dir, download_state)
}

fn download_iso_to(
    iso: &Iso,
    weak: slint::Weak<MainWindow>,
    dir: &Path,
    download_state: &AtomicU8,
) -> Result<PathBuf, String> {
    let result = download_iso_to_inner(iso, weak, dir, download_state);
    if download_state.load(Ordering::Acquire) == DOWNLOAD_CANCELLED
        || result
            .as_ref()
            .err()
            .is_some_and(|error| error == DOWNLOAD_CANCELLED_MESSAGE)
    {
        let part = dir.join(format!("{}.part", iso.name));
        let segments = dir.join(format!("{}.part-segments", iso.name));
        match fs::remove_file(&part) {
            Ok(()) => log_event(format!(
                "INFO etapa=descargar_iso cancelada archivo_parcial_eliminado={}",
                part.display()
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(file_error("eliminar descarga cancelada", &part, error)),
        }
        if segments.exists() {
            fs::remove_dir_all(&segments)
                .map_err(|error| file_error("eliminar fragmentos cancelados", &segments, error))?;
        }
        return Err(DOWNLOAD_CANCELLED_MESSAGE.into());
    }
    result
}

fn download_iso_to_inner(
    iso: &Iso,
    weak: slint::Weak<MainWindow>,
    dir: &Path,
    download_state: &AtomicU8,
) -> Result<PathBuf, String> {
    check_download_cancellation(download_state)?;
    fs::create_dir_all(dir).map_err(|e| file_error("crear carpeta de descarga", dir, e))?;
    let final_path = dir.join(&iso.name);
    log_event(format!(
        "INFO etapa=descargar_iso inicio nombre={} bytes={} destino={}",
        iso.name,
        iso.size,
        final_path.display()
    ));
    if final_path.exists()
        && verify_file(
            &final_path,
            iso,
            &weak,
            "Comprobando la ISO ya descargada",
            Some(download_state),
        )?
    {
        commit_download(download_state)?;
        log_event(format!(
            "INFO etapa=descargar_iso archivo_existente_verificado ruta={}",
            final_path.display()
        ));
        return Ok(final_path);
    }
    let part = dir.join(format!("{}.part", iso.name));
    let mut downloaded = fs::metadata(&part)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if downloaded > iso.size {
        fs::remove_file(&part)
            .map_err(|e| file_error("descartar descarga parcial inválida", &part, e))?;
        downloaded = 0;
    }
    if fs_free_bytes(&dir)? < iso.size.saturating_sub(downloaded) {
        return Err(format!(
            "No hay espacio suficiente para descargar {}",
            human(iso.size.saturating_sub(downloaded))
        ));
    }
    let mut hash = md5::Context::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    if downloaded > 0 {
        status(weak.clone(), "Verificando la descarga parcial…");
        let mut existing =
            File::open(&part).map_err(|e| file_error("abrir descarga parcial", &part, e))?;
        loop {
            check_download_cancellation(download_state)?;
            let len = existing
                .read(&mut buffer)
                .map_err(|e| file_error("leer descarga parcial", &part, e))?;
            if len == 0 {
                break;
            }
            hash.consume(&buffer[..len]);
        }
        log_event(format!(
            "INFO etapa=descargar_iso reanudar_desde_bytes={downloaded}"
        ));
    }
    if downloaded == iso.size {
        // El bucle anterior ya calculó el MD5 del archivo completo; no se vuelve a leer.
        let complete = std::mem::replace(&mut hash, md5::Context::new());
        let valid = iso.md5.as_ref().is_none_or(|expected| {
            format!("{:x}", complete.compute()).eq_ignore_ascii_case(expected)
        });
        if valid {
            commit_download(download_state)?;
            if final_path.exists() {
                fs::remove_file(&final_path)
                    .map_err(|e| file_error("reemplazar ISO antigua", &final_path, e))?;
            }
            fs::rename(&part, &final_path)
                .map_err(|e| file_error("finalizar descarga", &final_path, e))?;
            return Ok(final_path);
        }
        fs::remove_file(&part)
            .map_err(|e| file_error("descartar descarga incorrecta", &part, e))?;
        downloaded = 0;
    }
    let mut initial_bytes = downloaded;
    let mut start = Instant::now();
    let mut last_update = Instant::now() - Duration::from_secs(2);
    let mut last_log = Instant::now();
    let mut attempts = 0;
    let mut preferred_url = choose_download_mirror(iso, weak.clone(), download_state);
    check_download_cancellation(download_state)?;
    download_iso_parallel(
        iso,
        preferred_url.as_deref(),
        weak.clone(),
        dir,
        &part,
        &mut downloaded,
        &mut hash,
        download_state,
        initial_bytes,
        start,
        &mut last_update,
    )?;
    while downloaded < iso.size {
        check_download_cancellation(download_state)?;
        status(weak.clone(), "Conectando con SourceForge…");
        let selected_url = match preferred_url.take() {
            Some(url) => Ok(url),
            None => resolve_download(iso),
        };
        let direct_url = match selected_url {
            Ok(url) => url,
            Err(error) => {
                retry_iso_download(
                    weak.clone(),
                    &mut attempts,
                    downloaded,
                    &error,
                    download_state,
                )?;
                continue;
            }
        };
        check_download_cancellation(download_state)?;
        // Un límite para toda la petición cortaría una ISO de varios GB aunque
        // siguiera llegando; solo limitamos la conexión y los periodos sin datos.
        let agent = ureq::builder()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(10))
            .build();
        let mut request = agent
            .get(&direct_url)
            .set("User-Agent", "WinSlimUsbCreator/0.1");
        if downloaded > 0 {
            request = request.set("Range", &format!("bytes={downloaded}-"));
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(error) => {
                let reason = http_error("descargar ISO", error);
                retry_iso_download(
                    weak.clone(),
                    &mut attempts,
                    downloaded,
                    &reason,
                    download_state,
                )?;
                continue;
            }
        };
        check_download_cancellation(download_state)?;
        log_event(format!(
            "INFO etapa=descargar_iso http={} url_final={} content_type={:?} content_length={:?}",
            response.status(),
            response.get_url().split('?').next().unwrap_or(""),
            response.header("Content-Type"),
            response.header("Content-Length")
        ));
        if response
            .header("Content-Type")
            .is_some_and(|value| value.to_ascii_lowercase().contains("text/html"))
        {
            retry_iso_download(
                weak.clone(),
                &mut attempts,
                downloaded,
                "SourceForge devolvió una página HTML en vez de la ISO",
                download_state,
            )?;
            continue;
        }
        if downloaded > 0 && response.status() == 200 {
            log_event("WARNING etapa=descargar_iso servidor_ignoro_range reiniciando".into());
            downloaded = 0;
            hash = md5::Context::new();
            initial_bytes = 0;
            start = Instant::now();
        } else if downloaded > 0 {
            let expected = format!("bytes {downloaded}-");
            if response.status() != 206
                || !response
                    .header("Content-Range")
                    .is_some_and(|value| value.starts_with(&expected))
            {
                retry_iso_download(
                    weak.clone(),
                    &mut attempts,
                    downloaded,
                    "SourceForge no confirmó la posición de reanudación de la ISO",
                    download_state,
                )?;
                continue;
            }
        } else if response.status() != 200 {
            retry_iso_download(
                weak.clone(),
                &mut attempts,
                downloaded,
                "Respuesta inesperada al iniciar la descarga",
                download_state,
            )?;
            continue;
        }
        if response
            .header("Content-Length")
            .and_then(|s| s.parse::<u64>().ok())
            .is_some_and(|n| n != iso.size.saturating_sub(downloaded))
        {
            retry_iso_download(
                weak.clone(),
                &mut attempts,
                downloaded,
                "El tamaño servido no coincide con el RSS",
                download_state,
            )?;
            continue;
        }
        let mut reader = response.into_reader();
        let mut file = if downloaded > 0 {
            OpenOptions::new()
                .append(true)
                .open(&part)
                .map_err(|e| file_error("reanudar descarga temporal", &part, e))?
        } else {
            File::create(&part).map_err(|e| file_error("crear descarga temporal", &part, e))?
        };
        let connection_start = downloaded;
        let mut interrupted = None;
        while downloaded < iso.size {
            if download_state.load(Ordering::Acquire) == DOWNLOAD_CANCELLED {
                interrupted = Some(DOWNLOAD_CANCELLED_MESSAGE.into());
                break;
            }
            let read_limit = buffer.len().min((iso.size - downloaded) as usize);
            let len = match reader.read(&mut buffer[..read_limit]) {
                Ok(0) => {
                    interrupted =
                        Some("El espejo cerró la conexión antes de completar la ISO".to_owned());
                    break;
                }
                Ok(len) => len,
                Err(error) => {
                    interrupted = Some(format!("{} (os error {:?})", error, error.raw_os_error()));
                    break;
                }
            };
            if download_state.load(Ordering::Acquire) == DOWNLOAD_CANCELLED {
                interrupted = Some(DOWNLOAD_CANCELLED_MESSAGE.into());
                break;
            }
            file.write_all(&buffer[..len])
                .map_err(|e| file_error("escribir descarga temporal", &part, e))?;
            hash.consume(&buffer[..len]);
            downloaded += len as u64;
            if last_log.elapsed() >= Duration::from_secs(10) {
                log_event(format!(
                    "DEBUG etapa=descargar_iso progreso_bytes={} esperado_bytes={}",
                    downloaded, iso.size
                ));
                last_log = Instant::now();
            }
            if last_update.elapsed() >= Duration::from_millis(250) {
                let speed = downloaded.saturating_sub(initial_bytes) as f64
                    / start.elapsed().as_secs_f64().max(0.1);
                let remaining =
                    (iso.size.saturating_sub(downloaded) as f64 / speed.max(1.0)) as u64;
                let message = format!(
                    "Descargando · {} / {} · {}/s · {} restantes",
                    human(downloaded),
                    human(iso.size),
                    human(speed as u64),
                    format_duration(remaining)
                );
                let fraction = (downloaded as f64 / iso.size as f64).clamp(0.0, 1.0) as f32;
                ui(weak.clone(), move |window| {
                    window.set_status_text(message.into());
                    window.set_progress(fraction);
                });
                last_update = Instant::now();
            }
        }
        file.sync_all()
            .map_err(|e| file_error("guardar descarga", &part, e))?;
        drop(file);
        check_download_cancellation(download_state)?;
        // El límite de reintentos cuenta cortes seguidos sin avance, no todos los
        // cortes de una ISO de varios GB: si esta conexión recibió datos, se reinicia.
        if downloaded > connection_start {
            attempts = 0;
        }
        if let Some(reason) = interrupted {
            retry_iso_download(
                weak.clone(),
                &mut attempts,
                downloaded,
                &reason,
                download_state,
            )?;
        }
    }
    if downloaded != iso.size {
        return Err(format!(
            "Descarga incompleta: {} de {}",
            human(downloaded),
            human(iso.size)
        ));
    }
    if let Some(expected) = &iso.md5 {
        status(weak.clone(), "Verificando la integridad de la ISO…");
        let actual = format!("{:x}", hash.compute());
        if !actual.eq_ignore_ascii_case(expected) {
            log_event(format!(
                "ERROR etapa=verificar_iso md5_esperado={expected} md5_obtenido={actual}"
            ));
            // Un archivo completo con otro MD5 no se puede reanudar: conservarlo solo
            // obligaría al siguiente intento a leerlo entero antes de descartarlo.
            if let Err(error) = fs::remove_file(&part) {
                file_error("descartar descarga con MD5 incorrecto", &part, error);
            }
            return Err(
                "La suma MD5 no coincide con la publicada por SourceForge. Se descartó la descarga; pulsa Descargar para repetirla."
                    .into(),
            );
        }
    }
    commit_download(download_state)?;
    if final_path.exists() {
        fs::remove_file(&final_path)
            .map_err(|e| file_error("reemplazar ISO antigua", &final_path, e))?;
    }
    fs::rename(&part, &final_path).map_err(|e| file_error("finalizar descarga", &final_path, e))?;
    log_event(format!(
        "INFO etapa=descargar_iso completada bytes={} segundos={} ruta={}",
        downloaded,
        start.elapsed().as_secs(),
        final_path.display()
    ));
    Ok(final_path)
}

fn parse_mirror_choices(html: &str) -> Vec<String> {
    let Some(list) = html.split_once("<ul id=\"mirrorList\">") else {
        return Vec::new();
    };
    let Some((list, _)) = list.1.split_once("</ul>") else {
        return Vec::new();
    };
    let mut mirrors = Vec::new();
    for item in list.split("<li id=\"").skip(1) {
        let Some((name, _)) = item.split_once('"') else {
            continue;
        };
        if name != "autoselect"
            && !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && !mirrors.iter().any(|known: &String| known.as_str() == name)
        {
            mirrors.push(name.to_owned());
        }
    }
    mirrors
}

fn available_mirrors(iso: &Iso) -> Result<Vec<String>, String> {
    let response = ureq::get("https://sourceforge.net/settings/mirror_choices")
        .query("projectname", "winslim11-isos")
        .query("filename", &iso.name)
        .set("User-Agent", "WinSlimUsbCreator/0.1")
        .timeout(Duration::from_secs(8))
        .call()
        .map_err(|error| http_error("consultar espejos", error))?;
    let mut html = String::new();
    response
        .into_reader()
        .take(256 * 1024)
        .read_to_string(&mut html)
        .map_err(|error| format!("No se pudo leer la lista de espejos: {error}"))?;
    Ok(parse_mirror_choices(&html))
}

fn probe_mirror(
    iso: &Iso,
    mirror: &str,
    download_state: &AtomicU8,
) -> Option<(String, String, Duration)> {
    if download_state.load(Ordering::Acquire) == DOWNLOAD_CANCELLED {
        return None;
    }
    let sample = MIRROR_PROBE_BYTES.min(iso.size);
    let range = format!("bytes=0-{}", sample - 1);
    let url = format!("{}?use_mirror={mirror}", iso.url);
    let start = Instant::now();
    let response = ureq::builder()
        .timeout_connect(Duration::from_secs(3))
        .timeout_read(Duration::from_secs(4))
        .build()
        .get(&url)
        .set("User-Agent", "WinSlimUsbCreator/0.1")
        .set("Accept-Encoding", "identity")
        .set("Range", &range)
        .call()
        .ok()?;
    if response.status() != 206
        || response.header("Content-Range")
            != Some(format!("bytes 0-{}/{size}", sample - 1, size = iso.size).as_str())
        || response
            .header("Content-Type")
            .is_some_and(|value| value.to_ascii_lowercase().contains("text/html"))
    {
        return None;
    }
    let direct_url = response.get_url().to_owned();
    let actual_host = direct_url.strip_prefix("https://")?.split('/').next()?;
    let expected_host = format!("{mirror}.dl.sourceforge.net");
    if actual_host != expected_host.as_str() {
        return None;
    }
    if !direct_url.contains(&format!("/project/winslim11-isos/{}", iso.name)) {
        return None;
    }
    let mut reader = response.into_reader();
    let mut buffer = [0u8; 64 * 1024];
    let mut received = 0;
    while received < sample {
        if download_state.load(Ordering::Acquire) == DOWNLOAD_CANCELLED
            || start.elapsed() > Duration::from_secs(5)
        {
            return None;
        }
        let limit = (sample - received).min(buffer.len() as u64) as usize;
        let length = reader.read(&mut buffer[..limit]).ok()?;
        if length == 0 {
            return None;
        }
        received += length as u64;
    }
    Some((mirror.to_owned(), direct_url, start.elapsed()))
}

fn choose_download_mirror(
    iso: &Iso,
    weak: slint::Weak<MainWindow>,
    download_state: &AtomicU8,
) -> Option<String> {
    status(weak.clone(), "Consultando los espejos de SourceForge…");
    let mut mirrors = match available_mirrors(iso) {
        Ok(mirrors) => mirrors,
        Err(error) => {
            log_event(format!(
                "WARNING etapa=elegir_espejo motivo={error} usando_automatico"
            ));
            return None;
        }
    };
    if mirrors.len() <= 1 {
        log_event(format!(
            "INFO etapa=elegir_espejo disponibles={} unico={}",
            mirrors.len(),
            mirrors.first().map(String::as_str).unwrap_or("ninguno")
        ));
        status(weak, "Iniciando descarga de la ISO…");
        return None;
    }
    mirrors.sort_by_key(|mirror| mirror.as_str() == "master");
    mirrors.truncate(MAX_MIRROR_PROBES);
    status(weak.clone(), "Comparando la velocidad de los espejos…");
    let results = thread::scope(|scope| {
        let probes = mirrors
            .iter()
            .map(|mirror| scope.spawn(move || probe_mirror(iso, mirror, download_state)))
            .collect::<Vec<_>>();
        probes
            .into_iter()
            .filter_map(|probe| probe.join().ok().flatten())
            .collect::<Vec<_>>()
    });
    if download_state.load(Ordering::Acquire) == DOWNLOAD_CANCELLED {
        return None;
    }
    for (mirror, url, elapsed) in &results {
        log_event(format!(
            "INFO etapa=medir_espejo solicitado={mirror} servidor={} bytes={} segundos={:.2}",
            url.split('?').next().unwrap_or(""),
            MIRROR_PROBE_BYTES,
            elapsed.as_secs_f64()
        ));
    }
    let selected = results.into_iter().min_by_key(|(_, _, elapsed)| *elapsed);
    if let Some((mirror, url, _)) = selected {
        log_event(format!("INFO etapa=elegir_espejo seleccionado={mirror}"));
        status(
            weak,
            format!("Espejo seleccionado: {mirror}. Iniciando descarga…"),
        );
        Some(url)
    } else {
        log_event("WARNING etapa=elegir_espejo sin_candidatos_validos usando_automatico".into());
        None
    }
}

fn resolve_download(iso: &Iso) -> Result<String, String> {
    log_event(format!("DEBUG etapa=resolver_enlace url={}", iso.url));
    let response = ureq::get(&iso.url)
        .set("User-Agent", "WinSlimUsbCreator/0.1")
        .timeout(Duration::from_secs(10))
        .call()
        .map_err(|e| http_error("resolver espejo", e))?;
    log_event(format!(
        "DEBUG etapa=resolver_enlace http={} url_final={} content_type={:?}",
        response.status(),
        response.get_url().split('?').next().unwrap_or(""),
        response.header("Content-Type")
    ));
    if !response
        .header("Content-Type")
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/html"))
    {
        return Ok(response.get_url().to_owned());
    }
    let html = response.into_string().map_err(|e| e.to_string())?;
    let refresh = html
        .find("http-equiv=\"refresh\"")
        .ok_or("SourceForge no proporcionó un espejo de descarga")?;
    let rest = &html[refresh..];
    let start = rest.find("url=").ok_or("Enlace de espejo inválido")? + 4;
    let rest = &rest[start..];
    let end = rest.find('"').ok_or("Enlace de espejo incompleto")?;
    let url = rest[..end].replace("&amp;", "&");
    let expected = format!(
        "https://downloads.sourceforge.net/project/winslim11-isos/{}?",
        iso.name
    );
    if !url.starts_with(&expected) {
        log_event(format!(
            "ERROR etapa=resolver_enlace destino_inesperado={}",
            url.split('?').next().unwrap_or("")
        ));
        return Err("SourceForge devolvió un espejo inesperado".into());
    }
    log_event(format!(
        "INFO etapa=resolver_enlace espejo={}",
        url.split('?').next().unwrap_or("")
    ));
    Ok(url)
}

fn check_latest_download_available() -> Result<(), String> {
    let iso = fetch_latest()?;
    let url = resolve_download(&iso)?;
    // Solo leemos el primer byte: la comprobación no descarga la ISO.
    let response = ureq::builder()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(10))
        .build()
        .get(&url)
        .set("User-Agent", "WinSlimUsbCreator/0.1")
        .set("Accept-Encoding", "identity")
        .set("Range", "bytes=0-0")
        .call()
        .map_err(|error| http_error("comprobar disponibilidad de ISO", error))?;
    if response
        .header("Content-Type")
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/html"))
    {
        return Err("El servidor devolvió una página HTML en lugar de la ISO".into());
    }
    let correct_file = match response.status() {
        206 => response.header("Content-Range") == Some(format!("bytes 0-0/{}", iso.size).as_str()),
        200 => {
            response
                .header("Content-Length")
                .and_then(|value| value.parse::<u64>().ok())
                == Some(iso.size)
        }
        _ => false,
    };
    if !correct_file {
        return Err("El servidor no confirmó el tamaño de la ISO publicada".into());
    }
    let mut reader = response.into_reader();
    let mut first_byte = [0u8; 1];
    if reader
        .read(&mut first_byte)
        .map_err(|error| format!("No se pudo leer la ISO: {error}"))?
        != 1
    {
        return Err("El servidor no entregó datos de la ISO".into());
    }
    Ok(())
}

fn start_server_check(weak: slint::Weak<MainWindow>, running: Arc<AtomicBool>) {
    if running.swap(true, Ordering::AcqRel) {
        return;
    }
    thread::spawn(move || {
        // Se libera aunque el hilo falle; si no, no habría más comprobaciones.
        struct Release(Arc<AtomicBool>);
        impl Drop for Release {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _release = Release(running);
        let result = check_latest_download_available();
        let available = result.is_ok();
        match result {
            Ok(()) => log_event("INFO etapa=comprobar_servidor iso_disponible=true".into()),
            Err(error) => log_event(format!(
                "WARNING etapa=comprobar_servidor iso_disponible=false detalle={error}"
            )),
        }
        // Ambos estados se publican juntos para no mostrar «Descarga no disponible»
        // mientras todavía se comprueba la alternativa.
        let alternative = if available {
            0
        } else {
            alternative_availability()
        };
        ui(weak, move |window| {
            window.set_server_availability(if available { 1 } else { -1 });
            window.set_alternative_availability(alternative);
        });
    });
}

/// Comprueba tamaño y MD5 de una ISO. Leer varios GB tarda, así que muestra el
/// avance con `label` y, si se indica `cancel`, se detiene al cancelar la descarga.
fn verify_file(
    path: &Path,
    iso: &Iso,
    weak: &slint::Weak<MainWindow>,
    label: &str,
    cancel: Option<&AtomicU8>,
) -> Result<bool, String> {
    let length = fs::metadata(path)
        .map_err(|e| file_error("comprobar ISO", path, e))?
        .len();
    if length != iso.size {
        return Ok(false);
    }
    let Some(expected) = &iso.md5 else {
        return Ok(true);
    };
    status(weak.clone(), format!("{label}…"));
    let mut input = File::open(path).map_err(|e| file_error("abrir ISO", path, e))?;
    let mut hash = md5::Context::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut checked = 0u64;
    let mut last_update = Instant::now();
    loop {
        if let Some(cancel) = cancel {
            check_download_cancellation(cancel)?;
        }
        let len = input
            .read(&mut buffer)
            .map_err(|e| file_error("leer ISO", path, e))?;
        if len == 0 {
            break;
        }
        hash.consume(&buffer[..len]);
        checked += len as u64;
        if last_update.elapsed() >= Duration::from_millis(250) {
            let message = format!("{label} · {} / {}", human(checked), human(iso.size));
            let fraction = (checked as f64 / iso.size as f64).clamp(0.0, 1.0) as f32;
            ui(weak.clone(), move |window| {
                window.set_status_text(message.into());
                window.set_progress(fraction);
            });
            last_update = Instant::now();
        }
    }
    // La barra vuelve a cero para la fase siguiente (descarga o copia al USB).
    ui(weak.clone(), |window| window.set_progress(0.0));
    Ok(format!("{:x}", hash.compute()).eq_ignore_ascii_case(expected))
}

fn format_duration(seconds: u64) -> String {
    if seconds >= 3600 {
        format!("{} h {} min", seconds / 3600, seconds % 3600 / 60)
    } else {
        format!("{} min {} s", seconds / 60, seconds % 60)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum UsbAction {
    CopyExisting,
    InstallFresh,
    Reinstall,
}

fn usb_action(
    ventoy_expected: bool,
    ventoy_found: bool,
    force_reinstall: bool,
) -> Result<UsbAction, String> {
    if ventoy_expected && !ventoy_found {
        return Err("Ventoy ya no se detecta en el USB. No se ha formateado; actualiza la lista y vuelve a seleccionarlo.".into());
    }
    if !ventoy_expected && ventoy_found {
        return Err("El USB cambió desde la selección: ahora se detecta Ventoy. No se ha formateado; actualiza la lista y confirma de nuevo.".into());
    }
    if force_reinstall {
        if !ventoy_found {
            return Err(
                "No se puede reinstalar Ventoy porque ya no está presente en el USB.".into(),
            );
        }
        Ok(UsbAction::Reinstall)
    } else if ventoy_found {
        Ok(UsbAction::CopyExisting)
    } else {
        Ok(UsbAction::InstallFresh)
    }
}

const WINSLIM_THEME_PATH: &str = "/ventoy/winslim-theme/theme.txt";

fn winslim_ventoy_config(existing: Option<&[u8]>) -> Result<Option<Vec<u8>>, String> {
    let mut config: serde_json::Value = if let Some(bytes) = existing {
        serde_json::from_slice(bytes)
            .map_err(|error| format!("La configuración de Ventoy no es JSON válido: {error}"))?
    } else {
        serde_json::json!({
            "control": [
                {"VTOY_DEFAULT_MENU_MODE": "0"},
                {"VTOY_FILT_DOT_UNDERSCORE_FILE": "1"},
                {"VTOY_MENU_LANGUAGE": "es_ES"}
            ]
        })
    };
    let settings = config
        .as_object_mut()
        .ok_or("La configuración de Ventoy debe ser un objeto JSON")?;
    if let Some(theme) = settings.get("theme") {
        let Some(theme) = theme.as_object() else {
            return Ok(None);
        };
        if theme
            .get("file")
            .is_some_and(|file| file.as_str() != Some(WINSLIM_THEME_PATH) && !file.is_null())
        {
            return Ok(None);
        }
    }
    let controls = settings
        .entry("control")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .ok_or("El campo control de Ventoy debe ser una lista JSON")?;
    let language_found = controls.iter().any(|control| {
        control
            .as_object()
            .is_some_and(|object| object.contains_key("VTOY_MENU_LANGUAGE"))
    });
    if !language_found {
        controls.push(serde_json::json!({"VTOY_MENU_LANGUAGE": "es_ES"}));
    }
    settings.insert(
        "theme".into(),
        serde_json::json!({
            "file": WINSLIM_THEME_PATH,
            "display_mode": "GUI",
            // Ventoy inyecta su versión en el tema; el complemento solo permite
            // cambiar posición y color. En esta zona el fondo es negro liso.
            "ventoy_left": "40%",
            "ventoy_top": "88%",
            "ventoy_color": "#000000"
        }),
    );
    serde_json::to_vec_pretty(&config)
        .map(Some)
        .map_err(|error| format!("No se pudo serializar la configuración de Ventoy: {error}"))
}

fn install_winslim_ventoy_theme(root: &Path) -> Result<bool, String> {
    let config_dir = root.join("ventoy");
    let config_path = config_dir.join("ventoy.json");
    let existing = if config_path.exists() {
        Some(
            fs::read(&config_path)
                .map_err(|error| file_error("leer Ventoy", &config_path, error))?,
        )
    } else {
        None
    };
    let Some(config_bytes) = winslim_ventoy_config(existing.as_deref())? else {
        return Ok(false);
    };

    let theme_dir = config_dir.join("winslim-theme");
    fs::create_dir_all(&theme_dir).map_err(|error| file_error("crear tema", &theme_dir, error))?;
    for (name, contents) in [
        (
            "background.png",
            include_bytes!("../assets/ventoy-theme/background.png").as_slice(),
        ),
        (
            "icon.png",
            include_bytes!("../assets/ventoy-theme/icon.png").as_slice(),
        ),
        (
            "select_c.png",
            include_bytes!("../assets/ventoy-theme/select_c.png").as_slice(),
        ),
        (
            "theme.txt",
            include_bytes!("../assets/ventoy-theme/theme.txt").as_slice(),
        ),
    ] {
        let path = theme_dir.join(name);
        fs::write(&path, contents).map_err(|error| file_error("guardar tema", &path, error))?;
    }

    // Sustituir el JSON solo cuando todos los archivos del tema estén presentes.
    let pending = config_dir.join(format!("ventoy.json.winslim-{}.tmp", std::process::id()));
    let result = (|| -> Result<(), String> {
        let mut file = File::create(&pending)
            .map_err(|error| file_error("crear configuración temporal", &pending, error))?;
        file.write_all(&config_bytes)
            .map_err(|error| file_error("escribir configuración temporal", &pending, error))?;
        file.sync_all()
            .map_err(|error| file_error("sincronizar configuración temporal", &pending, error))?;
        drop(file);
        publish_config(&pending, &config_path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&pending);
    }
    result.map(|_| true)
}

fn unused_iso_path(root: &Path, name: &str) -> PathBuf {
    let original = root.join(name);
    if !original.exists() {
        return original;
    }
    let stem = name
        .strip_suffix(".iso")
        .or_else(|| name.strip_suffix(".ISO"))
        .unwrap_or(name);
    for number in 2.. {
        let candidate = root.join(format!("{stem} ({number}).iso"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

// Prefijo de la copia temporal de la ISO en el USB, común a ambas plataformas.
const USB_COPY_PREFIX: &str = ".winslim-copy-";

fn usb_copy_temp_path(root: &Path) -> Result<PathBuf, String> {
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    Ok(root.join(format!(
        "{USB_COPY_PREFIX}{}-{nanos}.part",
        std::process::id()
    )))
}

/// Borra copias temporales que quedaron en el USB al cerrar la aplicación o
/// desconectar la unidad en mitad de una copia; pueden ocupar una ISO entera.
fn remove_stale_usb_copies(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(USB_COPY_PREFIX)
            || !name.ends_with(".part")
            || !entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            continue;
        }
        let path = entry.path();
        match fs::remove_file(&path) {
            Ok(()) => log_event(format!(
                "INFO etapa=limpiar_usb copia_temporal_eliminada={}",
                path.display()
            )),
            Err(error) => log_event(format!(
                "WARNING etapa=limpiar_usb ruta={} detalle={error}",
                path.display()
            )),
        }
    }
}

fn copy_with_progress(
    from: &Path,
    to: &Path,
    total: u64,
    weak: slint::Weak<MainWindow>,
) -> Result<(), String> {
    copy_and_verify(from, to, total, |copied, verifying| {
        let label = if verifying {
            "Verificando ISO"
        } else {
            "Copiando ISO"
        };
        let message = format!("{} · {} / {}", label, human(copied), human(total));
        let fraction = (copied as f64 / total as f64).clamp(0.0, 1.0) as f32;
        ui(weak.clone(), move |window| {
            window.set_status_text(message.into());
            window.set_progress(fraction);
        });
    })
}

const CHUNK_SIZE: usize = 8 * 1024 * 1024;
// Alineación válida para leer sin caché en sectores de 512 B y de 4 KiB.
const UNCACHED_ALIGNMENT: usize = 4096;

/// Calcula el MD5 de la copia escrita en el USB. Con `uncached`, la lectura
/// evita la caché del sistema (ver `open_uncached` en cada plataforma).
fn read_back_md5(
    path: &Path,
    uncached: bool,
    progress: &mut impl FnMut(u64),
) -> std::io::Result<(u64, md5::Digest)> {
    let mut file = if uncached {
        open_uncached(path)?
    } else {
        File::open(path)?
    };
    let length = file.metadata()?.len();
    let mut storage = vec![0u8; CHUNK_SIZE + UNCACHED_ALIGNMENT];
    let offset = storage
        .as_ptr()
        .align_offset(UNCACHED_ALIGNMENT)
        .min(UNCACHED_ALIGNMENT);
    let buffer = &mut storage[offset..offset + CHUNK_SIZE];
    let mut hash = md5::Context::new();
    let mut total = 0u64;
    // Se detiene al alcanzar el tamaño del archivo: sin caché, una lectura más
    // tras el último bloque parcial partiría de una posición no alineada.
    while total < length {
        let len = file.read(buffer)?;
        if len == 0 {
            break;
        }
        hash.consume(&buffer[..len]);
        total += len as u64;
        progress(total);
    }
    Ok((total, hash.compute()))
}

fn copy_and_verify(
    from: &Path,
    to: &Path,
    total: u64,
    mut progress: impl FnMut(u64, bool),
) -> Result<(), String> {
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)
        .map_err(|e| file_error("crear copia temporal USB", to, e))?;
    // Dos bloques reutilizables permiten leer la ISO mientras el USB escribe.
    let (free_tx, free_rx) = mpsc::sync_channel::<Vec<u8>>(2);
    let (filled_tx, filled_rx) = mpsc::sync_channel::<Result<(Vec<u8>, usize), String>>(2);
    for _ in 0..2 {
        free_tx
            .send(vec![0u8; CHUNK_SIZE])
            .map_err(|e| e.to_string())?;
    }
    let mut copied = 0u64;
    let mut last_update = Instant::now() - Duration::from_secs(2);
    let started = Instant::now();
    let source_hash = thread::scope(|scope| {
        let reader = scope.spawn(move || -> Result<(u64, md5::Digest), String> {
            let mut source = File::open(from).map_err(|e| file_error("abrir ISO", from, e))?;
            let mut hash = md5::Context::new();
            let mut read_bytes = 0u64;
            while let Ok(mut buffer) = free_rx.recv() {
                let len = match source.read(&mut buffer) {
                    Ok(len) => len,
                    Err(e) => {
                        let message = file_error("leer ISO", from, e);
                        let _ = filled_tx.send(Err(message.clone()));
                        return Err(message);
                    }
                };
                if len == 0 {
                    break;
                }
                hash.consume(&buffer[..len]);
                read_bytes += len as u64;
                if filled_tx.send(Ok((buffer, len))).is_err() {
                    return Err("Se interrumpió la escritura de la ISO".into());
                }
            }
            Ok((read_bytes, hash.compute()))
        });
        let mut write_result = Ok(());
        for chunk in filled_rx.iter() {
            let (buffer, len) = match chunk {
                Ok(chunk) => chunk,
                Err(e) => {
                    write_result = Err(e);
                    break;
                }
            };
            if let Err(e) = target.write_all(&buffer[..len]) {
                write_result = Err(file_error("escribir ISO en USB", to, e));
                break;
            }
            copied += len as u64;
            if last_update.elapsed() >= Duration::from_millis(250) {
                progress(copied, false);
                last_update = Instant::now();
            }
            // El lector puede haber alcanzado EOF y cerrado este canal mientras
            // el escritor terminaba el último bloque. Su resultado se comprueba abajo.
            let _ = free_tx.send(buffer);
        }
        drop(filled_rx);
        drop(free_tx);
        let source_result = reader
            .join()
            .map_err(|_| "Falló el proceso de lectura de la ISO".to_string())?;
        write_result?;
        source_result
    })?;
    target.sync_all().map_err(|e| e.to_string())?;
    drop(target);
    if copied != total || source_hash.0 != total {
        return Err("La copia al USB quedó incompleta".into());
    }
    log_event(format!(
        "INFO etapa=copiar_iso bytes={} segundos={}",
        copied,
        started.elapsed().as_secs()
    ));
    progress(0, true);
    let mut report = |verified: u64| {
        if last_update.elapsed() >= Duration::from_millis(250) {
            progress(verified, true);
            last_update = Instant::now();
        }
    };
    // Se lee del USB sin la caché del sistema: si no, casi toda la ISO recién
    // escrita saldría de la RAM y un pendrive defectuoso o de capacidad falsa
    // pasaría la verificación.
    let (verified, digest) = match read_back_md5(to, true, &mut report) {
        Ok(result) => result,
        // Windows exige alineación de sector al leer sin caché; si la unidad la
        // rechaza, se repite con lectura normal para no bloquear la preparación.
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
            log_event(format!(
                "WARNING etapa=verificar_copia_iso lectura_sin_cache_no_disponible detalle={error}"
            ));
            read_back_md5(to, false, &mut report)
                .map_err(|e| file_error("verificar copia USB", to, e))?
        }
        Err(error) => return Err(file_error("verificar copia USB", to, error)),
    };
    if verified != total || digest != source_hash.1 {
        return Err(
            "La verificación de la ISO en el USB falló; vuelve a preparar la unidad".into(),
        );
    }
    log_event(format!(
        "INFO etapa=verificar_copia_iso bytes={} segundos={}",
        verified,
        started.elapsed().as_secs()
    ));
    progress(total, true);
    Ok(())
}

fn main() -> Result<(), slint::PlatformError> {
    // Se conserva el gestor por defecto para seguir viendo el pánico en la consola de depuración.
    let default_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log_event(format!("ERROR panic={info}"));
        default_panic_hook(info);
    }));
    log_event(format!(
        "INFO evento=inicio version={} sistema={} arquitectura={}",
        env!("CARGO_PKG_VERSION"),
        os_version(),
        std::env::consts::ARCH
    ));
    let window = MainWindow::new()?;
    size_initial_window(&window);
    window.set_restart_instructions(restart_instructions().into());
    // En Windows, softbuffer puede reutilizar su caché de daños aunque el área
    // cliente se haya vaciado al minimizar. Invalidar el fondo obliga a Slint
    // a pintar de nuevo toda la ventana al restaurarla o maximizarla.
    #[cfg(target_os = "windows")]
    {
        let weak = window.as_weak();
        let redraw_pending = Rc::new(Cell::new(false));
        window.window().on_winit_window_event(move |_, event| {
            let needs_full_redraw = match event {
                WindowEvent::Focused(true) | WindowEvent::Occluded(false) => true,
                WindowEvent::Resized(size) => size.width > 0 && size.height > 0,
                _ => false,
            };
            if needs_full_redraw && !redraw_pending.replace(true) {
                let weak = weak.clone();
                let redraw_pending = redraw_pending.clone();
                Timer::single_shot(Duration::ZERO, move || {
                    redraw_pending.set(false);
                    if let Some(window) = weak.upgrade() {
                        window.set_redraw_flip(!window.get_redraw_flip());
                        window.window().request_redraw();
                    }
                });
            }
            EventResult::Propagate
        });
    }
    let state = Arc::new(Mutex::new(State::default()));
    let download_state = Arc::new(AtomicU8::new(DOWNLOAD_IDLE));

    {
        let weak = window.as_weak();
        thread::spawn(move || {
            let detected = firmware_security_status();
            log_event(format!(
                "INFO etapa=seguridad_firmware tpm={:?} secure_boot={:?}",
                detected.tpm, detected.secure_boot
            ));
            ui(weak, move |window| {
                window.set_tpm_status(detected.tpm.ui_value());
                window.set_secure_boot_status(detected.secure_boot.ui_value());
            });
        });
    }

    {
        let weak = window.as_weak();
        let download_state = download_state.clone();
        window.on_cancel_download(move || {
            if download_state
                .compare_exchange(
                    DOWNLOAD_RUNNING,
                    DOWNLOAD_CANCELLED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                if let Some(window) = weak.upgrade() {
                    window.set_download_cancelling(true);
                    window.set_status_text(
                        "Cancelando descarga y eliminando el archivo parcial…".into(),
                    );
                }
            }
        });
    }

    {
        let weak = window.as_weak();
        window.on_close_success(move || {
            if let Some(window) = weak.upgrade() {
                window.set_success_visible(false);
            }
        });
    }
    window.on_close_app(|| {
        let _ = slint::quit_event_loop();
    });
    // Cerrar durante una descarga o la preparación del USB la interrumpe a medias:
    // se pide confirmación en vez de cerrar directamente.
    {
        let weak = window.as_weak();
        window.window().on_close_requested(move || {
            let Some(window) = weak.upgrade() else {
                return slint::CloseRequestResponse::HideWindow;
            };
            if window.get_download_active() || window.get_preparing() {
                window.set_close_confirm_visible(true);
                slint::CloseRequestResponse::KeepWindowShown
            } else {
                slint::CloseRequestResponse::HideWindow
            }
        });
    }
    window.on_confirm_close(|| {
        log_event("WARNING evento=cierre_confirmado_con_operacion_en_curso".into());
        let _ = slint::quit_event_loop();
    });
    {
        let weak = window.as_weak();
        window.on_cancel_close(move || {
            if let Some(window) = weak.upgrade() {
                window.set_close_confirm_visible(false);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_restart_to_usb(move || {
            if let Some(window) = weak.upgrade() {
                if window.get_restart_pending() {
                    return;
                }
                window.set_restart_pending(true);
                window.set_restart_message("Reiniciando hacia las opciones de inicio…".into());
            }
            let weak = weak.clone();
            thread::spawn(move || {
                let error = match restart_to_usb() {
                    Ok(()) => return,
                    Err(error) => error,
                };
                log_event(format!("ERROR etapa=reiniciar_usb detalle={error}"));
                ui(weak, move |window| {
                    window.set_restart_pending(false);
                    window.set_restart_message(error.into());
                });
            });
        });
    }

    let log_timer = Timer::default();
    {
        let weak = window.as_weak();
        // Tamaño y fecha del registro en la última lectura: si no cambian, no se relee.
        let mut last_seen = None;
        log_timer.start(TimerMode::Repeated, Duration::from_millis(700), move || {
            let Some(w) = weak.upgrade() else { return };
            if !w.get_logs_visible() {
                last_seen = None;
                return;
            }
            let current = log_path()
                .and_then(|path| fs::metadata(path).ok())
                .map(|metadata| (metadata.len(), metadata.modified().ok()));
            if current.is_some() && current == last_seen {
                return;
            }
            last_seen = current;
            let contents = read_log();
            if w.get_log_text().as_str() != contents {
                w.set_log_text(contents.into());
            }
        });
    }

    let server_timer = Timer::default();
    {
        let weak = window.as_weak();
        let running = Arc::new(AtomicBool::new(false));
        start_server_check(weak.clone(), running.clone());
        // Cada comprobación consulta el RSS, el enlace de descarga y un espejo;
        // diez minutos bastan para el indicador sin cargar a SourceForge.
        server_timer.start(TimerMode::Repeated, Duration::from_secs(600), move || {
            if weak.upgrade().is_some_and(|window| !window.get_busy()) {
                start_server_check(weak.clone(), running.clone());
            }
        });
    }

    {
        let weak = window.as_weak();
        window.on_show_logs(move || {
            if let Some(w) = weak.upgrade() {
                w.set_log_text(read_log().into());
                w.set_logs_visible(true);
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_close_logs(move || {
            if let Some(w) = weak.upgrade() {
                w.set_logs_visible(false);
            }
        });
    }
    window.on_copy_logs(move || {
        let result =
            arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(read_log()));
        match result {
            Ok(()) => log_event("INFO etapa=registro copiar_portapapeles=OK".into()),
            Err(e) => log_event(format!("ERROR etapa=registro copiar_portapapeles={e}")),
        }
    });
    window.on_open_log_folder(move || {
        if let Some(path) = log_path() {
            if let Some(dir) = path.parent() {
                if let Err(e) = open_folder(dir) {
                    log_event(format!("ERROR etapa=registro abrir_carpeta={e}"));
                }
            }
        }
    });
    {
        let weak = window.as_weak();
        window.on_clear_logs(move || {
            match clear_log() {
                Ok(()) => log_event("INFO etapa=registro limpiado_por_usuario".into()),
                Err(e) => log_event(format!("ERROR etapa=registro limpiar={e}")),
            }
            if let Some(w) = weak.upgrade() {
                w.set_log_text(read_log().into());
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_export_logs(move || {
            let Some(target) = rfd::FileDialog::new()
                .set_title("Guardar copia del registro")
                .set_file_name("WinSlimUsbCreator-log.txt")
                .add_filter("Archivo de texto", &["txt"])
                .save_file()
            else {
                return;
            };
            let result = log_path()
                .ok_or_else(|| "No se encontró el directorio del registro".to_owned())
                .and_then(|source| export_log(&source, &target));
            if let Some(w) = weak.upgrade() {
                match result {
                    Ok(_) => {
                        w.set_status_text(
                            format!("Registro exportado: {}", target.display()).into(),
                        );
                        log_event(format!(
                            "INFO etapa=registro exportado={}",
                            target.display()
                        ));
                    }
                    Err(e) => {
                        w.set_status_text(format!("No se pudo exportar el registro: {e}").into());
                        log_event(format!("ERROR etapa=registro exportar={e}"));
                    }
                }
            }
        });
    }

    {
        let weak = window.as_weak();
        let state = state.clone();
        window.on_lookup_iso(move || {
            let Some(window) = weak.upgrade() else { return };
            if !begin(&state, &window) {
                return;
            }
            window.set_status_text("Consultando SourceForge…".into());
            let weak = weak.clone();
            let state = state.clone();
            thread::spawn(move || {
                let _guard = PanicGuard::new(&weak, &state, None);
                let result = fetch_latest();
                if let Ok(iso) = &result {
                    let mut current = lock_state(&state);
                    let retained = if current.iso.as_ref().is_some_and(|previous| {
                        previous.url == iso.url
                            && previous.name == iso.name
                            && previous.size == iso.size
                            && previous.md5 == iso.md5
                    }) {
                        current
                            .local_iso
                            .as_ref()
                            .filter(|path| {
                                fs::metadata(path).is_ok_and(|metadata| {
                                    metadata.is_file() && metadata.len() == iso.size
                                })
                            })
                            .cloned()
                    } else {
                        None
                    };
                    current.iso = Some(iso.clone());
                    current.local_iso = retained.clone();
                    if retained.is_none() {
                        current.selected = None;
                    }
                    let name = iso.name.clone();
                    let size = human(iso.size);
                    let local_file = retained
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "Sin descargar".into());
                    ui(weak.clone(), move |w| {
                        w.set_iso_name(name.into());
                        w.set_iso_size(size.into());
                        w.set_iso_stage(if retained.is_some() { 3 } else { 1 });
                        w.set_local_file(local_file.into());
                        if retained.is_none() {
                            w.set_selected_disk(-1);
                            w.set_reuse_ventoy(false);
                        }
                    });
                }
                let result = match result {
                    Ok(_) => Ok("ISO más reciente encontrada".into()),
                    Err(error) if report_main_server_failure(weak.clone()) => {
                        log_event(format!("WARNING etapa=consultar_iso detalle={error}"));
                        Ok(MAIN_SERVER_UNAVAILABLE_MESSAGE.into())
                    }
                    Err(error) => Err(error),
                };
                finish(weak, result, state);
            });
        });
    }

    {
        let weak = window.as_weak();
        let state = state.clone();
        let download_state = download_state.clone();
        window.on_download_latest_iso(move || {
            let Some(window) = weak.upgrade() else { return };
            if !begin(&state, &window) {
                return;
            }
            download_state.store(DOWNLOAD_RUNNING, Ordering::Release);
            window.set_download_active(true);
            window.set_download_cancelling(false);
            window.set_status_text("Buscando la última ISO de WinSlim…".into());
            let weak = weak.clone();
            let state = state.clone();
            let download_state = download_state.clone();
            thread::spawn(move || {
                let _guard = PanicGuard::new(&weak, &state, Some(&download_state));
                let result = match fetch_latest() {
                    Ok(iso) => {
                        if download_state.load(Ordering::Acquire) == DOWNLOAD_RUNNING {
                            status(weak.clone(), "Descargando la última ISO de WinSlim…");
                        }
                        download_iso(&iso, weak.clone(), &download_state).map(|path| (iso, path))
                    }
                    Err(_) if download_state.load(Ordering::Acquire) == DOWNLOAD_CANCELLED => {
                        Err(DOWNLOAD_CANCELLED_MESSAGE.into())
                    }
                    Err(error) if report_main_server_failure(weak.clone()) => {
                        log_event(format!("WARNING etapa=consultar_iso detalle={error}"));
                        Err(MAIN_SERVER_UNAVAILABLE_MESSAGE.into())
                    }
                    Err(error) => Err(error),
                };
                if let Ok((iso, path)) = &result {
                    {
                        let mut current = lock_state(&state);
                        current.iso = Some(iso.clone());
                        current.local_iso = Some(path.clone());
                        current.selected = None;
                    }
                    let name = iso.name.clone();
                    let size = human(iso.size);
                    let local_file = path.to_string_lossy().into_owned();
                    ui(weak.clone(), move |window| {
                        window.set_iso_name(name.into());
                        window.set_iso_size(size.into());
                        window.set_server_availability(1);
                        window.set_local_file(local_file.into());
                        window.set_iso_stage(3);
                        window.set_selected_disk(-1);
                        window.set_reuse_ventoy(false);
                        window.set_progress(1.0);
                    });
                } else {
                    ui(weak.clone(), |window| window.set_progress(0.0));
                }
                finish_download(
                    weak,
                    result.map(|_| "Última ISO descargada y verificada".into()),
                    state,
                    download_state,
                );
            });
        });
    }

    {
        let weak = window.as_weak();
        let state = state.clone();
        let download_state = download_state.clone();
        window.on_download_iso(move || {
            let Some(window) = weak.upgrade() else { return };
            if !begin(&state, &window) {
                return;
            }
            download_state.store(DOWNLOAD_RUNNING, Ordering::Release);
            window.set_download_active(true);
            window.set_download_cancelling(false);
            let iso = lock_state(&state).iso.clone();
            let weak = weak.clone();
            let state = state.clone();
            let download_state = download_state.clone();
            thread::spawn(move || {
                let _guard = PanicGuard::new(&weak, &state, Some(&download_state));
                let result = iso
                    .ok_or("Busca primero una ISO".to_owned())
                    .and_then(|iso| {
                        download_iso(&iso, weak.clone(), &download_state).map(|path| (iso, path))
                    });
                if let Ok((iso, path)) = &result {
                    {
                        let mut current = lock_state(&state);
                        current.local_iso = Some(path.clone());
                        current.selected = None;
                    }
                    let label = path.to_string_lossy().to_string();
                    let size = human(iso.size);
                    ui(weak.clone(), move |w| {
                        w.set_local_file(label.into());
                        w.set_iso_size(size.into());
                        w.set_server_availability(1);
                        w.set_iso_stage(3);
                        w.set_progress(1.0);
                        w.set_selected_disk(-1);
                        w.set_reuse_ventoy(false);
                    });
                }
                finish_download(
                    weak,
                    result.map(|_| "Descarga completada y verificada".into()),
                    state,
                    download_state,
                );
            });
        });
    }

    {
        let weak = window.as_weak();
        let state = state.clone();
        window.on_download_alternative_iso(move || {
            let Some(window) = weak.upgrade() else { return };
            if !begin(&state, &window) {
                return;
            }
            window.set_status_text("Consultando el enlace alternativo en GitHub…".into());
            let weak = weak.clone();
            let state = state.clone();
            thread::spawn(move || {
                let _guard = PanicGuard::new(&weak, &state, None);
                let result = fetch_available_alternative_link();
                let alternative = if result.is_ok() { 1 } else { -1 };
                ui(weak.clone(), move |window| {
                    window.set_alternative_availability(alternative)
                });
                let result = result.and_then(|link| {
                    open_url(link.as_str())
                        .map_err(|error| format!("No se pudo abrir el navegador: {error}"))?;
                    log_event(format!(
                        "INFO etapa=descarga_alternativa dominio={}",
                        link.host_str().unwrap_or("")
                    ));
                    Ok("Enlace de descarga alternativo abierto en el navegador".into())
                });
                finish(weak, result, state);
            });
        });
    }

    {
        let weak = window.as_weak();
        let state = state.clone();
        window.on_load_local_iso(move || {
            let Some(window) = weak.upgrade() else { return };
            if lock_state(&state).busy {
                return;
            }
            let Some(path) = rfd::FileDialog::new()
                .set_title("Seleccionar una imagen ISO")
                .add_filter("Imagen ISO", &["iso"])
                .pick_file()
            else {
                return;
            };
            if !path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("iso"))
            {
                window.set_status_text("Selecciona un archivo con extensión .iso".into());
                return;
            }
            let size = match fs::metadata(&path) {
                Ok(meta) if meta.is_file() && meta.len() >= 100_000_000 => meta.len(),
                Ok(_) => {
                    window.set_status_text(
                        "La ISO local no es un archivo válido o está incompleta".into(),
                    );
                    return;
                }
                Err(error) => {
                    window
                        .set_status_text(format!("No se pudo abrir la ISO local: {error}").into());
                    return;
                }
            };
            let Some(name) = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                return;
            };
            let iso = Iso {
                name: name.clone(),
                url: String::new(),
                size,
                md5: None,
                published: 0,
            };
            {
                let mut current = lock_state(&state);
                current.iso = Some(iso);
                current.local_iso = Some(path.clone());
                current.selected = None;
            }
            log_event(format!(
                "INFO etapa=iso_local seleccionada={} bytes={size}",
                path.display()
            ));
            window.set_iso_name(name.into());
            window.set_iso_size(human(size).into());
            window.set_local_file(path.to_string_lossy().into_owned().into());
            window.set_iso_stage(2);
            window.set_preparation_complete(false);
            window.set_selected_disk(-1);
            window.set_reuse_ventoy(false);
            window.set_status_text("ISO local cargada. Selecciona un USB para continuar.".into());
            window.set_progress(0.0);
        });
    }

    {
        let weak = window.as_weak();
        let state = state.clone();
        window.on_refresh_usb(move || {
            let Some(window) = weak.upgrade() else { return };
            if !begin(&state, &window) {
                return;
            }
            window.set_usb_detecting(true);
            window.set_status_text("Buscando unidades USB…".into());
            log_event("INFO etapa=detectar_usb inicio".into());
            let weak = weak.clone();
            let state = state.clone();
            thread::spawn(move || {
                let _guard = PanicGuard::new(&weak, &state, None);
                let result = disks();
                if let Ok(disks) = &result {
                    let rows = disk_rows(disks);
                    {
                        let mut current = lock_state(&state);
                        current.disks = disks.clone();
                        current.selected = None;
                    }
                    ui(weak.clone(), move |w| {
                        w.set_disks(ModelRc::new(VecModel::from(rows)));
                        w.set_selected_disk(-1);
                        w.set_reuse_ventoy(false);
                    });
                }
                ui(weak.clone(), |window| window.set_usb_detecting(false));
                finish(
                    weak,
                    result.map(|d| format!("{} unidad(es) USB detectada(s)", d.len())),
                    state,
                );
            });
        });
    }

    {
        let state = state.clone();
        let weak = window.as_weak();
        window.on_select_disk(move |number| {
            let reuse = {
                let mut state = lock_state(&state);
                state.selected = Some(number as u32);
                state
                    .disks
                    .iter()
                    .find(|disk| disk.number == number as u32)
                    .and_then(|disk| ventoy_data_location(disk).ok())
                    .flatten()
                    .is_some()
            };
            if let Some(w) = weak.upgrade() {
                w.set_selected_disk(number);
                w.set_reuse_ventoy(reuse);
                w.set_preparation_complete(false);
            }
        });
    }
    {
        let weak = window.as_weak();
        let state = state.clone();
        window.on_request_prepare(move |force_reinstall| {
            let Some(window) = weak.upgrade() else { return };
            let mut state = lock_state(&state);
            if state.iso.is_none() || state.local_iso.is_none() {
                window.set_status_text("Descarga o carga una ISO antes de preparar el USB".into());
                return;
            }
            let Some(number) = state.selected else { return };
            let Some(disk) = state.disks.iter().find(|d| d.number == number) else {
                return;
            };
            let reuse = match ventoy_data_location(disk) {
                Ok(letter) => letter.is_some(),
                Err(error) => {
                    window.set_status_text(error.into());
                    return;
                }
            };
            if force_reinstall && !reuse {
                window.set_status_text(
                    "No se detecta Ventoy en esta unidad. Actualiza la lista antes de prepararla."
                        .into(),
                );
                return;
            }
            window.set_selected_disk(number as i32);
            window.set_reuse_ventoy(reuse);
            window.set_confirm_reinstall(force_reinstall);
            window.set_confirm_disk(disk_label(disk).into());
            state.confirming = Some(number);
            window.set_confirm_visible(true);
        });
    }
    {
        let weak = window.as_weak();
        let state = state.clone();
        window.on_cancel_prepare(move || {
            lock_state(&state).confirming = None;
            if let Some(w) = weak.upgrade() {
                w.set_confirm_visible(false);
            }
        });
    }
    {
        let weak = window.as_weak();
        let state = state.clone();
        window.on_confirm_prepare(move || {
            let Some(window) = weak.upgrade() else { return };
            window.set_confirm_visible(false);
            let confirmed = lock_state(&state).confirming.take();
            if !begin(&state, &window) {
                return;
            }
            window.set_preparing(true);
            let gpt = window.get_gpt();
            let ntfs = window.get_ntfs();
            let reuse_expected = window.get_reuse_ventoy();
            let force_reinstall = window.get_confirm_reinstall();
            let snapshot = {
                let state = lock_state(&state);
                // Se prepara exactamente el disco que mostraba la confirmación.
                let disk = match confirmed {
                    Some(number) if state.selected == Some(number) => state
                        .disks
                        .iter()
                        .find(|d| d.number == number)
                        .cloned()
                        .ok_or("El USB confirmado ya no está en la lista. Actualízala y vuelve a elegirlo."),
                    _ => Err("La selección del USB cambió después de la confirmación. No se ha modificado ninguna unidad; vuelve a pulsar Preparar."),
                };
                (disk, state.iso.clone(), state.local_iso.clone())
            };
            let weak = weak.clone();
            let state = state.clone();
            thread::spawn(move || {
                let _guard = PanicGuard::new(&weak, &state, None);
                let result = match snapshot {
                    (Err(error), _, _) => Err(error.to_owned()),
                    (Ok(disk), Some(iso), Some(path)) => prepare(
                        disk,
                        iso,
                        path,
                        gpt,
                        ntfs,
                        reuse_expected,
                        force_reinstall,
                        weak.clone(),
                    ),
                    _ => Err("Descarga o carga una ISO y selecciona un USB".into()),
                };
                let success = result.as_ref().ok().cloned();
                if success.is_some() {
                    match disks() {
                        Ok(updated) => {
                            let rows = disk_rows(&updated);
                            let mut current = lock_state(&state);
                            current.disks = updated;
                            current.selected = None;
                            ui(weak.clone(), move |window| {
                                window.set_disks(ModelRc::new(VecModel::from(rows)));
                                window.set_selected_disk(-1);
                                window.set_reuse_ventoy(false);
                            });
                        }
                        Err(error) => {
                            log_event(format!(
                                "ERROR etapa=actualizar_usb_tras_copia detalle={error}"
                            ));
                            let mut current = lock_state(&state);
                            current.disks.clear();
                            current.selected = None;
                            ui(weak.clone(), move |window| {
                                window
                                    .set_disks(ModelRc::new(VecModel::from(Vec::<DiskRow>::new())));
                                window.set_selected_disk(-1);
                                window.set_reuse_ventoy(false);
                            });
                        }
                    }
                }
                finish(weak.clone(), result, state);
                if let Some(detail) = success {
                    ui(weak, move |window| {
                        window.set_progress(1.0);
                        window.set_logs_visible(false);
                        if let Some(backdrop) = blurred_backdrop(&window) {
                            window.set_success_backdrop(backdrop);
                        }
                        window.set_success_detail(detail.into());
                        window.set_restart_message("".into());
                        window.set_restart_pending(false);
                        window.set_preparation_complete(true);
                        window.set_secure_boot_status(-2);
                        window.set_success_visible(true);
                        let weak = window.as_weak();
                        thread::spawn(move || {
                            let detected = firmware_security_status();
                            log_event(format!(
                                "INFO etapa=seguridad_firmware_tras_preparar tpm={:?} secure_boot={:?}",
                                detected.tpm, detected.secure_boot
                            ));
                            ui(weak, move |window| {
                                window.set_tpm_status(detected.tpm.ui_value());
                                window.set_secure_boot_status(detected.secure_boot.ui_value());
                            });
                        });
                    });
                }
            });
        });
    }

    {
        let weak = window.as_weak();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(window) = weak.upgrade() {
                window.invoke_refresh_usb();
            }
        });
    }
    let result = window.run();
    log_event(format!(
        "INFO evento=cierre resultado={:?}",
        result.as_ref().err()
    ));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn alternative_link_is_read_from_release_notes() {
        let plain = "https://drive.google.com/file/d/example/view?usp=sharing";
        assert_eq!(alternative_link_from_notes(plain).unwrap().as_str(), plain);
        let markdown = "Descarga: [ISO](https://example.org/new.iso?x=1&y=2).";
        assert_eq!(
            alternative_link_from_notes(markdown).unwrap().as_str(),
            "https://example.org/new.iso?x=1&y=2"
        );
        assert!(alternative_link_from_notes("Solo http://example.org/file.iso").is_err());
        assert!(alternative_link_from_notes("Sin enlace").is_err());
        assert!(
            alternative_link_from_notes("https://example.org/a https://example.org/b").is_err()
        );
    }

    #[test]
    fn iso_size_uses_binary_gib_like_windows_properties() {
        assert_eq!(human(9_311_354_880), "8,67 GiB");
        assert_eq!(human(1_048_576), "1,0 MiB");
    }

    #[test]
    fn mirror_choices_only_include_available_named_servers() {
        let html = r#"<li id="outside"></li><ul id="mirrorList">
            <li id="autoselect"></li><li id="netix"></li>
            <li id="master"></li><li id="netix"></li>
            <li id="invalid.example"></li></ul>"#;
        assert_eq!(
            parse_mirror_choices(html),
            vec!["netix".to_owned(), "master".to_owned()]
        );
    }

    #[test]
    fn ranged_download_resumes_a_fragment_and_rejects_ignored_ranges() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for request_number in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let length = stream.read(&mut buffer).unwrap();
                    assert!(length > 0);
                    request.extend_from_slice(&buffer[..length]);
                }
                let request = String::from_utf8(request).unwrap();
                if request_number == 0 {
                    assert!(request.contains("Range: bytes=16-127"));
                    write!(
                        stream,
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 16-127/256\r\nContent-Length: 112\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    stream.write_all(&(16u8..64).collect::<Vec<_>>()).unwrap();
                } else if request_number == 1 {
                    assert!(request.contains("Range: bytes=64-127"));
                    write!(
                        stream,
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 64-127/256\r\nContent-Length: 64\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    stream.write_all(&(64u8..128).collect::<Vec<_>>()).unwrap();
                } else {
                    assert!(request.contains("Range: bytes=16-127"));
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: 256\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                }
            }
        });
        let dir = std::env::temp_dir().join(format!(
            "winslim-range-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let url = format!("http://{address}/test.iso");
        let progress = AtomicU64::new(0);
        let abort = AtomicBool::new(false);
        let state = AtomicU8::new(DOWNLOAD_RUNNING);
        let good = dir.join("good.segment");
        download_range(&url, &good, 16, 127, 256, &progress, &abort, &state).unwrap();
        assert_eq!(fs::read(good).unwrap(), (16u8..128).collect::<Vec<_>>());
        assert_eq!(progress.load(Ordering::Acquire), 112);
        let bad = dir.join("bad.segment");
        assert!(download_range(&url, &bad, 16, 127, 256, &progress, &abort, &state).is_err());
        assert!(!bad.exists());
        server.join().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn usb_action_requires_explicit_reinstall_and_matching_ventoy_state() {
        assert_eq!(
            usb_action(true, true, false).unwrap(),
            UsbAction::CopyExisting
        );
        assert_eq!(usb_action(true, true, true).unwrap(), UsbAction::Reinstall);
        assert_eq!(
            usb_action(false, false, false).unwrap(),
            UsbAction::InstallFresh
        );
        assert!(usb_action(true, false, false).is_err());
        assert!(usb_action(false, true, false).is_err());
        assert!(usb_action(false, false, true).is_err());
    }

    #[test]
    fn new_ventoy_config_uses_winslim_graphical_theme() {
        let bytes = winslim_ventoy_config(None).unwrap().unwrap();
        let config: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(config["theme"]["file"], WINSLIM_THEME_PATH);
        assert_eq!(config["theme"]["display_mode"], "GUI");
        assert_eq!(config["theme"]["ventoy_left"], "40%");
        assert_eq!(config["theme"]["ventoy_top"], "88%");
        assert_eq!(config["theme"]["ventoy_color"], "#000000");
        assert_eq!(config["control"][0]["VTOY_DEFAULT_MENU_MODE"], "0");
        assert!(config["control"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["VTOY_MENU_LANGUAGE"] == "es_ES"));
    }

    #[test]
    fn ventoy_config_preserves_other_settings_and_existing_custom_theme() {
        let managed = br#"{"control":[{"VTOY_DEFAULT_MENU_MODE":"1"}],"menu_alias":[{"image":"/example.iso","alias":"Example"}],"theme":{"display_mode":"CLI"}}"#;
        let bytes = winslim_ventoy_config(Some(managed)).unwrap().unwrap();
        let config: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(config["control"][0]["VTOY_DEFAULT_MENU_MODE"], "1");
        assert!(config["control"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["VTOY_MENU_LANGUAGE"] == "es_ES"));
        assert_eq!(config["menu_alias"][0]["alias"], "Example");
        assert_eq!(config["theme"]["file"], WINSLIM_THEME_PATH);

        let explicit_language = br#"{"control":[{"VTOY_MENU_LANGUAGE":"en_US"}]}"#;
        let bytes = winslim_ventoy_config(Some(explicit_language))
            .unwrap()
            .unwrap();
        let config: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(config["control"][0]["VTOY_MENU_LANGUAGE"], "en_US");
        assert_eq!(config["control"].as_array().unwrap().len(), 1);

        let custom = br#"{"theme":{"file":"/ventoy/my-theme/theme.txt","display_mode":"GUI"}}"#;
        assert!(winslim_ventoy_config(Some(custom)).unwrap().is_none());
    }

    #[test]
    fn ventoy_theme_installs_sized_icon_and_boot_files() {
        let root = std::env::temp_dir().join(format!(
            "winslim-ventoy-theme-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        assert!(install_winslim_ventoy_theme(&root).unwrap());
        let theme = root.join("ventoy").join("winslim-theme");
        assert_eq!(
            fs::read(theme.join("icon.png")).unwrap(),
            include_bytes!("../assets/ventoy-theme/icon.png")
        );
        let icon = image::load_from_memory(&fs::read(theme.join("icon.png")).unwrap()).unwrap();
        assert_eq!((icon.width(), icon.height()), (50, 50));
        assert!(fs::read_to_string(theme.join("theme.txt"))
            .unwrap()
            .contains("file = \"icon.png\""));
        assert!(fs::read_to_string(theme.join("theme.txt"))
            .unwrap()
            .contains("[Enter] Entrar"));
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("ventoy").join("ventoy.json")).unwrap())
                .unwrap();
        assert_eq!(config["theme"]["file"], WINSLIM_THEME_PATH);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_download_resumes_and_checks_hash() {
        let data: Vec<u8> = (0..(256 * 1024))
            .map(|index| ((index * 37 + 11) % 251) as u8)
            .collect();
        let halfway = data.len() / 2;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server_data = data.clone();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            for request_number in 0..4 {
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "faltó una petición HTTP de prueba"
                            );
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("servidor de prueba: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&buffer[..count]);
                }
                let request = String::from_utf8(request).unwrap();
                if request_number == 3 {
                    assert!(request.contains(&format!("Range: bytes={halfway}-")));
                    write!(
                        stream,
                        "HTTP/1.1 206 Partial Content\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                        server_data.len() - halfway,
                        halfway,
                        server_data.len() - 1,
                        server_data.len()
                    )
                    .unwrap();
                    stream.write_all(&server_data[halfway..]).unwrap();
                } else {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        server_data.len()
                    )
                    .unwrap();
                    if request_number == 1 {
                        stream.write_all(&server_data[..halfway]).unwrap();
                    }
                }
            }
        });
        let dir = std::env::temp_dir().join(format!(
            "winslim-download-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let iso = Iso {
            name: "test.iso".into(),
            url: format!("http://{address}/test.iso"),
            size: data.len() as u64,
            md5: Some(format!("{:x}", md5::compute(&data))),
            published: 0,
        };
        let download_state = AtomicU8::new(DOWNLOAD_RUNNING);
        let result = download_iso_to(&iso, slint::Weak::default(), &dir, &download_state).unwrap();
        server.join().unwrap();
        assert_eq!(fs::read(result).unwrap(), data);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancelling_download_removes_partial_file() {
        let data = vec![42u8; 256 * 1024];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            for request_number in 0..2 {
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "faltó una petición HTTP de prueba"
                            );
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("servidor de prueba: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&buffer[..count]);
                }
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    data.len()
                )
                .unwrap();
                if request_number == 1 {
                    stream.write_all(&data[..64 * 1024]).unwrap();
                    started_tx.send(()).unwrap();
                    thread::sleep(Duration::from_millis(300));
                }
            }
        });
        let dir = std::env::temp_dir().join(format!(
            "winslim-cancel-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let iso = Iso {
            name: "cancel.iso".into(),
            url: format!("http://{address}/cancel.iso"),
            size: 256 * 1024,
            md5: Some(format!("{:x}", md5::compute(vec![42u8; 256 * 1024]))),
            published: 0,
        };
        let download_state = Arc::new(AtomicU8::new(DOWNLOAD_RUNNING));
        let worker_state = download_state.clone();
        let worker_dir = dir.clone();
        let worker = thread::spawn(move || {
            download_iso_to(&iso, slint::Weak::default(), &worker_dir, &worker_state)
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        download_state
            .compare_exchange(
                DOWNLOAD_RUNNING,
                DOWNLOAD_CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .unwrap();
        assert_eq!(
            worker.join().unwrap().unwrap_err(),
            DOWNLOAD_CANCELLED_MESSAGE
        );
        server.join().unwrap();
        assert!(!dir.join("cancel.iso.part").exists());
        assert!(!dir.join("cancel.iso").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn streamed_copy_crosses_buffer_boundary_and_verifies_destination() {
        let dir = std::env::temp_dir().join(format!(
            "winslim-copy-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let source = dir.join("source.iso");
        let destination = dir.join("destination.iso");
        let data: Vec<u8> = (0usize..(16 * 1024 * 1024 + 123))
            .map(|index| (index.wrapping_mul(37) % 251) as u8)
            .collect();
        fs::write(&source, &data).unwrap();
        let mut finished = false;
        copy_and_verify(
            &source,
            &destination,
            data.len() as u64,
            |copied, verifying| {
                if verifying && copied == data.len() as u64 {
                    finished = true;
                }
            },
        )
        .unwrap();
        assert!(finished);
        assert_eq!(fs::read(&destination).unwrap(), data);
        assert!(copy_and_verify(&source, &destination, data.len() as u64, |_, _| {}).is_err());
        assert_eq!(fs::read(&destination).unwrap(), data);
        let existing = dir.join("existing.iso");
        fs::write(&existing, b"original").unwrap();
        assert!(publish_iso(&destination, &existing).is_err());
        assert_eq!(fs::read(&existing).unwrap(), b"original");
        let published = dir.join("published.iso");
        publish_iso(&destination, &published).unwrap();
        assert_eq!(fs::read(&published).unwrap(), data);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reader_finishing_before_writer_returns_buffer_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!(
            "winslim-eof-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let source = dir.join("source.iso");
        let destination = dir.join("destination.iso");
        let data = vec![0x5a; 1024];
        fs::write(&source, &data).unwrap();
        copy_and_verify(&source, &destination, data.len() as u64, |_, verifying| {
            if !verifying {
                thread::sleep(Duration::from_millis(50));
            }
        })
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), data);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn ventoy_is_reused_only_with_verified_data_and_boot_partitions() {
        let mut disk = Disk {
            number: 9,
            device_path: None,
            friendly_name: "USB".into(),
            serial_number: Some("ABC".into()),
            size: 32_000_000_000,
            bus_type: "USB".into(),
            is_boot: false,
            is_system: false,
            is_read_only: false,
            volumes: vec![
                Volume {
                    letter: Some("M".into()),
                    mount_path: None,
                    device_path: None,
                    label: Some("Ventoy".into()),
                    file_system: Some("exFAT".into()),
                    size: 31_000_000_000,
                    partition_number: 1,
                },
                Volume {
                    letter: None,
                    mount_path: None,
                    device_path: None,
                    label: Some("VTOYEFI".into()),
                    file_system: Some("FAT".into()),
                    size: 32 * 1024 * 1024,
                    partition_number: 2,
                },
            ],
        };
        assert_eq!(ventoy_data_location(&disk).unwrap().as_deref(), Some("M"));
        disk.volumes[0].label = Some("WinSlim USB".into());
        assert_eq!(ventoy_data_location(&disk).unwrap().as_deref(), Some("M"));
        disk.volumes[0].file_system = Some("FAT32".into());
        disk.volumes[1].letter = Some("\0".into());
        assert_eq!(ventoy_data_location(&disk).unwrap().as_deref(), Some("M"));
        assert_eq!(disk_label(&disk).matches("M:").count(), 1);
        disk.volumes[1].label = None;
        assert!(ventoy_data_location(&disk).is_err());
        disk.volumes.clear();
        assert!(ventoy_data_location(&disk).unwrap().is_none());
    }

    #[test]
    fn latest_iso_uses_publication_date_and_ignores_other_files() {
        let xml = r#"<rss xmlns:media="http://video.search.yahoo.com/mrss/"><channel>
          <item><title>/WinSlim11_old.iso</title><link>https://sourceforge.net/projects/winslim11-isos/files/WinSlim11_old.iso/download</link><pubDate>Wed, 23 Sep 2026 00:09:08 GMT</pubDate><media:content filesize="9311354880"/></item>
          <item><title>/WinSlim11_new.iso</title><link>https://sourceforge.net/projects/winslim11-isos/files/WinSlim11_new.iso/download</link><pubDate>Thu, 24 Sep 2026 00:09:08 GMT</pubDate><media:content filesize="9311354880"/></item>
          <item><title>/other.txt</title><link>https://sourceforge.net/projects/winslim11-isos/files/other.txt/download</link><pubDate>Fri, 25 Sep 2026 00:09:08 GMT</pubDate><media:content filesize="9311354880"/></item>
        </channel></rss>"#;
        assert_eq!(parse_latest(xml).unwrap().name, "WinSlim11_new.iso");
    }

    #[test]
    fn latest_iso_ignores_subfolders_and_requires_md5_algorithm() {
        let xml = r#"<rss xmlns:media="http://video.search.yahoo.com/mrss/"><channel>
          <item><title>/WinSlim11_root.iso</title><link>https://sourceforge.net/projects/winslim11-isos/files/WinSlim11_root.iso/download</link><pubDate>Wed, 23 Sep 2026 00:09:08 UT</pubDate><media:content filesize="9311354880"><media:hash algo="sha1">0123456789abcdef0123456789abcdef01234567</media:hash><media:hash algo="md5">4B7EE2A61AEC7E5BB0C1ACC19FC3A80D</media:hash></media:content></item>
          <item><title>/.rsync-partial/WinSlim11_partial.iso</title><link>https://sourceforge.net/projects/winslim11-isos/files/.rsync-partial/WinSlim11_partial.iso/download</link><pubDate>Thu, 24 Sep 2026 00:09:08 UT</pubDate><media:content filesize="9311354880"/></item>
        </channel></rss>"#;
        let iso = parse_latest(xml).unwrap();
        assert_eq!(iso.name, "WinSlim11_root.iso");
        assert_eq!(iso.md5.as_deref(), Some("4B7EE2A61AEC7E5BB0C1ACC19FC3A80D"));
        let sha1_only = xml.replace(
            r#"<media:hash algo="md5">4B7EE2A61AEC7E5BB0C1ACC19FC3A80D</media:hash>"#,
            "",
        );
        assert_eq!(parse_latest(&sha1_only).unwrap().md5, None);
    }

    #[test]
    fn uncached_read_back_hashes_whole_file() {
        let dir = std::env::temp_dir().join(format!(
            "winslim-uncached-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("copy.iso");
        // Tamaño no múltiplo del sector para comprobar el último bloque parcial.
        let data: Vec<u8> = (0usize..(CHUNK_SIZE * 2 + 4097))
            .map(|index| (index.wrapping_mul(31) % 253) as u8)
            .collect();
        fs::write(&path, &data).unwrap();
        let mut last = 0;
        let mut report = |read: u64| last = read;
        let (length, digest) = read_back_md5(&path, true, &mut report).unwrap();
        assert_eq!(length, data.len() as u64);
        assert_eq!(last, data.len() as u64);
        assert_eq!(digest, md5::compute(&data));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_usb_copies_are_removed_without_touching_other_files() {
        let dir = std::env::temp_dir().join(format!(
            "winslim-stale-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&dir).unwrap();
        let stale = usb_copy_temp_path(&dir).unwrap();
        fs::write(&stale, b"copia interrumpida").unwrap();
        let kept = [
            dir.join("WinSlim11.iso"),
            dir.join(".winslim-copy-notas.txt"),
            dir.join("otra.part"),
        ];
        for path in &kept {
            fs::write(path, b"conservar").unwrap();
        }
        remove_stale_usb_copies(&dir);
        assert!(!stale.exists());
        assert!(kept.iter().all(|path| path.exists()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn md5_mismatch_discards_the_downloaded_file() {
        let data = vec![7u8; 128 * 1024];
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server_data = data.clone();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            // Petición 0: resolver el enlace; petición 1: la descarga completa.
            for request_number in 0..2 {
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "faltó una petición HTTP de prueba"
                            );
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("servidor de prueba: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&buffer[..count]);
                }
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    server_data.len()
                )
                .unwrap();
                if request_number == 1 {
                    stream.write_all(&server_data).unwrap();
                }
            }
        });
        let dir = std::env::temp_dir().join(format!(
            "winslim-md5-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let iso = Iso {
            name: "mismatch.iso".into(),
            url: format!("http://{address}/mismatch.iso"),
            size: data.len() as u64,
            md5: Some("0".repeat(32)),
            published: 0,
        };
        let download_state = AtomicU8::new(DOWNLOAD_RUNNING);
        let error =
            download_iso_to(&iso, slint::Weak::default(), &dir, &download_state).unwrap_err();
        server.join().unwrap();
        assert!(error.contains("MD5"), "{error}");
        assert!(!dir.join("mismatch.iso.part").exists());
        assert!(!dir.join("mismatch.iso").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn changed_usb_identity_is_rejected() {
        let disk = Disk {
            number: 2,
            device_path: None,
            friendly_name: "USB".into(),
            serial_number: Some("A".into()),
            size: 16_000_000_000,
            bus_type: "USB".into(),
            is_boot: false,
            is_system: false,
            is_read_only: false,
            volumes: vec![],
        };
        let mut other = disk.clone();
        assert!(same_disk(&disk, &other));
        other.serial_number = Some("B".into());
        assert!(!same_disk(&disk, &other));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn free_space_check_uses_windows_api() {
        assert!(fs_free_bytes(&std::env::temp_dir()).unwrap() > 0);
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn local_iso_on_target_usb_is_rejected() {
        let disk = Disk {
            number: 5,
            device_path: None,
            friendly_name: "USB".into(),
            serial_number: None,
            size: 32_000_000_000,
            bus_type: "USB".into(),
            is_boot: false,
            is_system: false,
            is_read_only: false,
            volumes: vec![Volume {
                letter: Some("E".into()),
                mount_path: None,
                device_path: None,
                label: None,
                file_system: Some("exFAT".into()),
                size: 32_000_000_000,
                partition_number: 1,
            }],
        };
        assert!(iso_on_selected_disk(Path::new("e:\\WinSlim.iso"), &disk));
        assert!(!iso_on_selected_disk(Path::new("C:\\WinSlim.iso"), &disk));
    }

    #[test]
    #[ignore = "requires SourceForge network access"]
    fn sourceforge_rss_is_readable() {
        let iso = fetch_latest().unwrap();
        assert!(iso.name.starts_with("WinSlim"));
        assert!(iso.size > 1_000_000_000);
        let url = resolve_download(&iso).unwrap();
        let response = ureq::head(&url).call().unwrap();
        assert_eq!(
            response
                .header("Content-Length")
                .unwrap()
                .parse::<u64>()
                .unwrap(),
            iso.size
        );
    }

    #[test]
    #[ignore = "requires SourceForge network access"]
    fn sourceforge_supports_resume() {
        let iso = fetch_latest().unwrap();
        let url = resolve_download(&iso).unwrap();
        let response = ureq::get(&url)
            .set("Range", "bytes=1024-2047")
            .set("User-Agent", "WinSlimUsbCreator/0.1")
            .call()
            .unwrap();
        assert_eq!(response.status(), 206);
        assert!(response
            .header("Content-Range")
            .unwrap()
            .starts_with("bytes 1024-2047/"));
    }

    #[test]
    fn bundled_ventoy_archive_is_valid() {
        let exe = ventoy_exe().unwrap();
        assert!(exe.exists());
        assert!(exe.parent().unwrap().join("ventoy").exists());
    }

    #[test]
    #[ignore = "depends on Windows disk inventory"]
    fn usb_enumeration_is_readable() {
        let found = disks().unwrap();
        assert!(found.iter().all(|disk| disk.bus_type == "USB"));
    }
}
