//! Integración exclusiva de Linux. La ventana, descarga y copia verificada se
//! comparten con Windows; aquí solo viven discos, permisos y Ventoy de Linux.
use super::*;
use std::{
    ffi::CString,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    process::Stdio,
};

const VENTOY_ARCHIVE: &[u8] = include_bytes!("../../vendor/ventoy-1.1.17-linux.tar.gz");
const VENTOY_SHA256: &str = "7fb4ed08cef6a6b4d39dd19260d8c80291a78dfdf9af7d461571e23cbbc43805";

pub(super) fn os_version() -> String {
    Command::new("uname")
        .arg("-r")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "Linux".into())
}

fn tpm_flag(path: &Path) -> Result<Option<bool>, ()> {
    match fs::read_to_string(path) {
        Ok(value) => match value.trim() {
            "1" => Ok(Some(true)),
            "0" => Ok(Some(false)),
            _ => Err(()),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

pub(super) fn firmware_security_status() -> FirmwareSecurityStatus {
    let tpm = match fs::read_dir("/sys/class/tpm") {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            FirmwareFeatureState::Disabled
        }
        Err(_) => FirmwareFeatureState::Unknown,
        Ok(entries) => {
            let mut uncertain = false;
            let mut available = false;
            for entry in entries {
                let Ok(entry) = entry else {
                    uncertain = true;
                    continue;
                };
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with("tpm") || !name[3..].chars().all(|ch| ch.is_ascii_digit()) {
                    continue;
                }
                let device = entry.path().join("device");
                let flags = ["enabled", "active", "temp_deactivated"]
                    .map(|name| tpm_flag(&device.join(name)));
                if flags.iter().any(Result::is_err) {
                    uncertain = true;
                    continue;
                }
                if flags[0] == Ok(Some(false))
                    || flags[1] == Ok(Some(false))
                    || flags[2] == Ok(Some(true))
                {
                    continue;
                }
                available = true;
                break;
            }
            if available {
                FirmwareFeatureState::Enabled
            } else if uncertain {
                FirmwareFeatureState::Unknown
            } else {
                FirmwareFeatureState::Disabled
            }
        }
    };

    let secure_boot = if !Path::new("/sys/firmware/efi").exists() {
        FirmwareFeatureState::Disabled
    } else {
        let variable = "/sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c";
        match fs::read(variable)
            .ok()
            .and_then(|bytes| bytes.get(4).copied())
        {
            Some(1) => FirmwareFeatureState::Enabled,
            Some(0) => FirmwareFeatureState::Disabled,
            _ => FirmwareFeatureState::Unknown,
        }
    };

    FirmwareSecurityStatus { tpm, secure_boot }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub(super) fn log_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".local/state")))?;
    Some(base.join("winslim-usb-creator/operations.log"))
}

pub(super) fn downloads_dir() -> Result<PathBuf, String> {
    // XDG_DOWNLOAD_DIR puede variar según el idioma. xdg-user-dir resuelve
    // esa configuración sin asumir que existe una carpeta llamada Downloads.
    let base = Command::new("xdg-user-dir")
        .arg("DOWNLOAD")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
        .filter(|path| path.is_absolute())
        .or_else(|| home_dir().map(|home| home.join("Downloads")))
        .ok_or("No se encontró el directorio personal")?;
    Ok(base.join("WinSlim"))
}

pub(super) fn volume_display(volume: &Volume) -> Option<String> {
    volume
        .mount_path
        .clone()
        .or_else(|| volume.device_path.clone())
}

fn string(value: &serde_json::Value, field: &str) -> Option<String> {
    value
        .get(field)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn integer(value: &serde_json::Value, field: &str) -> u64 {
    value
        .get(field)
        .and_then(|item| {
            item.as_u64()
                .or_else(|| item.as_bool().map(|value| if value { 1 } else { 0 }))
                .or_else(|| item.as_str()?.parse().ok())
        })
        .unwrap_or(0)
}

fn children(value: &serde_json::Value) -> &[serde_json::Value] {
    value
        .get("children")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn system_partition(value: &serde_json::Value) -> bool {
    matches!(
        string(value, "mountpoint").as_deref(),
        Some("/" | "/boot" | "/boot/efi")
    ) || children(value).iter().any(system_partition)
}

pub(super) fn disks() -> Result<Vec<Disk>, String> {
    let output = Command::new("lsblk")
        .args([
            "--json",
            "--bytes",
            "--output",
            "PATH,MODEL,SERIAL,SIZE,TRAN,RO,TYPE,FSTYPE,LABEL,MOUNTPOINT,PARTN",
        ])
        .output()
        .map_err(|error| format!("No se pudo ejecutar lsblk: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "lsblk: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let result = parse_lsblk(&output.stdout)?;
    log_event(format!("INFO etapa=detectar_usb cantidad={}", result.len()));
    Ok(result)
}

fn parse_lsblk(output: &[u8]) -> Result<Vec<Disk>, String> {
    let tree: serde_json::Value = serde_json::from_slice(output)
        .map_err(|error| format!("Respuesta de lsblk no válida: {error}"))?;
    let devices = tree
        .get("blockdevices")
        .and_then(serde_json::Value::as_array)
        .ok_or("lsblk no devolvió blockdevices")?;
    let mut result = Vec::new();
    for device in devices {
        if string(device, "type").as_deref() != Some("disk")
            || string(device, "tran").as_deref() != Some("usb")
            || system_partition(device)
        {
            continue;
        }
        let Some(path) = string(device, "path") else {
            continue;
        };
        let volumes = children(device)
            .iter()
            .filter(|item| string(item, "type").as_deref() == Some("part"))
            .map(|partition| Volume {
                letter: None,
                mount_path: string(partition, "mountpoint"),
                device_path: string(partition, "path"),
                label: string(partition, "label"),
                file_system: string(partition, "fstype"),
                size: integer(partition, "size"),
                partition_number: integer(partition, "partn") as u32,
            })
            .collect();
        result.push(Disk {
            number: result.len() as u32 + 1,
            device_path: Some(path.clone()),
            friendly_name: string(device, "model").unwrap_or_else(|| path.clone()),
            serial_number: string(device, "serial"),
            size: integer(device, "size"),
            bus_type: "USB".into(),
            is_boot: false,
            is_system: false,
            is_read_only: integer(device, "ro") != 0,
            volumes,
        });
    }
    Ok(result)
}

pub(super) fn same_disk(a: &Disk, b: &Disk) -> bool {
    a.device_path == b.device_path
        && a.device_path.is_some()
        && a.size == b.size
        && a.serial_number == b.serial_number
        && b.bus_type == "USB"
        && !b.is_boot
        && !b.is_system
        && !b.is_read_only
}

pub(super) fn ventoy_data_location(disk: &Disk) -> Result<Option<String>, String> {
    let efi = disk.volumes.iter().find(|volume| {
        volume
            .label
            .as_deref()
            .is_some_and(|label| label.eq_ignore_ascii_case("VTOYEFI"))
    });
    let data = disk.volumes.iter().find(|volume| {
        volume.partition_number == 1
            && volume.device_path.is_some()
            && volume.file_system.as_deref().is_some_and(|fs| {
                matches!(fs.to_ascii_lowercase().as_str(), "exfat" | "ntfs" | "vfat")
            })
    });
    if let (Some(efi), Some(data)) = (efi, data) {
        if efi.partition_number != data.partition_number
            && (8 * 1024 * 1024..=256 * 1024 * 1024).contains(&efi.size)
            && efi
                .file_system
                .as_deref()
                .is_some_and(|fs| fs.eq_ignore_ascii_case("vfat"))
        {
            return Ok(data.device_path.clone());
        }
    }
    let looks_like_ventoy = efi.is_some()
        || disk.volumes.iter().any(|volume| {
            volume.label.as_deref().is_some_and(|label| {
                label.eq_ignore_ascii_case("Ventoy") || label.eq_ignore_ascii_case("WinSlim USB")
            })
        });
    if looks_like_ventoy {
        Err("El USB parece tener Ventoy, pero no se pudo verificar VTOYEFI. No se ha formateado; revisa la unidad.".into())
    } else {
        Ok(None)
    }
}

pub(super) fn iso_on_selected_disk(path: &Path, disk: &Disk) -> bool {
    let Ok(source) = fs::metadata(path) else {
        return false;
    };
    disk.volumes
        .iter()
        .filter_map(|volume| volume.mount_path.as_ref())
        .any(|mount| fs::metadata(mount).is_ok_and(|root| source.dev() == root.dev()))
}

fn stable_usb_link(device: &str) -> Result<PathBuf, String> {
    let actual = fs::canonicalize(device)
        .map_err(|error| format!("El dispositivo USB ya no está disponible: {error}"))?;
    let mut links = fs::read_dir("/dev/disk/by-id")
        .map_err(|error| format!("No se pudo consultar la identidad estable del USB: {error}"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with("usb-") && !name.contains("-part")
            })
        })
        .filter(|path| fs::canonicalize(path).is_ok_and(|target| target == actual))
        .collect::<Vec<_>>();
    links.sort();
    links.into_iter().next().ok_or_else(|| {
        "No se encontró una ruta USB estable en /dev/disk/by-id. No se ha formateado nada.".into()
    })
}

fn ventoy_package() -> Result<PathBuf, String> {
    let mut hash = Sha256::new();
    hash.update(VENTOY_ARCHIVE);
    if format!("{:x}", hash.finalize()) != VENTOY_SHA256 {
        return Err("El paquete Linux de Ventoy no pasó la verificación SHA-256".into());
    }
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".cache")))
        .ok_or("No se encontró el directorio de caché")?
        .join("winslim-usb-creator");
    let staging = cache.join(format!(
        "ventoy-run-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos()
    ));
    let package = staging.join("ventoy-1.1.17");
    fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
    let archive = staging.join("ventoy-1.1.17-linux.tar.gz");
    fs::write(&archive, VENTOY_ARCHIVE).map_err(|error| error.to_string())?;
    let output = Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&staging)
        .output()
        .map_err(|error| format!("No se pudo extraer Ventoy: {error}"))?;
    if !output.status.success() || !package.join("Ventoy2Disk.sh").exists() {
        return Err(format!(
            "Falló la extracción de Ventoy: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let _ = fs::remove_file(&archive);
    // El lanzador oficial reenvía los argumentos con $*, que divide etiquetas
    // con espacios. Conservar cada argumento permite usar «WinSlim USB».
    let script = package.join("Ventoy2Disk.sh");
    let original = fs::read_to_string(&script).map_err(|error| error.to_string())?;
    let corrected = original
        .replace(
            "/bin/bash ./tool/VentoyWorker.sh $*",
            "/bin/bash ./tool/VentoyWorker.sh \"$@\"",
        )
        .replace(
            "ash ./tool/VentoyWorker.sh $*",
            "ash ./tool/VentoyWorker.sh \"$@\"",
        );
    if corrected == original {
        return Err("No se encontró el lanzador esperado en el paquete Linux de Ventoy".into());
    }
    fs::write(&script, corrected).map_err(|error| error.to_string())?;
    Ok(package)
}

fn executable_path(command: &str) -> Option<String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg("command -v \"$1\"")
        .arg("sh")
        .arg(command)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Path::new(path.as_str()).is_absolute().then_some(path)
}

fn available(command: &str) -> bool {
    executable_path(command).is_some()
}

fn privileged(mut command: Command, input: Option<&[u8]>) -> Result<(), String> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("No se pudo solicitar autorización de administrador: {error}"))?;
    if let Some(bytes) = input {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(bytes);
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "La operación privilegiada falló (código {:?}): {} {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim(),
            String::from_utf8_lossy(&output.stdout).trim()
        ))
    }
}

fn mount_data(device: &str) -> Result<PathBuf, String> {
    for _ in 0..15 {
        if let Some(mount) = disks()?
            .iter()
            .flat_map(|disk| &disk.volumes)
            .find(|volume| volume.device_path.as_deref() == Some(device))
            .and_then(|volume| volume.mount_path.as_deref())
            .map(PathBuf::from)
        {
            return Ok(mount);
        }
        let output = Command::new("udisksctl")
            .args(["mount", "-b", device])
            .output();
        if let Ok(output) = output {
            if !output.status.success() {
                log_event(format!(
                    "DEBUG etapa=montar_usb detalle={}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }
        thread::sleep(Duration::from_secs(2));
    }
    Err(
        "La partición de datos de Ventoy no se pudo montar. Comprueba que udisks2 esté disponible."
            .into(),
    )
}

pub(super) fn prepare(
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
        .find(|item| item.device_path == disk.device_path)
        .ok_or("El USB seleccionado ya no está conectado")?;
    if !same_disk(&disk, &current) {
        return Err("El USB seleccionado cambió. Vuelve a elegirlo.".into());
    }
    let existing = ventoy_data_location(&current)?;
    let action = usb_action(reuse_expected, existing.is_some(), force_reinstall)?;
    let reused = action == UsbAction::CopyExisting;
    if reused
        && iso.size > u32::MAX as u64
        && current.volumes.iter().any(|volume| {
            volume.partition_number == 1
                && volume
                    .file_system
                    .as_deref()
                    .is_some_and(|fs| fs.eq_ignore_ascii_case("vfat"))
        })
    {
        return Err("La partición FAT32 de Ventoy no admite una ISO de más de 4 GiB.".into());
    }
    if !reused && iso_on_selected_disk(&local_iso, &current) {
        return Err(
            "La ISO está guardada en el USB que se va a borrar. Muévela a otra unidad.".into(),
        );
    }
    if !reused && current.size < iso.size + 256 * 1024 * 1024 {
        return Err("El USB no tiene capacidad suficiente para la ISO y Ventoy".into());
    }
    // Comprobar dependencias antes de iniciar cualquier operación destructiva.
    if !available("udisksctl")
        || (!reused
            && (!available("pkexec") || !available("tar") || (ntfs && !available("mkfs.ntfs"))))
    {
        return Err(
            "Faltan udisksctl, pkexec, tar o mkfs.ntfs para preparar esta unidad en Linux.".into(),
        );
    }
    let device = current
        .device_path
        .as_deref()
        .ok_or("No se encontró el dispositivo USB")?;
    let partition = if reused {
        status(
            weak.clone(),
            "Ventoy ya está instalado; se conservarán sus datos…",
        );
        existing.ok_or("No se encontró la partición de datos de Ventoy")?
    } else {
        let stable_device = stable_usb_link(device)?;
        status(weak.clone(), "Verificando el paquete oficial de Ventoy…");
        let package = ventoy_package()?;
        status(
            weak.clone(),
            "Instalando el cargador de arranque GRUB de Ventoy en la unidad USB…",
        );
        let mut command = Command::new("pkexec");
        command
            .current_dir(&package)
            .arg("/bin/sh")
            .arg(package.join("Ventoy2Disk.sh"));
        command.arg(if force_reinstall { "-I" } else { "-i" });
        if gpt {
            command.arg("-g");
        }
        command.args(["-L", "WinSlim USB"]).arg(&stable_device);
        let result = privileged(command, Some(b"y\ny\n"));
        if let Some(staging) = package.parent() {
            let _ = fs::remove_dir_all(staging);
        }
        result?;
        let mut found = None;
        for _ in 0..30 {
            thread::sleep(Duration::from_secs(2));
            if let Some(updated) = disks()?
                .into_iter()
                .find(|item| item.device_path == current.device_path)
            {
                if !same_disk(&current, &updated) {
                    return Err("El dispositivo USB cambió durante la instalación".into());
                }
                found = ventoy_data_location(&updated).ok().flatten();
                if found.is_some() {
                    break;
                }
            }
        }
        let partition = found
            .ok_or("Ventoy terminó, pero no se pudo verificar la partición de datos y VTOYEFI")?;
        if ntfs {
            status(weak.clone(), "Formateando la partición de datos como NTFS…");
            // Algunos escritorios montan exFAT en cuanto Ventoy crea la partición.
            if disks()?
                .iter()
                .flat_map(|disk| &disk.volumes)
                .any(|volume| {
                    volume.device_path.as_deref() == Some(partition.as_str())
                        && volume.mount_path.is_some()
                })
            {
                let output = Command::new("udisksctl")
                    .args(["unmount", "-b", partition.as_str()])
                    .output()
                    .map_err(|error| format!("No se pudo desmontar la partición: {error}"))?;
                if !output.status.success() {
                    return Err(format!(
                        "No se pudo desmontar antes de NTFS: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    ));
                }
            }
            let mut command = Command::new("pkexec");
            let mkfs = executable_path("mkfs.ntfs").ok_or("No se encontró mkfs.ntfs")?;
            let stable_partition = PathBuf::from(format!("{}-part1", stable_device.display()));
            let stable_target = fs::canonicalize(&stable_partition)
                .map_err(|_| "No se encontró la partición USB estable; no se aplicó NTFS")?;
            let current_target = fs::canonicalize(&partition)
                .map_err(|_| "La partición USB desapareció; no se aplicó NTFS")?;
            if stable_target != current_target {
                return Err("La identidad de la partición USB cambió; no se aplicó NTFS.".into());
            }
            command
                .arg(mkfs)
                .args(["-Q", "-F", "-L", "WinSlim USB"])
                .arg(&stable_partition);
            privileged(command, None)?;
        }
        partition
    };
    let root = mount_data(&partition)?;
    let mounted_disk = disks()?
        .into_iter()
        .find(|item| item.device_path == current.device_path)
        .ok_or("El USB desapareció antes de copiar la ISO")?;
    if !same_disk(&current, &mounted_disk)
        || ventoy_data_location(&mounted_disk)?.as_deref() != Some(partition.as_str())
    {
        return Err("El USB cambió antes de copiar la ISO; operación detenida.".into());
    }
    let destination = if reused {
        unused_iso_path(&root, &iso.name)
    } else {
        root.join(&iso.name)
    };
    if fs_free_bytes(&root)? < iso.size {
        return Err("La partición no tiene espacio suficiente para la ISO".into());
    }
    let temporary = root.join(format!(
        ".winslim-copy-{}-{}.part",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| error.to_string())?
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
    status(weak, "Configurando el tema de arranque de WinSlim…");
    let theme_warning = match install_winslim_ventoy_theme(&root) {
        Ok(_) => "",
        Err(error) => {
            log_event(format!("ERROR etapa=configurar_ventoy detalle={error}"));
            " · tema no aplicado (consulta Registro)"
        }
    };
    Ok(format!(
        "USB preparado: {} · {}{}",
        disk.friendly_name,
        destination.display(),
        theme_warning
    ))
}

pub(super) fn publish_iso(from: &Path, to: &Path) -> Result<(), String> {
    let target = to;
    let from = CString::new(from.as_os_str().as_bytes()).map_err(|error| error.to_string())?;
    let to = CString::new(to.as_os_str().as_bytes()).map_err(|error| error.to_string())?;
    if unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } != 0
    {
        return Err(file_error(
            "finalizar copia USB",
            target,
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

pub(super) fn publish_config(from: &Path, to: &Path) -> Result<(), String> {
    fs::rename(from, to).map_err(|error| file_error("publicar configuración de Ventoy", to, error))
}

pub(super) fn fs_free_bytes(root: &Path) -> Result<u64, String> {
    let path = CString::new(root.as_os_str().as_bytes()).map_err(|error| error.to_string())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(file_error(
            "comprobar espacio libre",
            root,
            std::io::Error::last_os_error(),
        ));
    }
    let stat = unsafe { stat.assume_init() };
    Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
}

pub(super) fn blurred_backdrop(_window: &MainWindow) -> Option<slint::Image> {
    // Wayland no permite capturar una ventana sin un portal y consentimiento.
    None
}

pub(super) fn size_initial_window(window: &MainWindow) {
    let available = window
        .window()
        .with_winit_window(|native| native.current_monitor().map(|monitor| monitor.size()));
    if let Some(Some(size)) = available {
        let scale = window.window().scale_factor().max(1.0);
        let width = 1200.0_f32.min(size.width as f32 / scale - 24.0).max(680.0);
        let height = 1040.0_f32.min(size.height as f32 / scale - 48.0).max(530.0);
        window
            .window()
            .set_size(slint::LogicalSize::new(width, height));
    }
}

pub(super) fn restart_to_usb() -> Result<(), String> {
    let output = Command::new("systemctl")
        .args(["reboot", "--firmware-setup"])
        .output()
        .map_err(|error| format!("No se pudo solicitar el reinicio: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "No se pudo abrir el firmware para elegir el USB: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

pub(super) fn restart_instructions() -> &'static str {
    "Guarda tu trabajo. Linux reiniciará en el menú del firmware para que elijas tu USB."
}

pub(super) fn open_folder(path: &Path) -> Result<(), String> {
    Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

pub(super) fn open_url(url: &str) -> Result<(), String> {
    Command::new("xdg-open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsblk_excludes_system_and_non_usb_disks() {
        let sample = br#"{"blockdevices":[
            {"path":"/dev/nvme0n1","type":"disk","tran":"nvme","size":500000000000,"children":[{"path":"/dev/nvme0n1p1","type":"part","mountpoint":"/"}]},
            {"path":"/dev/sda","type":"disk","tran":"usb","size":32000000000,"ro":false,"model":"USB","serial":"ABC","children":[
                {"path":"/dev/sda1","type":"part","partn":1,"fstype":"exfat","label":"WinSlim USB","size":31900000000,"mountpoint":"/media/user/USB"},
                {"path":"/dev/sda2","type":"part","partn":2,"fstype":"vfat","label":"VTOYEFI","size":33554432,"mountpoint":null}
            ]},
            {"path":"/dev/sdb","type":"disk","tran":"usb","size":32000000000,"ro":false,"children":[{"type":"part","mountpoint":"/boot"}]}
        ]}"#;
        let found = parse_lsblk(sample).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].device_path.as_deref(), Some("/dev/sda"));
        assert_eq!(
            ventoy_data_location(&found[0]).unwrap().as_deref(),
            Some("/dev/sda1")
        );
    }

    #[test]
    fn usb_identity_change_is_rejected() {
        let sample = br#"{"blockdevices":[{"path":"/dev/sdc","type":"disk","tran":"usb","size":16000000000,"serial":"A","children":[]}]}"#;
        let original = parse_lsblk(sample).unwrap().remove(0);
        let mut replacement = original.clone();
        assert!(same_disk(&original, &replacement));
        replacement.serial_number = Some("B".into());
        assert!(!same_disk(&original, &replacement));
    }
}
