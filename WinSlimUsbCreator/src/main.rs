#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();

use serde::Deserialize;
use sha2::{Digest, Sha256};
use slint::winit_030::winit::event::WindowEvent;
use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::{ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::{
    cell::Cell,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::windows::{ffi::OsStrExt, process::CommandExt},
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
    sync::{
        atomic::{AtomicU8, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, MoveFileExW, MoveFileW, SetVolumeLabelW, MOVEFILE_REPLACE_EXISTING,
    MOVEFILE_WRITE_THROUGH,
};
use windows_sys::Win32::{
    Foundation::RECT,
    Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
    },
    UI::WindowsAndMessaging::{GetClientRect, SystemParametersInfoW, SPI_GETWORKAREA},
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DOWNLOAD_IDLE: u8 = 0;
const DOWNLOAD_RUNNING: u8 = 1;
const DOWNLOAD_CANCELLED: u8 = 2;
const DOWNLOAD_COMMITTING: u8 = 3;
const DOWNLOAD_CANCELLED_MESSAGE: &str = "Descarga cancelada";

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

#[repr(C)]
struct OsVersionInfo {
    size: u32,
    major: u32,
    minor: u32,
    build: u32,
    platform: u32,
    service_pack: [u16; 128],
}

#[link(name = "ntdll")]
extern "system" {
    fn RtlGetVersion(info: *mut OsVersionInfo) -> i32;
}

fn windows_version() -> String {
    let mut info = OsVersionInfo {
        size: std::mem::size_of::<OsVersionInfo>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        service_pack: [0; 128],
    };
    if unsafe { RtlGetVersion(&mut info) } == 0 {
        format!("{}.{}.{}", info.major, info.minor, info.build)
    } else {
        "desconocida".into()
    }
}

const RSS: &str = "https://sourceforge.net/projects/winslim11-isos/rss?path=/";
const VENTOY_SHA256: &str = "d250e97a7595fdac4f97debc630d7a8da942319274a76cb32384596b659dbaeb";
const VENTOY_ARCHIVE: &[u8] = include_bytes!("../vendor/ventoy-1.1.17-windows.zip");

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
    label: Option<String>,
    file_system: Option<String>,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    partition_number: u32,
}

fn drive_letter(volume: &Volume) -> Option<char> {
    let text = volume.letter.as_deref()?;
    let mut chars = text.chars();
    let letter = chars.next()?;
    if letter.is_ascii_alphabetic() && chars.next().is_none() {
        Some(letter.to_ascii_uppercase())
    } else {
        None
    }
}

#[derive(Default)]
struct State {
    iso: Option<Iso>,
    local_iso: Option<PathBuf>,
    disks: Vec<Disk>,
    selected: Option<u32>,
    busy: bool,
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
    state.lock().unwrap().busy = false;
    log_event(match &result {
        Ok(message) => format!("INFO resultado=OK detalle={message}"),
        Err(message) => format!("ERROR resultado=FALLO detalle={message}"),
    });
    ui(weak, move |window| {
        window.set_busy(false);
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

fn log_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|base| {
        PathBuf::from(base)
            .join("WinSlimUsbCreator")
            .join("operations.log")
    })
}

fn log_event(message: String) {
    let Some(path) = log_path() else {
        return;
    };
    if fs::create_dir_all(path.parent().unwrap()).is_err() {
        return;
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(
            file,
            "{} {message}",
            httpdate::fmt_http_date(std::time::SystemTime::now())
        );
    }
}

fn read_log() -> String {
    let Some(path) = log_path() else {
        return "No se encontró LOCALAPPDATA".into();
    };
    match fs::read_to_string(path) {
        Ok(contents) => {
            let max = 500_000;
            if contents.len() <= max {
                contents
            } else {
                let mut start = contents.len() - max;
                while !contents.is_char_boundary(start) {
                    start += 1;
                }
                format!(
                    "[Se muestran los últimos 500 KB del registro]\n{}",
                    &contents[start..]
                )
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "El registro está vacío".into(),
        Err(e) => format!("No se pudo leer el registro: {e}"),
    }
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
    let summary = if error.raw_os_error() == Some(112) {
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
    let mut state = state.lock().unwrap();
    if state.busy {
        return false;
    }
    state.busy = true;
    window.set_busy(true);
    window.set_progress(0.0);
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
        let raw_name = get("title").unwrap_or("");
        let name = raw_name.rsplit('/').next().unwrap_or("");
        if !name.to_ascii_lowercase().starts_with("winslim")
            || !name.to_ascii_lowercase().ends_with(".iso")
            || name.contains(['/', '\\'])
        {
            continue;
        }
        let Some(url) = get("link") else { continue };
        if !url.starts_with("https://sourceforge.net/projects/winslim11-isos/files/")
            || !url.ends_with("/download")
        {
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
        let md5 = media
            .and_then(|n| {
                n.descendants()
                    .find(|n| n.is_element() && n.tag_name().name() == "hash")
            })
            .and_then(|n| n.text())
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

fn powershell(script: &str) -> Result<String, String> {
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("PowerShell: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn disks() -> Result<Vec<Disk>, String> {
    const SCRIPT: &str = r#"$ErrorActionPreference='Stop'; @(Get-Disk | Where-Object { $_.BusType -eq 'USB' -and -not $_.IsBoot -and -not $_.IsSystem } | ForEach-Object { $d=$_; $v=@(Get-Partition -DiskNumber $d.Number -ErrorAction SilentlyContinue | ForEach-Object { $p=$_; $vol=$p | Get-Volume -ErrorAction SilentlyContinue; $label=''; $fs=''; if ($vol) { $label=[string]$vol.FileSystemLabel; $fs=[string]$vol.FileSystem }; [pscustomobject]@{ Letter=[string]$p.DriveLetter; Label=$label; FileSystem=$fs; Size=[uint64]$p.Size; PartitionNumber=[uint32]$p.PartitionNumber } }); [pscustomobject]@{ Number=$d.Number; FriendlyName=[string]$d.FriendlyName; SerialNumber=[string]$d.SerialNumber; Size=[uint64]$d.Size; BusType=[string]$d.BusType; IsBoot=[bool]$d.IsBoot; IsSystem=[bool]$d.IsSystem; IsReadOnly=[bool]$d.IsReadOnly; Volumes=$v } }) | ConvertTo-Json -Depth 5 -Compress"#;
    let output = powershell(SCRIPT)?;
    if output.is_empty() || output == "null" {
        return Ok(Vec::new());
    }
    let value: serde_json::Value =
        serde_json::from_str(&output).map_err(|e| format!("Discos: {e}"))?;
    let values = if let Some(array) = value.as_array() {
        array.clone()
    } else {
        vec![value]
    };
    let found: Result<Vec<Disk>, String> = values
        .into_iter()
        .map(|value| serde_json::from_value(value).map_err(|e| e.to_string()))
        .collect();
    let found = found?;
    log_event(format!("INFO etapa=detectar_usb cantidad={}", found.len()));
    for disk in &found {
        log_event(format!(
            "DEBUG etapa=detectar_usb numero={} modelo={} bytes={} bus={} readonly={} volumenes={}",
            disk.number,
            disk.friendly_name,
            disk.size,
            disk.bus_type,
            disk.is_read_only,
            disk.volumes.len()
        ));
    }
    Ok(found)
}

fn disk_label(disk: &Disk) -> String {
    let letters = disk
        .volumes
        .iter()
        .filter_map(drive_letter)
        .map(|letter| format!("{letter}:"))
        .collect::<Vec<_>>()
        .join(", ");
    let letter_part = if letters.is_empty() {
        "sin letra".to_owned()
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
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1_000_000_000.0)
    } else {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
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

fn download_iso(
    iso: &Iso,
    weak: slint::Weak<MainWindow>,
    download_state: &AtomicU8,
) -> Result<PathBuf, String> {
    let home = std::env::var_os("USERPROFILE").ok_or("No se encontró el perfil de usuario")?;
    let dir = PathBuf::from(home).join("Downloads").join("WinSlim");
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
        match fs::remove_file(&part) {
            Ok(()) => log_event(format!(
                "INFO etapa=descargar_iso cancelada archivo_parcial_eliminado={}",
                part.display()
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(file_error("eliminar descarga cancelada", &part, error)),
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
    if final_path.exists() && verify_file(&final_path, iso)? {
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
        if verify_file(&part, iso)? {
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
        hash = md5::Context::new();
    }
    let mut initial_bytes = downloaded;
    let mut start = Instant::now();
    let mut last_update = Instant::now() - Duration::from_secs(2);
    let mut last_log = Instant::now();
    let mut attempts = 0;
    while downloaded < iso.size {
        check_download_cancellation(download_state)?;
        status(weak.clone(), "Seleccionando un espejo de SourceForge…");
        let direct_url = match resolve_download(iso) {
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
        let actual = format!("{:x}", hash.compute());
        if !actual.eq_ignore_ascii_case(expected) {
            log_event(format!(
                "ERROR etapa=verificar_iso md5_esperado={expected} md5_obtenido={actual}"
            ));
            return Err("La suma MD5 no coincide con la publicada por SourceForge".into());
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

fn verify_file(path: &Path, iso: &Iso) -> Result<bool, String> {
    if fs::metadata(path).map_err(|e| e.to_string())?.len() != iso.size {
        return Ok(false);
    }
    let Some(expected) = &iso.md5 else {
        return Ok(true);
    };
    let mut input = File::open(path).map_err(|e| e.to_string())?;
    let mut hash = md5::Context::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let len = input.read(&mut buffer).map_err(|e| e.to_string())?;
        if len == 0 {
            break;
        }
        hash.consume(&buffer[..len]);
    }
    Ok(format!("{:x}", hash.compute()).eq_ignore_ascii_case(expected))
}

fn format_duration(seconds: u64) -> String {
    if seconds >= 3600 {
        format!("{} h {} min", seconds / 3600, seconds % 3600 / 60)
    } else {
        format!("{} min {} s", seconds / 60, seconds % 60)
    }
}

fn ventoy_exe() -> Result<PathBuf, String> {
    log_event("INFO etapa=ventoy verificar_paquete=sha256 version=1.1.17".into());
    let mut hash = Sha256::new();
    hash.update(VENTOY_ARCHIVE);
    if format!("{:x}", hash.finalize()) != VENTOY_SHA256 {
        return Err("El paquete de Ventoy no pasó la verificación SHA-256".into());
    }
    let base = std::env::var_os("LOCALAPPDATA").ok_or("No se encontró LOCALAPPDATA")?;
    let dir = PathBuf::from(base)
        .join("WinSlimUsbCreator")
        .join("ventoy-1.1.17");
    let package = dir.join("ventoy-1.1.17");
    let exe = package.join("Ventoy2Disk_X64.exe");
    if exe.exists() && package.join("ventoy").join("ventoy.disk.img.xz").exists() {
        log_event(format!(
            "INFO etapa=ventoy paquete_extraido ruta={}",
            package.display()
        ));
        return Ok(exe);
    }
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(VENTOY_ARCHIVE)).map_err(|e| e.to_string())?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let Some(name) = entry.enclosed_name() else {
            return Err("Ruta no segura en el paquete de Ventoy".into());
        };
        let target = dir.join(name);
        if entry.is_dir() {
            fs::create_dir_all(&target).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut output = File::create(&target).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut output).map_err(|e| e.to_string())?;
    }
    let alternate = package.join("altexe").join("Ventoy2Disk_X64.exe");
    fs::copy(alternate, &exe).map_err(|e| format!("No se pudo preparar Ventoy x64: {e}"))?;
    log_event(format!(
        "INFO etapa=ventoy paquete_extraido ruta={}",
        package.display()
    ));
    Ok(exe)
}

fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn install_ventoy(exe: &Path, disk: &Disk, gpt: bool, ntfs: bool) -> Result<(), String> {
    let path = ps_quote(&exe.to_string_lossy());
    let install_dir = exe.parent().ok_or("Directorio de Ventoy no válido")?;
    let done = install_dir.join("cli_done.txt");
    let _ = fs::remove_file(&done);
    let mut args = format!(
        "VTOYCLI /I /PhyDrive:{} /FS:{}",
        disk.number,
        if ntfs { "NTFS" } else { "EXFAT" }
    );
    if gpt {
        args.push_str(" /GPT");
    }
    log_event(format!(
        "INFO etapa=instalar_ventoy ejecutable={} argumentos={args}",
        exe.display()
    ));
    let script = format!("$ErrorActionPreference='Stop'; $p=Start-Process -FilePath {path} -WorkingDirectory {} -ArgumentList {} -Verb RunAs -Wait -PassThru; if ($null -eq $p) {{ exit 1 }}; exit $p.ExitCode", ps_quote(&install_dir.to_string_lossy()), ps_quote(&args));
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("Elevación: {e}"))?;
    log_event(format!(
        "INFO etapa=instalar_ventoy proceso_auxiliar=powershell.exe exit_code={:?} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).trim()
    ));
    if let Ok(contents) = fs::read_to_string(install_dir.join("cli_log.txt")) {
        let max = 100_000;
        let mut start = contents.len().saturating_sub(max);
        while !contents.is_char_boundary(start) {
            start += 1;
        }
        log_event(format!(
            "DEBUG etapa=instalar_ventoy cli_log_inicio\n{}\ncli_log_fin",
            &contents[start..]
        ));
    }
    if !output.status.success() {
        return Err(format!(
            "Ventoy terminó con error o se canceló UAC (código {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let marker = fs::read_to_string(&done)
        .map_err(|_| "Ventoy no generó su marcador de finalización".to_owned())?;
    if marker.trim() != "0" {
        log_event(format!(
            "ERROR etapa=instalar_ventoy marcador={} log={}",
            marker.trim(),
            install_dir.join("cli_log.txt").display()
        ));
        return Err("Ventoy informó que la instalación falló; revisa cli_log.txt".into());
    }
    log_event(format!(
        "INFO etapa=instalar_ventoy resultado=OK marcador={} log={}",
        marker.trim(),
        install_dir.join("cli_log.txt").display()
    ));
    Ok(())
}

fn same_disk(a: &Disk, b: &Disk) -> bool {
    a.number == b.number
        && a.size == b.size
        && a.bus_type.eq_ignore_ascii_case("USB")
        && !b.is_boot
        && !b.is_system
        && !b.is_read_only
        && a.serial_number.as_deref().unwrap_or("").trim()
            == b.serial_number.as_deref().unwrap_or("").trim()
}

fn ventoy_data_letter(disk: &Disk) -> Result<Option<String>, String> {
    let efi = disk.volumes.iter().find(|volume| {
        volume
            .label
            .as_deref()
            .is_some_and(|label| label.eq_ignore_ascii_case("VTOYEFI"))
    });
    let data = disk.volumes.iter().find(|volume| {
        volume.partition_number == 1
            && drive_letter(volume).is_some()
            && volume.file_system.as_deref().is_some_and(|fs| {
                fs.eq_ignore_ascii_case("exFAT")
                    || fs.eq_ignore_ascii_case("NTFS")
                    || fs.eq_ignore_ascii_case("FAT32")
            })
    });
    if let (Some(efi), Some(data)) = (efi, data) {
        let valid_efi = efi.partition_number != data.partition_number
            && (8 * 1024 * 1024..=256 * 1024 * 1024).contains(&efi.size)
            && efi.file_system.as_deref().is_some_and(|fs| {
                fs.eq_ignore_ascii_case("FAT") || fs.eq_ignore_ascii_case("FAT32")
            });
        if valid_efi {
            return Ok(drive_letter(data).map(|letter| letter.to_string()));
        }
    }
    let looks_like_ventoy = efi.is_some()
        || disk.volumes.iter().any(|volume| {
            volume.label.as_deref().is_some_and(|label| {
                label.eq_ignore_ascii_case("Ventoy") || label.eq_ignore_ascii_case("WinSlim USB")
            })
        })
        || (data.is_some()
            && disk.volumes.iter().any(|volume| {
                volume.partition_number != 1
                    && (8 * 1024 * 1024..=256 * 1024 * 1024).contains(&volume.size)
            }));
    if looks_like_ventoy {
        Err("El USB parece tener Ventoy, pero no se pudo verificar su partición de arranque VTOYEFI. No se formateó; revisa la unidad.".into())
    } else {
        Ok(None)
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

fn set_usb_label(root: &Path) -> Result<(), String> {
    let mut root_wide = root.as_os_str().encode_wide().collect::<Vec<_>>();
    root_wide.push(0);
    let mut label_wide = "WinSlim USB".encode_utf16().collect::<Vec<_>>();
    label_wide.push(0);
    if unsafe { SetVolumeLabelW(root_wide.as_ptr(), label_wide.as_ptr()) } == 0 {
        return Err(file_error(
            "asignar etiqueta WinSlim USB",
            root,
            std::io::Error::last_os_error(),
        ));
    }
    log_event(format!(
        "INFO etapa=etiquetar_usb unidad={} etiqueta=WinSlim USB",
        root.display()
    ));
    Ok(())
}

fn publish_iso(from: &Path, to: &Path) -> Result<(), String> {
    let mut from_wide = from.as_os_str().encode_wide().collect::<Vec<_>>();
    from_wide.push(0);
    let mut to_wide = to.as_os_str().encode_wide().collect::<Vec<_>>();
    to_wide.push(0);
    // MoveFileW falla si el destino ya existe: no sobrescribe una ISO del usuario.
    if unsafe { MoveFileW(from_wide.as_ptr(), to_wide.as_ptr()) } == 0 {
        return Err(file_error(
            "finalizar copia USB",
            to,
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

fn iso_on_selected_disk(path: &Path, disk: &Disk) -> bool {
    let path_text = path.to_string_lossy();
    let bytes = path_text.as_bytes();
    if bytes.len() < 2 || bytes[1] != b':' {
        return false;
    }
    let letter = bytes[0].to_ascii_uppercase();
    disk.volumes
        .iter()
        .filter_map(drive_letter)
        .any(|volume| volume as u8 == letter)
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
        let mut from = pending.as_os_str().encode_wide().collect::<Vec<_>>();
        let mut to = config_path.as_os_str().encode_wide().collect::<Vec<_>>();
        from.push(0);
        to.push(0);
        if unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(file_error(
                "publicar configuración de Ventoy",
                &config_path,
                std::io::Error::last_os_error(),
            ));
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&pending);
    }
    result.map(|_| true)
}

fn prepare(
    disk: Disk,
    iso: Iso,
    local_iso: PathBuf,
    gpt: bool,
    ntfs: bool,
    reuse_expected: bool,
    force_reinstall: bool,
    weak: slint::Weak<MainWindow>,
) -> Result<String, String> {
    if !verify_file(&local_iso, &iso)? {
        return Err("La ISO local no coincide con SourceForge. Descárgala de nuevo.".into());
    }
    let current = disks()?
        .into_iter()
        .find(|d| d.number == disk.number)
        .ok_or("El USB seleccionado ya no está conectado")?;
    if !same_disk(&disk, &current) {
        return Err("El USB seleccionado cambió. Vuelve a elegirlo.".into());
    }
    let existing_letter = ventoy_data_letter(&current)?;
    let action = usb_action(reuse_expected, existing_letter.is_some(), force_reinstall)?;
    let reused = action == UsbAction::CopyExisting;
    if reused
        && iso.size > u32::MAX as u64
        && current.volumes.iter().any(|volume| {
            volume.partition_number == 1
                && volume
                    .file_system
                    .as_deref()
                    .is_some_and(|fs| fs.eq_ignore_ascii_case("FAT32"))
        })
    {
        return Err("Ventoy está instalado, pero su partición FAT32 no admite una ISO de más de 4 GB. No se ha formateado ni modificado el USB.".into());
    }
    if !reused && iso_on_selected_disk(&local_iso, &current) {
        return Err(
            "La ISO está guardada en el USB que se va a borrar. Muévela a otra unidad.".into(),
        );
    }
    if !reused && current.size < iso.size + 256 * 1024 * 1024 {
        return Err("El USB no tiene capacidad suficiente para la ISO y Ventoy".into());
    }
    log_event(format!(
        "PREPARE {} {} {} accion={action:?}",
        disk_label(&disk),
        if gpt { "GPT" } else { "MBR" },
        if ntfs { "NTFS" } else { "exFAT" }
    ));
    let letter = if reused {
        let letter = existing_letter.ok_or("No se encontró la partición de datos de Ventoy")?;
        log_event(format!(
            "INFO etapa=preparar_usb ventoy_existente disco={} unidad={}",
            disk.number, letter
        ));
        status(
            weak.clone(),
            "Ventoy ya está instalado; se conservarán sus datos…",
        );
        letter
    } else {
        status(weak.clone(), "Verificando el paquete oficial de Ventoy…");
        let exe = ventoy_exe()?;
        status(
            weak.clone(),
            "Instalando el cargador de arranque GRUB de Ventoy en la unidad USB…",
        );
        install_ventoy(&exe, &disk, gpt, ntfs)?;
        log_event(format!(
            "INFO etapa=preparar_usb particionado_y_formato_completados disco={}",
            disk.number
        ));
        status(weak.clone(), "Buscando la nueva partición de datos…");
        let mut target = None;
        for _ in 0..30 {
            thread::sleep(Duration::from_secs(2));
            if let Some(updated) = disks()?.into_iter().find(|d| d.number == disk.number) {
                if !same_disk(&disk, &updated) {
                    return Err("El dispositivo USB cambió durante la instalación".into());
                }
                target = ventoy_data_letter(&updated).ok().flatten();
                if target.is_some() {
                    break;
                }
            }
        }
        target.ok_or("Ventoy terminó, pero no se pudo verificar su partición de datos y VTOYEFI")?
    };
    let root = PathBuf::from(format!("{}:\\", letter.chars().next().unwrap()));
    if !reused {
        set_usb_label(&root)?;
    }
    let destination = if reused {
        unused_iso_path(&root, &iso.name)
    } else {
        root.join(&iso.name)
    };
    log_event(format!(
        "INFO etapa=copiar_iso origen={} destino={} bytes={}",
        local_iso.display(),
        destination.display(),
        iso.size
    ));
    let free = fs_free_bytes(&root)?;
    if free < iso.size {
        return Err("La nueva partición no tiene espacio suficiente para la ISO".into());
    }
    let temporary = root.join(format!(
        ".winslim-copy-{}-{}.part",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos()
    ));
    status(weak.clone(), "Copiando la ISO al USB…");
    if let Err(error) = copy_with_progress(&local_iso, &temporary, iso.size, weak.clone()) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = publish_iso(&temporary, &destination) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    status(weak.clone(), "Configurando el tema de arranque de WinSlim…");
    let theme_warning = match install_winslim_ventoy_theme(&root) {
        Ok(true) => {
            log_event(format!(
                "INFO etapa=configurar_ventoy tema=winslim archivo={}",
                root.join("ventoy").join("ventoy.json").display()
            ));
            ""
        }
        Ok(false) => {
            log_event("INFO etapa=configurar_ventoy tema=personalizado_conservado".to_string());
            ""
        }
        Err(error) => {
            log_event(format!("ERROR etapa=configurar_ventoy detalle={error}"));
            " · tema no aplicado (consulta Registro)"
        }
    };
    Ok(format!(
        "USB preparado: Disco {} · {} · {}: · {}{}",
        disk.number,
        disk.friendly_name,
        letter,
        destination.file_name().unwrap().to_string_lossy(),
        theme_warning
    ))
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

fn fs_free_bytes(root: &Path) -> Result<u64, String> {
    let mut wide = root.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    let mut free = 0u64;
    let success = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if success == 0 {
        let error = std::io::Error::last_os_error();
        return Err(file_error("comprobar espacio libre", root, error));
    }
    Ok(free)
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
    const CHUNK_SIZE: usize = 8 * 1024 * 1024;
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
    let mut copied_file = File::open(to).map_err(|e| file_error("abrir copia USB", to, e))?;
    let mut hash = md5::Context::new();
    let mut buffer = vec![0u8; CHUNK_SIZE];
    let mut verified = 0u64;
    loop {
        let len = copied_file
            .read(&mut buffer)
            .map_err(|e| file_error("verificar copia USB", to, e))?;
        if len == 0 {
            break;
        }
        hash.consume(&buffer[..len]);
        verified += len as u64;
        if last_update.elapsed() >= Duration::from_millis(250) {
            progress(verified, true);
            last_update = Instant::now();
        }
    }
    if verified != total || hash.compute() != source_hash.1 {
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

fn blurred_backdrop(window: &MainWindow) -> Option<slint::Image> {
    let hwnd = window.window().with_winit_window(|native| {
        match native.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as *mut std::ffi::c_void),
            _ => None,
        }
    })??;
    let mut rect = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut rect) } == 0 {
        return None;
    }
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 || width > 4096 || height > 4096 {
        return None;
    }
    let source_dc = unsafe { GetDC(hwnd) };
    if source_dc.is_null() {
        return None;
    }
    let memory_dc = unsafe { CreateCompatibleDC(source_dc) };
    let bitmap = unsafe { CreateCompatibleBitmap(source_dc, width, height) };
    if memory_dc.is_null() || bitmap.is_null() {
        if !bitmap.is_null() {
            unsafe { DeleteObject(bitmap) };
        }
        if !memory_dc.is_null() {
            unsafe { DeleteDC(memory_dc) };
        }
        unsafe { ReleaseDC(hwnd, source_dc) };
        return None;
    }
    let previous = unsafe { SelectObject(memory_dc, bitmap) };
    let copied = unsafe { BitBlt(memory_dc, 0, 0, width, height, source_dc, 0, 0, SRCCOPY) };
    unsafe { SelectObject(memory_dc, previous) };
    let mut info = BITMAPINFO::default();
    info.bmiHeader.biSize = std::mem::size_of_val(&info.bmiHeader) as u32;
    info.bmiHeader.biWidth = width;
    info.bmiHeader.biHeight = -height;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB;
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    let read = unsafe {
        GetDIBits(
            memory_dc,
            bitmap,
            0,
            height as u32,
            pixels.as_mut_ptr().cast(),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    unsafe {
        DeleteObject(bitmap);
        DeleteDC(memory_dc);
        ReleaseDC(hwnd, source_dc);
    }
    if copied == 0 || read != height {
        return None;
    }
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        pixel[3] = 255;
    }
    let frame = image::RgbaImage::from_raw(width as u32, height as u32, pixels)?;
    let small = image::imageops::resize(
        &frame,
        (width as u32 / 4).max(1),
        (height as u32 / 4).max(1),
        image::imageops::FilterType::Triangle,
    );
    let blurred = image::imageops::blur(&small, 4.0);
    let mut output =
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(blurred.width(), blurred.height());
    for (target, source) in output.make_mut_slice().iter_mut().zip(blurred.pixels()) {
        *target = slint::Rgba8Pixel {
            r: source[0],
            g: source[1],
            b: source[2],
            a: 255,
        };
    }
    Some(slint::Image::from_rgba8(output))
}

fn size_initial_window(window: &MainWindow) {
    let mut work_area = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let found = unsafe {
        SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut work_area as *mut RECT).cast(), 0)
    };
    if found == 0 {
        return;
    }
    let scale = window.window().scale_factor().max(1.0);
    let available_width = (work_area.right - work_area.left) as f32 / scale - 24.0;
    let available_height = (work_area.bottom - work_area.top) as f32 / scale - 48.0;
    let width = 1200.0_f32.min(available_width).max(680.0);
    let height = 985.0_f32.min(available_height).min(width * 0.84).max(530.0);
    window
        .window()
        .set_size(slint::LogicalSize::new(width, height));
}

fn main() -> Result<(), slint::PlatformError> {
    std::panic::set_hook(Box::new(|info| log_event(format!("ERROR panic={info}"))));
    log_event(format!(
        "INFO evento=inicio version={} windows={} arquitectura={}",
        env!("CARGO_PKG_VERSION"),
        windows_version(),
        std::env::consts::ARCH
    ));
    let window = MainWindow::new()?;
    size_initial_window(&window);
    // En Windows, softbuffer puede reutilizar su caché de daños aunque el área
    // cliente se haya vaciado al minimizar. Invalidar el fondo obliga a Slint
    // a pintar de nuevo toda la ventana al restaurarla o maximizarla.
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
                let shutdown = PathBuf::from(
                    std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()),
                )
                .join("System32")
                .join("shutdown.exe");
                let result = Command::new(shutdown)
                    .args(["/r", "/o", "/t", "0"])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output();
                let error = match result {
                    Ok(output) if output.status.success() => return,
                    Ok(output) => {
                        let detail = if output.stderr.is_empty() {
                            &output.stdout
                        } else {
                            &output.stderr
                        };
                        format!(
                            "Windows no pudo iniciar el reinicio (código {:?}): {}",
                            output.status.code(),
                            String::from_utf8_lossy(detail).trim()
                        )
                    }
                    Err(error) => format!("Windows no pudo iniciar el reinicio: {error}"),
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
        log_timer.start(TimerMode::Repeated, Duration::from_millis(700), move || {
            if let Some(w) = weak.upgrade() {
                if w.get_logs_visible() {
                    let contents = read_log();
                    if w.get_log_text().as_str() != contents {
                        w.set_log_text(contents.into());
                    }
                }
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
                if let Err(e) = Command::new("explorer.exe").arg(dir).spawn() {
                    log_event(format!("ERROR etapa=registro abrir_carpeta={e}"));
                }
            }
        }
    });
    {
        let weak = window.as_weak();
        window.on_clear_logs(move || {
            if let Some(path) = log_path() {
                if let Err(e) = fs::write(&path, "") {
                    log_event(format!("ERROR etapa=registro limpiar={e}"));
                } else {
                    log_event("INFO etapa=registro limpiado_por_usuario".into());
                }
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
                .ok_or_else(|| "No se encontró LOCALAPPDATA".to_owned())
                .and_then(|source| fs::copy(source, &target).map_err(|e| e.to_string()));
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
                let result = fetch_latest();
                if let Ok(iso) = &result {
                    let mut current = state.lock().unwrap();
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
                    let detail = format!(
                        "{} · {}",
                        human(iso.size),
                        if retained.is_some() {
                            "descargada de SourceForge"
                        } else {
                            "publicado en SourceForge"
                        }
                    );
                    let local_file = retained
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "Sin descargar".into());
                    ui(weak.clone(), move |w| {
                        w.set_iso_name(name.into());
                        w.set_iso_detail(detail.into());
                        w.set_iso_stage(if retained.is_some() { 3 } else { 1 });
                        w.set_local_file(local_file.into());
                        if retained.is_none() {
                            w.set_selected_disk(-1);
                            w.set_reuse_ventoy(false);
                        }
                    });
                }
                finish(
                    weak,
                    result.map(|_| "ISO más reciente encontrada".into()),
                    state,
                );
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
                    Err(error) => Err(error),
                };
                if let Ok((iso, path)) = &result {
                    {
                        let mut current = state.lock().unwrap();
                        current.iso = Some(iso.clone());
                        current.local_iso = Some(path.clone());
                        current.selected = None;
                    }
                    let name = iso.name.clone();
                    let detail = format!("{} · descargada de SourceForge", human(iso.size));
                    let local_file = path.to_string_lossy().into_owned();
                    ui(weak.clone(), move |window| {
                        window.set_iso_name(name.into());
                        window.set_iso_detail(detail.into());
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
            let iso = state.lock().unwrap().iso.clone();
            let weak = weak.clone();
            let state = state.clone();
            let download_state = download_state.clone();
            thread::spawn(move || {
                let result = iso
                    .ok_or("Busca primero una ISO".to_owned())
                    .and_then(|iso| {
                        download_iso(&iso, weak.clone(), &download_state).map(|path| (iso, path))
                    });
                if let Ok((iso, path)) = &result {
                    {
                        let mut current = state.lock().unwrap();
                        current.local_iso = Some(path.clone());
                        current.selected = None;
                    }
                    let label = path.to_string_lossy().to_string();
                    let detail = format!("{} · descargada de SourceForge", human(iso.size));
                    ui(weak.clone(), move |w| {
                        w.set_local_file(label.into());
                        w.set_iso_detail(detail.into());
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
        window.on_load_local_iso(move || {
            let Some(window) = weak.upgrade() else { return };
            if state.lock().unwrap().busy {
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
                let mut current = state.lock().unwrap();
                current.iso = Some(iso);
                current.local_iso = Some(path.clone());
                current.selected = None;
            }
            log_event(format!(
                "INFO etapa=iso_local seleccionada={} bytes={size}",
                path.display()
            ));
            window.set_iso_name(name.into());
            window.set_iso_detail(format!("ISO local · {}", human(size)).into());
            window.set_local_file(path.to_string_lossy().into_owned().into());
            window.set_iso_stage(2);
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
            window.set_status_text("Buscando discos USB…".into());
            log_event("INFO etapa=detectar_usb inicio".into());
            let weak = weak.clone();
            let state = state.clone();
            thread::spawn(move || {
                let result = disks();
                if let Ok(disks) = &result {
                    let rows = disk_rows(disks);
                    state.lock().unwrap().disks = disks.clone();
                    state.lock().unwrap().selected = None;
                    ui(weak.clone(), move |w| {
                        w.set_disks(ModelRc::new(VecModel::from(rows)));
                        w.set_selected_disk(-1);
                        w.set_reuse_ventoy(false);
                    });
                }
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
                let mut state = state.lock().unwrap();
                state.selected = Some(number as u32);
                state
                    .disks
                    .iter()
                    .find(|disk| disk.number == number as u32)
                    .and_then(|disk| ventoy_data_letter(disk).ok())
                    .flatten()
                    .is_some()
            };
            if let Some(w) = weak.upgrade() {
                w.set_selected_disk(number);
                w.set_reuse_ventoy(reuse);
            }
        });
    }
    {
        let weak = window.as_weak();
        let state = state.clone();
        window.on_request_prepare(move |force_reinstall| {
            let Some(window) = weak.upgrade() else { return };
            let state = state.lock().unwrap();
            if state.iso.is_none() || state.local_iso.is_none() {
                window.set_status_text("Descarga o carga una ISO antes de preparar el USB".into());
                return;
            }
            let Some(number) = state.selected else { return };
            let Some(disk) = state.disks.iter().find(|d| d.number == number) else {
                return;
            };
            let reuse = match ventoy_data_letter(disk) {
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
            window.set_confirm_visible(true);
        });
    }
    {
        let weak = window.as_weak();
        window.on_cancel_prepare(move || {
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
            if !begin(&state, &window) {
                return;
            }
            let gpt = window.get_gpt();
            let ntfs = window.get_ntfs();
            let reuse_expected = window.get_reuse_ventoy();
            let force_reinstall = window.get_confirm_reinstall();
            let snapshot = {
                let state = state.lock().unwrap();
                (
                    state
                        .selected
                        .and_then(|n| state.disks.iter().find(|d| d.number == n).cloned()),
                    state.iso.clone(),
                    state.local_iso.clone(),
                )
            };
            let weak = weak.clone();
            let state = state.clone();
            thread::spawn(move || {
                let result = match snapshot {
                    (Some(disk), Some(iso), Some(path)) => prepare(
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
                            let mut current = state.lock().unwrap();
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
                            let mut current = state.lock().unwrap();
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
                        window.set_success_visible(true);
                    });
                }
            });
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
    fn ventoy_is_reused_only_with_verified_data_and_boot_partitions() {
        let mut disk = Disk {
            number: 9,
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
                    label: Some("Ventoy".into()),
                    file_system: Some("exFAT".into()),
                    size: 31_000_000_000,
                    partition_number: 1,
                },
                Volume {
                    letter: None,
                    label: Some("VTOYEFI".into()),
                    file_system: Some("FAT".into()),
                    size: 32 * 1024 * 1024,
                    partition_number: 2,
                },
            ],
        };
        assert_eq!(ventoy_data_letter(&disk).unwrap().as_deref(), Some("M"));
        disk.volumes[0].label = Some("WinSlim USB".into());
        assert_eq!(ventoy_data_letter(&disk).unwrap().as_deref(), Some("M"));
        disk.volumes[0].file_system = Some("FAT32".into());
        disk.volumes[1].letter = Some("\0".into());
        assert_eq!(ventoy_data_letter(&disk).unwrap().as_deref(), Some("M"));
        assert_eq!(disk_label(&disk).matches("M:").count(), 1);
        disk.volumes[1].label = None;
        assert!(ventoy_data_letter(&disk).is_err());
        disk.volumes.clear();
        assert!(ventoy_data_letter(&disk).unwrap().is_none());
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
    fn changed_usb_identity_is_rejected() {
        let disk = Disk {
            number: 2,
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
    fn free_space_check_uses_windows_api() {
        assert!(fs_free_bytes(&std::env::temp_dir()).unwrap() > 0);
    }

    #[test]
    fn local_iso_on_target_usb_is_rejected() {
        let disk = Disk {
            number: 5,
            friendly_name: "USB".into(),
            serial_number: None,
            size: 32_000_000_000,
            bus_type: "USB".into(),
            is_boot: false,
            is_system: false,
            is_read_only: false,
            volumes: vec![Volume {
                letter: Some("E".into()),
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
