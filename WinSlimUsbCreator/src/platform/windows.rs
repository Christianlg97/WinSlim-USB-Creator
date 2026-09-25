use super::*;
use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::os::windows::{ffi::OsStrExt, process::CommandExt};
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
    UI::{
        Shell::ShellExecuteW,
        WindowsAndMessaging::{
            GetClientRect, SystemParametersInfoW, SPI_GETWORKAREA, SW_SHOWNORMAL,
        },
    },
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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

pub(super) fn os_version() -> String {
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

const VENTOY_SHA256: &str = "d250e97a7595fdac4f97debc630d7a8da942319274a76cb32384596b659dbaeb";
const VENTOY_ARCHIVE: &[u8] = include_bytes!("../../vendor/ventoy-1.1.17-windows.zip");

pub(super) fn powershell(script: &str) -> Result<String, String> {
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

pub(super) fn firmware_security_status() -> FirmwareSecurityStatus {
    // Get-Tpm y Confirm-SecureBootUEFI requieren elevación. La enumeración PnP
    // y el estado de Secure Boot del Registro se pueden leer como usuario normal.
    const SCRIPT: &str = r#"$ErrorActionPreference='Stop'; $tpm=-1; $secureBoot=-1; try { $devices=@(Get-PnpDevice -PresentOnly -ErrorAction Stop | Where-Object { $_.Class -eq 'SecurityDevices' -and ($_.InstanceId -match '^ACPI\\(MSFT0101|PNP0C31)' -or $_.FriendlyName -match '(?i)TPM|Trusted Platform Module|M[oó]dulo de plataforma segura') }); if (@($devices | Where-Object { $_.Status -eq 'OK' }).Count -gt 0) { $tpm=1 } else { $tpm=0 } } catch {} ; try { $value=(Get-ItemProperty -LiteralPath 'HKLM:\SYSTEM\CurrentControlSet\Control\SecureBoot\State' -Name UEFISecureBootEnabled -ErrorAction Stop).UEFISecureBootEnabled; if ($value -eq 1) { $secureBoot=1 } elseif ($value -eq 0) { $secureBoot=0 } } catch {} ; if ($secureBoot -eq -1) { try { if (Confirm-SecureBootUEFI -ErrorAction Stop) { $secureBoot=1 } else { $secureBoot=0 } } catch {} }; [pscustomobject]@{Tpm=$tpm;SecureBoot=$secureBoot} | ConvertTo-Json -Compress"#;
    let unknown = FirmwareSecurityStatus {
        tpm: FirmwareFeatureState::Unknown,
        secure_boot: FirmwareFeatureState::Unknown,
    };
    let Ok(output) = powershell(SCRIPT) else {
        return unknown;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&output) else {
        return unknown;
    };
    let state = |name: &str| match value.get(name).and_then(serde_json::Value::as_i64) {
        Some(1) => FirmwareFeatureState::Enabled,
        Some(0) => FirmwareFeatureState::Disabled,
        _ => FirmwareFeatureState::Unknown,
    };
    FirmwareSecurityStatus {
        tpm: state("Tpm"),
        secure_boot: state("SecureBoot"),
    }
}

pub(super) fn disks() -> Result<Vec<Disk>, String> {
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

pub(super) fn ventoy_exe() -> Result<PathBuf, String> {
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

pub(super) fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub(super) fn install_ventoy(exe: &Path, disk: &Disk, gpt: bool, ntfs: bool) -> Result<(), String> {
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

pub(super) fn same_disk(a: &Disk, b: &Disk) -> bool {
    a.number == b.number
        && a.size == b.size
        && a.bus_type.eq_ignore_ascii_case("USB")
        && !b.is_boot
        && !b.is_system
        && !b.is_read_only
        && a.serial_number.as_deref().unwrap_or("").trim()
            == b.serial_number.as_deref().unwrap_or("").trim()
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

pub(super) fn set_usb_label(root: &Path) -> Result<(), String> {
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

pub(super) fn publish_iso(from: &Path, to: &Path) -> Result<(), String> {
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

pub(super) fn iso_on_selected_disk(path: &Path, disk: &Disk) -> bool {
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
        .find(|d| d.number == disk.number)
        .ok_or("El USB seleccionado ya no está conectado")?;
    if !same_disk(&disk, &current) {
        return Err("El USB seleccionado cambió. Vuelve a elegirlo.".into());
    }
    let existing_letter = ventoy_data_location(&current)?;
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
                target = ventoy_data_location(&updated).ok().flatten();
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

pub(super) fn fs_free_bytes(root: &Path) -> Result<u64, String> {
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

pub(super) fn blurred_backdrop(window: &MainWindow) -> Option<slint::Image> {
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

pub(super) fn size_initial_window(window: &MainWindow) {
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
    let available_height = (work_area.bottom - work_area.top) as f32 / scale - 32.0;
    let width = 1200.0_f32.min(available_width).max(680.0);
    let height = 1040.0_f32.min(available_height).max(530.0);
    window
        .window()
        .set_size(slint::LogicalSize::new(width, height));
}

pub(super) fn log_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|base| {
        PathBuf::from(base)
            .join("WinSlimUsbCreator")
            .join("operations.log")
    })
}

pub(super) fn downloads_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("USERPROFILE").ok_or("No se encontró el perfil de usuario")?;
    Ok(PathBuf::from(home).join("Downloads").join("WinSlim"))
}

pub(super) fn volume_display(volume: &Volume) -> Option<String> {
    drive_letter(volume).map(|letter| format!("{letter}:"))
}

pub(super) fn publish_config(from: &Path, to: &Path) -> Result<(), String> {
    let mut from_wide = from.as_os_str().encode_wide().collect::<Vec<_>>();
    let mut to_wide = to.as_os_str().encode_wide().collect::<Vec<_>>();
    from_wide.push(0);
    to_wide.push(0);
    if unsafe {
        MoveFileExW(
            from_wide.as_ptr(),
            to_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(file_error(
            "publicar configuración de Ventoy",
            to,
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

pub(super) fn restart_to_usb() -> Result<(), String> {
    let shutdown =
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
            .join("System32")
            .join("shutdown.exe");
    let output = Command::new(shutdown)
        .args(["/r", "/o", "/t", "0"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("Windows no pudo iniciar el reinicio: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let detail = if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        };
        Err(format!(
            "Windows no pudo iniciar el reinicio (código {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(detail).trim()
        ))
    }
}

pub(super) fn restart_instructions() -> &'static str {
    "Guarda tu trabajo. Windows abrirá Inicio avanzado; elige «Usar un dispositivo» y selecciona tu USB."
}

pub(super) fn open_folder(path: &Path) -> Result<(), String> {
    Command::new("explorer.exe")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

pub(super) fn open_url(url: &str) -> Result<(), String> {
    let operation: Vec<u16> = std::ffi::OsStr::new("open")
        .encode_wide()
        .chain(Some(0))
        .collect();
    let target: Vec<u16> = std::ffi::OsStr::new(url)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if result as isize > 32 {
        Ok(())
    } else {
        Err(format!(
            "Windows no pudo abrir el enlace (código {})",
            result as isize
        ))
    }
}
