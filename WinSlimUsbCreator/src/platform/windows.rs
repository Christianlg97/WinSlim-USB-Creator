use super::*;
use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::ffi::OsStr;
use std::mem::{offset_of, size_of};
use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, process::CommandExt};
use windows_sys::Win32::Storage::FileSystem::{
    BusTypeUsb, CreateFileW, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose,
    GetDiskFreeSpaceExW, GetVolumeInformationW, GetVolumeNameForVolumeMountPointW,
    GetVolumePathNameW, GetVolumePathNamesForVolumeNameW, MoveFileExW, MoveFileW, SetVolumeLabelW,
    FILE_FLAG_NO_BUFFERING, FILE_SHARE_READ, FILE_SHARE_WRITE,
    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::{
    PropertyStandardQuery, StorageDeviceProperty, DISK_ATTRIBUTE_READ_ONLY, DISK_EXTENT,
    DISK_GEOMETRY_EX, DRIVE_LAYOUT_INFORMATION_EX, GET_DISK_ATTRIBUTES,
    IOCTL_DISK_GET_DISK_ATTRIBUTES, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
    IOCTL_DISK_GET_DRIVE_LAYOUT_EX, IOCTL_DISK_IS_WRITABLE, IOCTL_STORAGE_GET_DEVICE_NUMBER,
    IOCTL_STORAGE_QUERY_PROPERTY, PARTITION_INFORMATION_EX, STORAGE_DEVICE_DESCRIPTOR,
    STORAGE_DEVICE_NUMBER, STORAGE_PROPERTY_QUERY, VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_CANCELLED, ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA,
        ERROR_WRITE_PROTECT, HANDLE, INVALID_HANDLE_VALUE, RECT, WAIT_OBJECT_0,
    },
    Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BI_RGB, DIB_RGB_COLORS, SRCCOPY,
    },
    System::{
        Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE},
        Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ},
        SystemInformation::{FirmwareTypeBios, GetFirmwareType, GetSystemWindowsDirectoryW},
        Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE},
        TpmBaseServices::{Tbsi_GetDeviceInfo, TBS_SUCCESS, TPM_DEVICE_INFO},
        IO::DeviceIoControl,
    },
    UI::{
        HiDpi::GetDpiForSystem,
        Shell::{
            ShellExecuteExW, ShellExecuteW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS,
            SHELLEXECUTEINFOW,
        },
        WindowsAndMessaging::{
            GetClientRect, SystemParametersInfoW, SPI_GETWORKAREA, SW_SHOWNORMAL,
        },
    },
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// `DeviceType` de `STORAGE_DEVICE_NUMBER` para discos (FILE_DEVICE_DISK).
const FILE_DEVICE_DISK: u32 = 7;
/// Abrir un `\\.\PhysicalDriveN` inexistente falla al instante, así que se
/// recorren todos los números: al retirar discos la numeración queda con huecos.
const MAX_PHYSICAL_DRIVES: u32 = 256;
const TBS_E_TPM_NOT_FOUND: u32 = 0x8028_400F;

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

// ---------------------------------------------------------------------------
// Utilidades Win32. Toda la detección usa APIs nativas en lugar de PowerShell:
// Get-Disk y Get-Partition devuelven una lista vacía a un usuario sin
// elevación en algunos equipos (p. ej. con controladores AMD-RAID), arrancar
// PowerShell tarda segundos y podía mostrar brevemente una consola.
// ---------------------------------------------------------------------------

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn to_wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain(Some(0)).collect()
}

fn string_from_wide(buffer: &[u16]) -> String {
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

fn invalid_data(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.to_owned())
}

/// Abre un disco o volumen solo para consultar metadatos: con acceso 0 los
/// IOCTL de consulta funcionan sin elevación y no bloquean la unidad.
fn open_device(path: &str) -> std::io::Result<OwnedHandle> {
    let name = to_wide(path);
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(OwnedHandle(handle))
    }
}

/// Ejecuta un IOCTL y devuelve los bytes recibidos. El búfer es de `u64` para
/// respetar la alineación de 8 bytes de las estructuras de Windows.
fn device_io<T>(
    handle: &OwnedHandle,
    code: u32,
    input: Option<&T>,
    output: &mut [u64],
) -> std::io::Result<usize> {
    let (input_ptr, input_len) = input.map_or((std::ptr::null(), 0), |value| {
        ((value as *const T).cast(), size_of::<T>() as u32)
    });
    let mut returned = 0u32;
    let success = unsafe {
        DeviceIoControl(
            handle.0,
            code,
            input_ptr,
            input_len,
            output.as_mut_ptr().cast(),
            std::mem::size_of_val(output) as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if success == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(returned as usize)
    }
}

/// Igual que `device_io`, pero amplía el búfer cuando la respuesta no cabe
/// (tablas de particiones o volúmenes repartidos en varios discos).
fn device_io_growing(handle: &OwnedHandle, code: u32) -> std::io::Result<(Vec<u64>, usize)> {
    let mut words = 512;
    loop {
        let mut buffer = vec![0u64; words];
        match device_io::<()>(handle, code, None, &mut buffer) {
            Ok(length) => return Ok((buffer, length)),
            Err(error)
                if words < 128 * 1024
                    && error.raw_os_error().is_some_and(|code| {
                        code as u32 == ERROR_INSUFFICIENT_BUFFER || code as u32 == ERROR_MORE_DATA
                    }) =>
            {
                words *= 4
            }
            Err(error) => return Err(error),
        }
    }
}

fn buffer_bytes(buffer: &[u64], length: usize) -> &[u8] {
    let bytes = unsafe {
        std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), std::mem::size_of_val(buffer))
    };
    &bytes[..length.min(bytes.len())]
}

/// Copia un valor de `bytes[offset..]` si cabe entero. Solo se usa con enteros
/// y estructuras formadas por enteros: un `bool` de Windows con un valor
/// distinto de 0/1 no sería un `bool` válido de Rust.
fn read_at<T>(bytes: &[u8], offset: usize) -> Option<T> {
    let end = offset.checked_add(size_of::<T>())?;
    (end <= bytes.len())
        .then(|| unsafe { std::ptr::read_unaligned(bytes.as_ptr().add(offset).cast::<T>()) })
}

fn registry_dword(key: &str, value: &str) -> Option<u32> {
    let key = to_wide(key);
    let value = to_wide(value);
    let mut data = 0u32;
    let mut size = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    (status == 0).then_some(data)
}

fn registry_string(key: &str, value: &str) -> Option<String> {
    let key = to_wide(key);
    let value = to_wide(value);
    let mut data = [0u16; 512];
    let mut size = std::mem::size_of_val(&data) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut size,
        )
    };
    (status == 0).then(|| string_from_wide(&data))
}

pub(super) fn firmware_security_status() -> FirmwareSecurityStatus {
    // TBS y el Registro se consultan como usuario normal (Get-Tpm y
    // Confirm-SecureBootUEFI exigen elevación).
    let mut info = TPM_DEVICE_INFO {
        structVersion: 0,
        tpmVersion: 0,
        tpmInterfaceType: 0,
        tpmImpRevision: 0,
    };
    let tpm = match unsafe {
        Tbsi_GetDeviceInfo(
            size_of::<TPM_DEVICE_INFO>() as u32,
            (&mut info as *mut TPM_DEVICE_INFO).cast(),
        )
    } {
        TBS_SUCCESS => FirmwareFeatureState::Enabled,
        TBS_E_TPM_NOT_FOUND => FirmwareFeatureState::Disabled,
        _ => FirmwareFeatureState::Unknown,
    };
    let secure_boot = match registry_dword(
        r"SYSTEM\CurrentControlSet\Control\SecureBoot\State",
        "UEFISecureBootEnabled",
    ) {
        Some(1) => FirmwareFeatureState::Enabled,
        Some(0) => FirmwareFeatureState::Disabled,
        Some(_) => FirmwareFeatureState::Unknown,
        None => {
            // Sin la clave del Registro: en BIOS heredada Secure Boot no existe.
            let mut firmware = 0;
            if unsafe { GetFirmwareType(&mut firmware) } != 0 && firmware == FirmwareTypeBios {
                FirmwareFeatureState::Disabled
            } else {
                FirmwareFeatureState::Unknown
            }
        }
    };
    FirmwareSecurityStatus { tpm, secure_boot }
}

struct DeviceInfo {
    bus_type: i32,
    name: String,
    serial: Option<String>,
}

/// Cadena ASCII terminada en NUL dentro del descriptor; vacía si no existe.
/// Algunos pendrives incluyen caracteres de control en el número de serie.
fn ascii_at(bytes: &[u8], offset: u32) -> String {
    if offset == 0 {
        return String::new();
    }
    let Some(tail) = bytes.get(offset as usize..) else {
        return String::new();
    };
    let end = tail
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(tail.len());
    String::from_utf8_lossy(&tail[..end])
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>()
        .trim()
        .to_owned()
}

fn storage_device_info(handle: &OwnedHandle) -> std::io::Result<DeviceInfo> {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut buffer = vec![0u64; 512];
    let length = device_io(
        handle,
        IOCTL_STORAGE_QUERY_PROPERTY,
        Some(&query),
        &mut buffer,
    )?;
    let bytes = buffer_bytes(&buffer, length);
    let bus_type = read_at::<i32>(bytes, offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType))
        .ok_or_else(|| invalid_data("descriptor de almacenamiento incompleto"))?;
    let text = |offset: usize| ascii_at(bytes, read_at::<u32>(bytes, offset).unwrap_or(0));
    let vendor = text(offset_of!(STORAGE_DEVICE_DESCRIPTOR, VendorIdOffset));
    let product = text(offset_of!(STORAGE_DEVICE_DESCRIPTOR, ProductIdOffset));
    let serial = text(offset_of!(STORAGE_DEVICE_DESCRIPTOR, SerialNumberOffset));
    let name = [vendor, product]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    Ok(DeviceInfo {
        bus_type,
        name: if name.is_empty() {
            "Disco USB".into()
        } else {
            name
        },
        serial: (!serial.is_empty()).then_some(serial),
    })
}

/// Falla si no hay medio insertado (lector de tarjetas vacío).
fn disk_size(handle: &OwnedHandle) -> std::io::Result<u64> {
    let mut buffer = vec![0u64; 32];
    let length = device_io::<()>(handle, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, None, &mut buffer)?;
    read_at::<i64>(
        buffer_bytes(&buffer, length),
        offset_of!(DISK_GEOMETRY_EX, DiskSize),
    )
    .map(|size| size.max(0) as u64)
    .ok_or_else(|| invalid_data("geometría de disco incompleta"))
}

/// Solo lectura por el interruptor de protección o por el atributo del disco.
fn disk_read_only(handle: &OwnedHandle) -> bool {
    let write_protected = device_io::<()>(handle, IOCTL_DISK_IS_WRITABLE, None, &mut [])
        .is_err_and(|error| error.raw_os_error() == Some(ERROR_WRITE_PROTECT as i32));
    let mut buffer = [0u64; 4];
    let read_only_attribute =
        device_io::<()>(handle, IOCTL_DISK_GET_DISK_ATTRIBUTES, None, &mut buffer)
            .ok()
            .and_then(|length| {
                read_at::<u64>(
                    buffer_bytes(&buffer, length),
                    offset_of!(GET_DISK_ATTRIBUTES, Attributes),
                )
            })
            .is_some_and(|attributes| attributes & DISK_ATTRIBUTE_READ_ONLY != 0);
    write_protected || read_only_attribute
}

/// Particiones `(número, bytes)` del disco, también las que no tienen volumen.
fn disk_partitions(handle: &OwnedHandle) -> std::io::Result<Vec<(u32, u64)>> {
    let (buffer, length) = device_io_growing(handle, IOCTL_DISK_GET_DRIVE_LAYOUT_EX)?;
    let bytes = buffer_bytes(&buffer, length);
    let count = read_at::<u32>(
        bytes,
        offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionCount),
    )
    .unwrap_or(0);
    let first = offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry);
    let mut partitions = Vec::new();
    for index in 0..count as usize {
        let entry = first + index * size_of::<PARTITION_INFORMATION_EX>();
        let (Some(number), Some(size)) = (
            read_at::<u32>(
                bytes,
                entry + offset_of!(PARTITION_INFORMATION_EX, PartitionNumber),
            ),
            read_at::<i64>(
                bytes,
                entry + offset_of!(PARTITION_INFORMATION_EX, PartitionLength),
            ),
        ) else {
            break;
        };
        // Las entradas MBR vacías y los contenedores extendidos tienen número 0.
        if number != 0 && size > 0 {
            partitions.push((number, size as u64));
        }
    }
    Ok(partitions)
}

/// Volúmenes `\\?\Volume{GUID}\` del sistema, también los que no tienen letra
/// (como VTOYEFI).
fn volume_names() -> Vec<String> {
    let mut buffer = [0u16; 260];
    let find = unsafe { FindFirstVolumeW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if find == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let mut names = vec![string_from_wide(&buffer)];
    while unsafe { FindNextVolumeW(find, buffer.as_mut_ptr(), buffer.len() as u32) } != 0 {
        names.push(string_from_wide(&buffer));
    }
    unsafe { FindVolumeClose(find) };
    names
}

fn device_number(handle: &OwnedHandle) -> std::io::Result<STORAGE_DEVICE_NUMBER> {
    let mut buffer = [0u64; 2];
    let length = device_io::<()>(handle, IOCTL_STORAGE_GET_DEVICE_NUMBER, None, &mut buffer)?;
    read_at(buffer_bytes(&buffer, length), 0)
        .ok_or_else(|| invalid_data("número de dispositivo incompleto"))
}

/// Letra asignada al volumen (`"D"`), si tiene alguna.
fn volume_letter(volume: &str) -> Option<String> {
    let name = to_wide(volume);
    let mut buffer = vec![0u16; 1024];
    let mut needed = 0u32;
    let query = |buffer: &mut Vec<u16>, needed: &mut u32| unsafe {
        GetVolumePathNamesForVolumeNameW(
            name.as_ptr(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            needed,
        ) != 0
    };
    if !query(&mut buffer, &mut needed) {
        // Con muchas carpetas de montaje la lista no cabe: se repite con el tamaño pedido.
        if needed as usize <= buffer.len() {
            return None;
        }
        buffer = vec![0u16; needed as usize];
        if !query(&mut buffer, &mut needed) {
            return None;
        }
    }
    buffer
        .split(|&unit| unit == 0)
        .map(String::from_utf16_lossy)
        .find_map(|path| {
            let mut chars = path.chars();
            let letter = chars.next()?;
            (letter.is_ascii_alphabetic() && chars.as_str() == ":\\")
                .then(|| letter.to_ascii_uppercase().to_string())
        })
}

/// Etiqueta y sistema de archivos; `None` si el volumen no tiene formato o no se lee.
fn volume_label_and_file_system(volume: &str) -> (Option<String>, Option<String>) {
    let root = to_wide(volume);
    let mut label = [0u16; 261];
    let mut file_system = [0u16; 261];
    let success = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            file_system.as_mut_ptr(),
            file_system.len() as u32,
        )
    };
    if success == 0 {
        return (None, None);
    }
    let non_empty = |text: String| (!text.is_empty()).then_some(text);
    (
        non_empty(string_from_wide(&label)),
        non_empty(string_from_wide(&file_system)),
    )
}

/// Discos que ocupa un volumen (uno distribuido puede abarcar varios).
fn volume_disk_numbers(device: &str) -> std::io::Result<Vec<u32>> {
    let handle = open_device(device)?;
    let (buffer, length) = device_io_growing(&handle, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS)?;
    let bytes = buffer_bytes(&buffer, length);
    let count =
        read_at::<u32>(bytes, offset_of!(VOLUME_DISK_EXTENTS, NumberOfDiskExtents)).unwrap_or(0);
    let first = offset_of!(VOLUME_DISK_EXTENTS, Extents);
    Ok((0..count as usize)
        .map_while(|index| {
            read_at::<u32>(
                bytes,
                first + index * size_of::<DISK_EXTENT>() + offset_of!(DISK_EXTENT, DiskNumber),
            )
        })
        .collect())
}

/// Discos con Windows (arranque) y con la partición de sistema/EFI. No se
/// listan nunca, aunque sean USB (Windows To Go), igual que `IsBoot`/`IsSystem`.
fn protected_disks() -> Result<(Vec<u32>, Vec<u32>), String> {
    let mut buffer = [0u16; 260];
    let length =
        unsafe { GetSystemWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    let windows = String::from_utf16_lossy(&buffer[..length.min(buffer.len())]);
    let drive = windows
        .get(..2)
        .filter(|drive| drive.ends_with(':'))
        .ok_or("No se pudo identificar la unidad de Windows")?;
    let boot = volume_disk_numbers(&format!(r"\\.\{drive}"))
        .map_err(|error| format!("No se pudo identificar el disco de Windows: {error}"))?;
    let system = match registry_string(r"SYSTEM\Setup", "SystemPartition") {
        Some(device) => {
            volume_disk_numbers(&format!(r"\\?\GLOBALROOT{device}")).unwrap_or_else(|error| {
                log_event(format!(
                    "WARNING etapa=detectar_usb particion_sistema={device} detalle={error}"
                ));
                Vec::new()
            })
        }
        None => {
            log_event("WARNING etapa=detectar_usb particion_sistema=desconocida".into());
            Vec::new()
        }
    };
    Ok((boot, system))
}

/// Completa letra, etiqueta y sistema de archivos de las particiones de `disks`.
fn attach_volumes(disks: &mut [Disk]) {
    for name in volume_names() {
        let Ok(handle) = open_device(name.trim_end_matches('\\')) else {
            continue;
        };
        let Ok(location) = device_number(&handle) else {
            continue;
        };
        drop(handle);
        if location.DeviceType != FILE_DEVICE_DISK {
            continue;
        }
        let Some(disk) = disks
            .iter_mut()
            .find(|disk| disk.number == location.DeviceNumber)
        else {
            continue;
        };
        let (label, file_system) = volume_label_and_file_system(&name);
        let letter = volume_letter(&name);
        match disk
            .volumes
            .iter_mut()
            .find(|volume| volume.partition_number == location.PartitionNumber)
        {
            Some(volume) => {
                volume.letter = letter;
                volume.label = label;
                volume.file_system = file_system;
            }
            // Sin tabla de particiones (superfloppy) el volumen ocupa todo el disco.
            None => disk.volumes.push(Volume {
                letter,
                mount_path: None,
                device_path: None,
                label,
                file_system,
                size: if location.PartitionNumber == 0 {
                    disk.size
                } else {
                    0
                },
                partition_number: location.PartitionNumber,
            }),
        }
    }
    for disk in disks {
        disk.volumes.sort_by_key(|volume| volume.partition_number);
    }
}

pub(super) fn disks() -> Result<Vec<Disk>, String> {
    let (boot_disks, system_disks) = protected_disks()?;
    let mut found = Vec::new();
    for number in 0..MAX_PHYSICAL_DRIVES {
        let Ok(handle) = open_device(&format!(r"\\.\PhysicalDrive{number}")) else {
            continue;
        };
        let info = match storage_device_info(&handle) {
            Ok(info) => info,
            Err(error) => {
                log_event(format!(
                    "DEBUG etapa=detectar_usb numero={number} descriptor_no_disponible detalle={error}"
                ));
                continue;
            }
        };
        if info.bus_type != BusTypeUsb {
            continue;
        }
        if boot_disks.contains(&number) || system_disks.contains(&number) {
            log_event(format!(
                "INFO etapa=detectar_usb numero={number} modelo={} excluido=disco_de_sistema",
                info.name
            ));
            continue;
        }
        let size = match disk_size(&handle) {
            Ok(size) if size > 0 => size,
            result => {
                log_event(format!(
                    "DEBUG etapa=detectar_usb numero={number} modelo={} sin_medio detalle={:?}",
                    info.name,
                    result.err()
                ));
                continue;
            }
        };
        let partitions = disk_partitions(&handle).unwrap_or_else(|error| {
            log_event(format!(
                "WARNING etapa=detectar_usb numero={number} particiones_no_disponibles detalle={error}"
            ));
            Vec::new()
        });
        found.push(Disk {
            number,
            device_path: None,
            friendly_name: info.name,
            serial_number: info.serial,
            size,
            bus_type: "USB".into(),
            is_boot: false,
            is_system: false,
            is_read_only: disk_read_only(&handle),
            volumes: partitions
                .into_iter()
                .map(|(partition_number, size)| Volume {
                    letter: None,
                    mount_path: None,
                    device_path: None,
                    label: None,
                    file_system: None,
                    size,
                    partition_number,
                })
                .collect(),
        });
    }
    if !found.is_empty() {
        attach_volumes(&mut found);
    }
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
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(VENTOY_ARCHIVE)).map_err(|e| e.to_string())?;
    // Ventoy se ejecuta con elevación desde una carpeta del usuario: la copia
    // extraída solo se reutiliza si coincide byte a byte con el paquete verificado.
    if exe.exists() {
        match extracted_ventoy_matches(&mut archive, &dir, &exe) {
            Ok(true) => {
                log_event(format!(
                    "INFO etapa=ventoy paquete_extraido verificado=true ruta={}",
                    package.display()
                ));
                return Ok(exe);
            }
            Ok(false) => log_event(format!(
                "WARNING etapa=ventoy paquete_extraido_distinto reextraer ruta={}",
                package.display()
            )),
            Err(error) => return Err(error),
        }
    }
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
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

const VENTOY_X64_ENTRY: &str = "ventoy-1.1.17/altexe/Ventoy2Disk_X64.exe";

fn sha256_of(reader: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let len = reader.read(&mut buffer)?;
        if len == 0 {
            break;
        }
        hash.update(&buffer[..len]);
    }
    Ok(hash.finalize().to_vec())
}

/// Un archivo ausente, ilegible o distinto cuenta como no coincidente.
fn file_matches_sha256(path: &Path, expected: &[u8]) -> bool {
    File::open(path)
        .and_then(|mut file| sha256_of(&mut file))
        .is_ok_and(|actual| actual == expected)
}

/// Compara cada archivo del ZIP con su copia extraída y el ejecutable x64 con
/// su original en `altexe`. Los archivos que genera Ventoy (cli_log.txt, etc.)
/// no forman parte del paquete y no se comprueban.
fn extracted_ventoy_matches(
    archive: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>,
    dir: &Path,
    exe: &Path,
) -> Result<bool, String> {
    let mut x64_hash = None;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        if entry.is_dir() {
            continue;
        }
        let Some(name) = entry.enclosed_name() else {
            return Err("Ruta no segura en el paquete de Ventoy".into());
        };
        let expected = sha256_of(&mut entry).map_err(|e| e.to_string())?;
        if !file_matches_sha256(&dir.join(&name), &expected) {
            return Ok(false);
        }
        if name == Path::new(VENTOY_X64_ENTRY) {
            x64_hash = Some(expected);
        }
    }
    Ok(x64_hash.is_some_and(|expected| file_matches_sha256(exe, &expected)))
}

/// Ejecuta `exe` con elevación (UAC), espera a que termine y devuelve su código
/// de salida. Sustituye a `Start-Process -Verb RunAs` de PowerShell: no lanza
/// un proceso auxiliar y distingue la cancelación de UAC de otros errores.
fn run_elevated(exe: &Path, arguments: &str, directory: &Path) -> Result<u32, String> {
    let verb = to_wide("runas");
    let file = to_wide(exe);
    let parameters = to_wide(arguments);
    let directory = to_wide(directory);
    // ShellExecuteEx puede delegar en extensiones COM del shell.
    let com = unsafe {
        CoInitializeEx(
            std::ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    };
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.lpDirectory = directory.as_ptr();
    info.nShow = SW_SHOWNORMAL;
    let launched = unsafe { ShellExecuteExW(&mut info) } != 0;
    let launch_error = std::io::Error::last_os_error();
    if com >= 0 {
        unsafe { CoUninitialize() };
    }
    if !launched {
        return Err(
            if launch_error.raw_os_error() == Some(ERROR_CANCELLED as i32) {
                "Se canceló la autorización de administrador (UAC). No se ha modificado el USB."
                    .into()
            } else {
                format!("No se pudo iniciar Ventoy con permisos de administrador: {launch_error}")
            },
        );
    }
    if info.hProcess.is_null() {
        return Err("Windows no devolvió el proceso de Ventoy".into());
    }
    let process = OwnedHandle(info.hProcess);
    if unsafe { WaitForSingleObject(process.0, INFINITE) } != WAIT_OBJECT_0 {
        return Err(format!(
            "No se pudo esperar a que Ventoy terminara: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut code = 0u32;
    if unsafe { GetExitCodeProcess(process.0, &mut code) } == 0 {
        return Err(format!(
            "No se pudo leer el resultado de Ventoy: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(code)
}

pub(super) fn install_ventoy(exe: &Path, disk: &Disk, gpt: bool, ntfs: bool) -> Result<(), String> {
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
    let exit_code = run_elevated(exe, &args, install_dir);
    log_event(format!(
        "INFO etapa=instalar_ventoy resultado_proceso={exit_code:?}"
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
    let exit_code = exit_code?;
    if exit_code != 0 {
        return Err(format!(
            "Ventoy terminó con error (código {exit_code}); revisa cli_log.txt en el Registro"
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

/// Discos físicos que ocupa el volumen donde está `path`. A diferencia de la
/// letra, también funciona con volúmenes montados en una carpeta.
fn path_disk_numbers(path: &Path) -> Option<Vec<u32>> {
    let path = to_wide(path);
    let mut mount_point = [0u16; 1024];
    if unsafe {
        GetVolumePathNameW(
            path.as_ptr(),
            mount_point.as_mut_ptr(),
            mount_point.len() as u32,
        )
    } == 0
    {
        return None;
    }
    let mut volume = [0u16; 64];
    if unsafe {
        GetVolumeNameForVolumeMountPointW(
            mount_point.as_ptr(),
            volume.as_mut_ptr(),
            volume.len() as u32,
        )
    } == 0
    {
        return None;
    }
    volume_disk_numbers(string_from_wide(&volume).trim_end_matches('\\')).ok()
}

pub(super) fn iso_on_selected_disk(path: &Path, disk: &Disk) -> bool {
    if path_disk_numbers(path).is_some_and(|numbers| numbers.contains(&disk.number)) {
        return true;
    }
    // Respaldo por letra, útil si el volumen no responde a la consulta anterior.
    let path_text = path.to_string_lossy();
    let path_text = path_text.strip_prefix(r"\\?\").unwrap_or(&path_text);
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
    verify_iso_for_usb(&local_iso, &iso, &weak)?;
    status(weak.clone(), "Comprobando la unidad USB…");
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
    remove_stale_usb_copies(&root);
    let free = fs_free_bytes(&root)?;
    if free < iso.size {
        return Err("La nueva partición no tiene espacio suficiente para la ISO".into());
    }
    let temporary = usb_copy_temp_path(&root)?;
    status(weak.clone(), "Copiando la ISO al USB…");
    if let Err(error) = copy_with_progress(&local_iso, &temporary, &iso, weak.clone()) {
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

/// Abre un archivo para leerlo directamente del dispositivo, sin la caché de
/// Windows. Exige búferes y tamaños de lectura alineados con el sector.
pub(super) fn open_uncached(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_NO_BUFFERING)
        .open(path)
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
    // La ventana nativa aún no existe y `scale_factor()` devolvería 1.0: con la
    // escala de Windows al 125-150 % la ventana saldría más alta que la pantalla.
    // El área de trabajo está en píxeles físicos, así que se usa el DPI del sistema.
    let scale = (unsafe { GetDpiForSystem() } as f32 / 96.0).max(1.0);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_strings_are_bounded_and_cleaned() {
        let bytes = b"\0\0\0\0 SanDisk \0\x03C\0";
        assert_eq!(ascii_at(bytes, 4), "SanDisk");
        assert_eq!(ascii_at(bytes, 14), "C");
        assert_eq!(ascii_at(bytes, 0), "");
        assert_eq!(ascii_at(bytes, 400), "");
        assert_eq!(read_at::<u32>(&[1, 0, 0, 0], 0), Some(1));
        assert_eq!(read_at::<u32>(&[1, 0, 0], 0), None);
        assert_eq!(read_at::<u32>(&[1, 0, 0, 0], usize::MAX), None);
    }

    #[test]
    #[ignore = "depends on the TPM and Secure Boot state of this computer"]
    fn firmware_status_is_readable() {
        let status = firmware_security_status();
        println!("tpm={:?} secure_boot={:?}", status.tpm, status.secure_boot);
    }

    #[test]
    fn windows_disk_is_identified_without_elevation() {
        let (boot, _system) = protected_disks().unwrap();
        assert!(!boot.is_empty());
        let windows = std::env::var("SystemRoot").unwrap();
        let numbers = path_disk_numbers(Path::new(&windows)).unwrap();
        assert!(numbers.iter().all(|number| boot.contains(number)));
    }

    #[test]
    fn modified_ventoy_extraction_is_detected() {
        let dir = std::env::temp_dir().join(format!(
            "winslim-ventoy-verify-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(VENTOY_ARCHIVE)).unwrap();
        archive.extract(&dir).unwrap();
        let exe = dir.join("ventoy-1.1.17").join("Ventoy2Disk_X64.exe");
        fs::copy(dir.join(VENTOY_X64_ENTRY), &exe).unwrap();
        assert!(extracted_ventoy_matches(&mut archive, &dir, &exe).unwrap());

        fs::write(&exe, b"otro programa").unwrap();
        assert!(!extracted_ventoy_matches(&mut archive, &dir, &exe).unwrap());

        fs::copy(dir.join(VENTOY_X64_ENTRY), &exe).unwrap();
        let image = dir
            .join("ventoy-1.1.17")
            .join("ventoy")
            .join("ventoy.disk.img.xz");
        fs::write(&image, b"").unwrap();
        assert!(!extracted_ventoy_matches(&mut archive, &dir, &exe).unwrap());

        fs::remove_dir_all(dir).unwrap();
    }
}
