# AI_MAP — Mapa Técnico del Proyecto para Asistentes IA

> **Regla obligatoria:** cualquier asistente que cambie código, scripts, recursos, dependencias, flujos o comportamiento del proyecto debe actualizar este archivo en el mismo cambio. Mantén el documento conciso, factual y sincronizado; no copies aquí implementaciones completas.

---

## 1. Propósito general y arquitectura

El repositorio combina dos piezas del ecosistema **WinSlim11**:
1. **WinSlim USB Creator** (`WinSlimUsbCreator/`): aplicación de escritorio nativa en **Rust + Slint** (sin WebView) para **Windows 10/11 x64** y **Linux x86_64** que localiza/descarga la última ISO de WinSlim desde SourceForge (o abre un mirror alternativo desde GitHub Releases / carga una ISO local), detecta unidades USB, instala o reutiliza **Ventoy 1.1.17** con tema GRUB personalizado y copia/verifica la ISO sin caché del sistema.
2. **Pipeline de subida de ISOs** (`SourceForge_Upload_ISO.cmd`): script independiente en Batch/PowerShell que aprovisiona `Cygwin64` (`rsync` + `openssh` en `C:\WSCore\Components\Cygwin64`) y sube ISOs locales a SourceForge FRS (`frs.sourceforge.net:/home/frs/project/winslim11-isos/`) usando `--partial-dir=.rsync-partial` para no publicar nunca una ISO incompleta en la raíz.

---

## 2. Estructura de archivos y responsabilidades

```text
WinSlim-USB-Creator/
├── AI_MAP.md                              # Este mapa técnico (mantener siempre sincronizado)
├── README.md                              # Documentación del ecosistema y de SourceForge_Upload_ISO.cmd
├── BugTrack.txt                           # Histórico/especificación de mejoras de UI y logs (no ejecutable)
├── Compilar.cmd                           # Único lanzador CMD en Windows; invoca scripts/build-windows.ps1 (acepta --no-pause)
├── SourceForge_Upload_ISO.cmd             # Subida desatendida de *.iso en la raíz hacia SourceForge vía rsync/SSH
├── .gitignore / .gitattributes            # Ignora *.iso, target/, release/, *.part; reserva Git LFS para binarios >100 MiB
└── WinSlimUsbCreator/                     # Crate Rust principal (winslim-usb-creator v0.1.0)
    ├── Cargo.toml                         # Dependencias, features Slint (backend-winit + renderer-software) y perfil release
    ├── build.rs                           # Compila ui/main.slint (estilo fluent-dark) e incrusta assets/icon.ico en Windows
    ├── README.md                          # Documentación funcional y técnica de la aplicación de escritorio
    ├── src/
    │   ├── main.rs                        # Núcleo compartido: estado, callbacks Slint, red, descarga, copia, tema Ventoy, logs y tests
    │   ├── segmented.rs                   # Descarga segmentada multifuente (SourceForge + alternativa), reanudable y verificada por MD5
    │   └── platform/
    │       ├── windows.rs                 # Implementación Windows: PowerShell/Win32, extracción/elevación Ventoy, GDI backdrop
    │       └── linux.rs                   # Implementación Linux: lsblk, udisksctl, pkexec, Ventoy2Disk.sh, libc/statvfs
    ├── ui/
    │   └── main.slint                     # Interfaz gráfica completa, componentes reutilizables, estados visuales y modales
    ├── assets/
    │   ├── icon.png / icon.ico            # Icono principal 256×256 de ventana/ejecutable
    │   ├── icon-header-40.png             # Icono pre-escalado 40×40 para la cabecera (evita downsampling en software renderer)
    │   ├── fonts/UbuntuSans-*.ttf         # Tipografía Ubuntu Sans v1.006 (Regular, Medium, SemiBold, Bold)
    │   └── ventoy-theme/                  # Tema GRUB WinSlim (background.png, icon.png 50×50, select_c.png, theme.txt)
    ├── vendor/
    │   ├── ventoy-1.1.17-windows.zip      # Paquete oficial Ventoy Windows embebido con include_bytes!
    │   └── ventoy-1.1.17-linux.tar.gz     # Paquete oficial Ventoy Linux embebido con include_bytes!
    ├── packaging/linux/
    │   └── winslim-usb-creator.desktop    # Metadatos Desktop Entry para empaquetado AppImage
    └── scripts/
        ├── build-windows.ps1              # Verifica/instala VS Build Tools + Windows SDK + Rust y genera release/WinSlim-USB-Creator.exe
        ├── build-linux.sh                 # Verifica SHA-256 de Ventoy, compila binario Linux y empaqueta AppImage con linuxdeploy
        ├── build-linux.ps1                # Puente opcional desde Windows hacia WSL para invocar scripts/build-linux.sh
        └── create-icon.ps1                # Regenera icon.png, icon-header-40.png e icon.ico mediante System.Drawing
```

---

## 3. Mapa de `WinSlimUsbCreator/src/main.rs`

| Bloque / Símbolos | Descripción y responsabilidad |
| :--- | :--- |
| `FirmwareFeatureState`, `FirmwareSecurityStatus` | Estado de `tpm` y `secure_boot` (`Enabled` -> `1`, `Disabled` -> `0`, `Unknown` -> `-1`). |
| Constantes de descarga y red | `DOWNLOAD_IDLE` (`0`), `DOWNLOAD_RUNNING` (`1`), `DOWNLOAD_CANCELLED` (`2`), `DOWNLOAD_COMMITTING` (`3`); `PARALLEL_MIN_BYTES` (`64 MiB`, umbral de la descarga segmentada), `MAX_MIRROR_PROBES` (`8`), `AVAILABILITY_CONNECT_TIMEOUT` (`4 s`), `AVAILABILITY_READ_TIMEOUT` (`5 s`), `RSS`, `ALTERNATIVE_RELEASE_API`. |
| `Iso`, `Disk`, `Volume`, `State` | Modelos principales. `State` (`Arc<Mutex<State>>`) guarda `iso`, `local_iso`, `disks`, `selected`, `confirming` (disco exacto mostrado en el modal de confirmación) y `busy`. |
| `lock_state`, `PanicGuard`, `ui`, `status`, `finish`, `finish_download`, `begin` | Concurrencia segura: `lock_state` se recupera de `PoisonError`; `PanicGuard` desbloquea `State.busy`, `download_state` y las propiedades Slint si un worker entra en pánico; `ui()` despacha al bucle de eventos con `slint::invoke_from_event_loop`. |
| `log_event`, `clear_log`, `export_log`, `read_log`, `http_error`, `file_error` | Registro persistente con rotación a `operations.log.1` al superar `LOG_MAX_BYTES` (`5 MiB`) protegido por `LOG_LOCK`. `read_log()` lee solo los últimos `500 KB` para no bloquear la UI. `http_error` y `file_error` traducen fallos técnicos a mensajes claros en UI y guardan la causa completa en el log. |
| `fetch_latest`, `parse_latest` | Consulta el RSS de SourceForge, filtra exclusivamente `WinSlim*.iso` en la raíz del proyecto (sin `/` ni `\`, `size >= 100 MB`), extrae `<media:hash algo="md5">` válido de 32 hex y selecciona el elemento con `pubDate` más reciente. |
| `alternative_link_from_notes`, `fetch_alternative_link`, `fetch_available_alternative_link`, `alternative_availability`, `report_main_server_failure` | Respaldo cuando SourceForge falla: consulta la release `Latest_Mirror_URL` en GitHub, exige un único enlace `https://` en `body`, verifica que responda por HTTP y que no redirija a login de Google (`accounts.google.com`). |
| `parse_mirror_choices`, `available_mirrors`, `resolve_download` | Lista de espejos (`/settings/mirror_choices`) que usa `segmented::discover_sources`, y resolución del enlace `/download` (redirección directa o `http-equiv="refresh"`) para la descarga de una conexión. |
| `check_download_available`, `check_latest_download_available`, `start_server_check` | Comprobación en segundo plano de disponibilidad del servidor principal mediante una única petición `Range: bytes=0-0` leyendo 1 byte real. Acepta HTTP `200` o `206` aunque el espejo omita `Content-Length`/`Content-Range`; rechaza `text/html` o tamaños explícitos distintos a `iso.size`. |
| `download_iso`, `download_iso_to`, `download_iso_to_inner`, `retry_iso_download` | Descargador en `Downloads/WinSlim`. Si hay MD5 y la ISO mide `>= 64 MiB` usa `segmented.rs` (ver §3.1); si no hay fuentes con rangos, convierte el `.part` a formato continuo (`convert_to_contiguous`) y usa 1 conexión con hasta 12 reintentos (backoff exponencial), reanudando el `.part` con su MD5 acumulado. Verifica tamaño y MD5 antes de renombrar `.part` a `.iso`; al cancelar borra `.part`, `.part.ranges` y la carpeta antigua `.part-segments/`. |
| `verify_file` | Verifica tamaño y MD5 de una ISO existente en disco reportando progreso a la UI y permitiendo cancelación. |
| `VERIFIED_ISO`, `remember_verified_iso`, `iso_recently_verified`, `verify_iso_for_usb` | Caché en memoria de la última ISO con MD5 verificado (ruta, tamaño, fecha de modificación, MD5). `download_iso_to` la rellena al terminar; `prepare` llama a `verify_iso_for_usb()` y omite la relectura completa si el archivo no cambió. |
| `UsbAction`, `usb_action` | Determina si la preparación debe `CopyExisting`, `InstallFresh` o `Reinstall`, abortando si la presencia de Ventoy cambió entre la selección y la ejecución. |
| `winslim_ventoy_config`, `install_winslim_ventoy_theme` | Escribe el tema en `/ventoy/winslim-theme/` y actualiza atómicamente `/ventoy/ventoy.json` (idioma inicial `es_ES`, modo `GUI`, texto de versión de Ventoy oculto en `#000000` sobre fondo negro). Conserva configuraciones previas y no sobrescribe temas personalizados ajenos. |
| `unused_iso_path`, `usb_copy_temp_path`, `remove_stale_usb_copies`, `copy_with_progress`, `copy_and_verify`, `read_back_md5` | Limpia `.winslim-copy-*.part` huérfanos, copia la ISO al USB con doble búfer de `8 MiB` en paralelo (lectura + MD5 de origen simultáneos a la escritura), exige que el MD5 de origen coincida con `iso.md5` (`expected_md5`), sincroniza (`sync_all`) y verifica releyendo del USB sin caché del SO (`open_uncached`). |
| `main` | Inicializa `MainWindow`, aplica workaround de repintado en Windows (`redraw-flip`), lanza detección inicial de USB, estado de TPM/Secure Boot y disponibilidad del servidor, activa timers (`log_timer` cada 700 ms, `server_timer` cada 600 s) y conecta todos los callbacks de Slint. |
| `mod tests` | Pruebas unitarias de parseo RSS, enlaces alternativos, espejos, disponibilidad HTTP `200`/`206`, descarga por rangos, reanudación, cancelación, fallo MD5, copia verificada sin caché, configuración de Ventoy e invariantes de discos USB. |

## 3.1. Mapa de `WinSlimUsbCreator/src/segmented.rs`

Contexto medido (2026-11-11): SourceForge sirve este proyecto solo desde `master.dl.sourceforge.net` (todos los espejos redirigen allí, URL firmada que caduca en ~24 h) y limita cada conexión a ~150-300 KB/s; la descarga alternativa (Google Drive) da ~25 MB/s por conexión pero tiene una cuota diaria de descargas («Quota exceeded»).

| Bloque / Símbolos | Descripción y responsabilidad |
| :--- | :--- |
| Constantes | `ALTERNATIVE_CONNECTIONS` (`12`, satura ~50 MB/s), `SOURCEFORGE_CONNECTIONS` (`16` si es la única fuente; no subir: más conexiones solo cargarían un servidor compartido), `SOURCEFORGE_CONNECTIONS_WITH_ALTERNATIVE` (`4`), `SOURCE_GRACE` (`45 s`; ×4 si es la última fuente). |
| `SegmentConfig` | Tramos: `min_split` 1 MiB, `initial_chunk` 4 MiB, `max_chunk` 256 MiB, `target_chunk_seconds` 20 s, `steal_after` 3 s, `max_source_failures` 8. |
| `Source`, `discover_sources`, `sourceforge_endpoints`, `alternative_endpoint`, `probe_range_source`, `direct_download_url` | Descubre en paralelo los espejos de SourceForge (sin repetir servidor) y la alternativa de GitHub (`Latest_Mirror_URL`; enlaces de Drive → `drive.usercontent.google.com/download?...&confirm=t`). Cada fuente se valida con `Range: bytes=0-0`: `206`, `Content-Range` con el tamaño del RSS, sin HTML y `Content-Disposition` con el mismo nombre. Las URL de SourceForge se renuevan (`refresh`) ante 401/403/404/410. |
| `Ranges`, `SavedProgress`, `state_path`, `prepare_part`, `save_progress`, `convert_to_contiguous` | Intervalos completados; progreso persistente en `<iso>.part.ranges` cada 2 s. `prepare_part` adopta un `.part` antiguo (prefijo continuo) y reserva el tamaño completo; `convert_to_contiguous` deja el `.part` en el formato de la descarga de una conexión. |
| `Engine`, `Job`, `next_job`, `finish_job` | Planificador: entrega primero los tramos pendientes de menor posición (el MD5 avanza a la vez); sin pendientes divide por la mitad el tramo con más trabajo (`>= 2 × min_split`) o rescata entero un tramo lento/atascado (antigüedad `>= steal_after`, velocidad efectiva ×4 menor). **Un rescate nunca se vuelve a rescatar** (evita el bloqueo en el que las conexiones ociosas se robaban el último tramo sin descargarlo). |
| `run_worker`, `fetch_range`, `write_all_at` | Una conexión por hilo con `ureq::Agent` propio (keep-alive), escritura posicional en el `.part`, tramo adaptado a su velocidad, reintentos con backoff y descarte de la fuente tras fallos continuados; si se descarta una fuente, las demás abren más conexiones hasta su máximo. |
| `run_hasher`, `download_segmented` | MD5 en orden sobre el prefijo completado; monitor con velocidad en ventana de 5 s, progreso en la UI («· N conexiones»), cancelación y comprobación final del MD5. Si no coincide se borra todo y la alternativa queda desactivada en la sesión (`ALTERNATIVE_DISTRUSTED`). El botón «Descarga alternativa» de la UI no usa este módulo: abre el enlace de Drive en el navegador. |

---

## 4. Contrato entre `main.rs` y `src/platform/{windows,linux}.rs`

Ambos módulos exponen exactamente la misma interfaz `pub(super)` hacia `main.rs`:

| Función `pub(super)` | Implementación Windows (`platform/windows.rs`) | Implementación Linux (`platform/linux.rs`) |
| :--- | :--- | :--- |
| `os_version()` | `RtlGetVersion` (`ntdll`). | `uname -r`. |
| `firmware_security_status()` | Nativo sin elevación: `Tbsi_GetDeviceInfo` (TBS; `TBS_E_TPM_NOT_FOUND` = deshabilitado) y registro `UEFISecureBootEnabled` (`RegGetValueW`; sin clave y `GetFirmwareType == Bios` = deshabilitado). | `/sys/class/tpm/tpm*/device/{enabled,active,temp_deactivated}` y `/sys/firmware/efi/efivars/SecureBoot-*`. |
| `disks()` | Nativo sin elevación (no usar `Get-Disk`/`Get-Partition`: devuelven vacío sin admin en algunos equipos). Abre `\\.\PhysicalDrive0..255` con acceso 0: `IOCTL_STORAGE_QUERY_PROPERTY` (`BusTypeUsb`, modelo, serie), `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` (tamaño; sin medio = se omite), `IOCTL_DISK_GET_DRIVE_LAYOUT_EX` (particiones), `IOCTL_DISK_IS_WRITABLE` + `IOCTL_DISK_GET_DISK_ATTRIBUTES` (solo lectura). Volúmenes con `FindFirstVolumeW` + `IOCTL_STORAGE_GET_DEVICE_NUMBER` + `GetVolumeInformationW`/`GetVolumePathNamesForVolumeNameW`. Excluye discos de Windows y de la partición de sistema (`protected_disks`: `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` sobre `%SystemRoot%` y `HKLM\SYSTEM\Setup\SystemPartition`). | `lsblk --json --bytes` (`type == "disk"`, `tran == "usb"`, excluye discos con `/`, `/boot` o `/boot/efi`). |
| `same_disk(a, b)` | Compara `number`, `size`, `bus_type == "USB"`, `!is_boot`, `!is_system`, `!is_read_only` y `serial_number`. | Compara `device_path`, `size`, `serial_number`, `bus_type == "USB"`, `!is_boot`, `!is_system` y `!is_read_only`. |
| `ventoy_data_location(disk)` | Busca partición `VTOYEFI` (`FAT`/`FAT32`, 8–256 MiB) + partición 1 con letra (`exFAT`/`NTFS`/`FAT32`). Error si parece Ventoy incompleto. | Busca partición `VTOYEFI` (`vfat`, 8–256 MiB) + partición 1 (`exfat`/`ntfs`/`vfat`). Error si parece Ventoy incompleto. |
| `iso_on_selected_disk(path, disk)` | Resuelve el disco físico del volumen de `path` (`GetVolumePathNameW` + extensiones de volumen, válido para carpetas de montaje); respaldo por letra de unidad (admite prefijo `\\?\`). | Compara `MetadataExt::dev()` del archivo con los puntos de montaje del disco. |
| `prepare(...)` | Verifica ISO y disco; si instala/reinstala: valida SHA-256 del ZIP de Ventoy, verifica/extrae en `%LOCALAPPDATA%\WinSlimUsbCreator\ventoy-1.1.17`, ejecuta `Ventoy2Disk_X64.exe VTOYCLI` con `ShellExecuteExW("runas")` (`run_elevated`, espera y lee el código de salida; UAC cancelado = `ERROR_CANCELLED`), valida `cli_done.txt == "0"`, etiqueta `WinSlim USB` (`SetVolumeLabelW`); luego copia/verifica ISO e instala tema. | Verifica ISO, disco y herramientas (`udisksctl`, `pkexec`, `tar`, `mkfs.ntfs`); resuelve `/dev/disk/by-id/usb-*`, extrae tar.gz validado por SHA-256 en caché temporal, parchea `Ventoy2Disk.sh` (`"$@"`), instala con `pkexec`, formatea NTFS si se eligió, monta con `udisksctl`, copia/verifica ISO e instala tema. |
| `publish_iso(from, to)` | `MoveFileW` (falla si el destino ya existe; nunca sobrescribe). | `libc::renameat2(..., RENAME_NOREPLACE)`. |
| `publish_config(from, to)` | `MoveFileExW` con `MOVEFILE_REPLACE_EXISTING \| MOVEFILE_WRITE_THROUGH`. | `fs::rename(from, to)`. |
| `open_uncached(path)` | `OpenOptions` con `FILE_FLAG_NO_BUFFERING`. | `File::open` + `libc::posix_fadvise(..., POSIX_FADV_DONTNEED)`. |
| `fs_free_bytes(root)` | `GetDiskFreeSpaceExW`. | `libc::statvfs` (`f_bavail * f_frsize`). |
| `blurred_backdrop(window)` | Captura cliente Win32 (`BitBlt`/`GetDIBits`), reduce a 1/4 y aplica `imageops::blur`. | Devuelve `None` (Wayland no permite captura sin portal). |
| `size_initial_window(window)` | Ajusta a `SPI_GETWORKAREA` y `GetDpiForSystem()` (máx. `1200×1040`, mín. `680×530`). | Ajusta al tamaño del monitor activo vía winit (`1200×1040` máx., `680×530` mín.). |
| `log_path()` | `%LOCALAPPDATA%\WinSlimUsbCreator\operations.log`. | `$XDG_STATE_HOME` (o `~/.local/state`) + `/winslim-usb-creator/operations.log`. |
| `downloads_dir()` | `%USERPROFILE%\Downloads\WinSlim`. | `xdg-user-dir DOWNLOAD` (fallback `~/Downloads`) + `/WinSlim`. |
| `restart_to_usb()` | `shutdown.exe /r /o /t 0` (Inicio avanzado de Windows). | `systemctl reboot --firmware-setup`. |
| `open_folder` / `open_url` | `explorer.exe` / `ShellExecuteW("open", ...)`. | `xdg-open`. |

---

## 5. Mapa de `WinSlimUsbCreator/ui/main.slint`

- **Componentes internos:**
  - `DiskRow` (`struct`): `{ label, detail, number }`.
  - `ActionButton`: botón con variantes `primary`, `quiet`, `destructive`, `enabled` y popup de ayuda contextual tras 2500 ms de hover (`PopupWindow`).
  - `RoundedSelect` y `FormatSelect`: selector desplegable personalizado para esquema de partición (`MBR` / `GPT`) y sistema de archivos (`exFAT` / `NTFS`).
  - `StepBadge`: indicador numérico o `✓` para los pasos `01`, `02` y `03`.
- **Propiedades clave de `MainWindow`:**
  - Origen ISO (Paso 01): `iso-name`, `iso-size`, `local-file`, `iso-stage` (`0` sin ISO, `1` remota localizada, `2` local cargada, `3` descargada), `server-availability` (`-1` no disponible, `0` comprobando, `1` disponible), `alternative-availability` (`-1` no disponible, `0` sin comprobar, `1` disponible), `alternative-only` y `alternative-mode` (privadas).
  - Unidades USB y formato (Pasos 02 y 03): `disks`, `selected-disk` (`-1` ninguno), `usb-detecting`, `reuse-ventoy`, `gpt` (`false` = MBR por defecto), `ntfs` (`false` = exFAT por defecto).
  - Estado y bloqueo de operaciones: `busy`, `download-active`, `download-cancelling`, `preparing`, `preparation-complete`, `active-step` (`1..4`), `progress` (`0.0..1.0`), `status-text`.
  - Seguridad firmware (barra inferior y modal de éxito): `tpm-status` y `secure-boot-status` (`-2` comprobando, `-1` no se pudo comprobar, `0` deshabilitado, `1` habilitado).
  - Modales:
    - Confirmación de preparación: `confirm-visible`, `confirm-reinstall`, `confirm-disk`.
    - Registro y diagnóstico: `logs-visible`, `log-text`.
    - Éxito y reinicio: `success-visible`, `success-detail`, `success-backdrop`, `restart-instructions`, `restart-message`, `restart-pending`.
    - Confirmación de cierre con operación en curso: `close-confirm-visible`.

---

## 6. Reglas e invariantes críticas (No romper)

1. **Seguridad de discos USB:** nunca formatear, instalar Ventoy ni copiar sin revalidar inmediatamente antes que el disco seleccionado (`confirming` == `selected`) sigue conectado, conserva exactamente la misma identidad (`same_disk`), no es disco de arranque/sistema/solo lectura y su estado de Ventoy coincide con lo confirmado (`usb_action`).
2. **Protección de archivos del usuario:** nunca sobrescribir una ISO existente en el USB (`publish_iso` usa `MoveFileW` / `RENAME_NOREPLACE` y `unused_iso_path` añade sufijos `(2)`, `(3)` al reutilizar Ventoy). Rechazar preparar con borrado si la ISO local reside en el mismo USB seleccionado (`iso_on_selected_disk`).
3. **Atomicidad de temporales:** las descargas usan `<iso>.part` (en la descarga segmentada, con su tabla de tramos `<iso>.part.ranges`); las copias al USB usan `.winslim-copy-<pid>-<nanos>.part`. Nunca publicar un archivo sin haber completado su verificación de tamaño y MD5 (con lectura sin caché en el USB).
4. **Hilos y UI Slint:** toda la lógica de negocio vive en Rust. Nunca bloquear el hilo de la UI con red, disco o procesos externos; desde hilos secundarios actualizar Slint únicamente mediante `ui(weak, ...)` y proteger cada worker con `PanicGuard`.
5. **Disponibilidad e ISOs de SourceForge:** no fijar nombres de ISO ni URLs de espejos en el código. Aceptar solo `WinSlim*.iso` en la raíz del proyecto (ignorando `.rsync-partial/` u otras subcarpetas). En `check_download_available()`, no exigir `Content-Length` ni `Content-Range` obligatorios (algunos espejos válidos usan transferencia chunked o ignoran `Range` devolviendo `200`), pero rechazar siempre `text/html` o tamaños declarados que contradigan el RSS.
6. **Integridad de Ventoy embebido:** los paquetes en `vendor/` se validan por SHA-256 antes de usarse. En Windows, la copia extraída en `%LOCALAPPDATA%` debe verificarse archivo por archivo (`extracted_ventoy_matches`) antes de reutilizarse con elevación UAC. No modificar los archivos de `vendor/` salvo actualización explícita de versión y hash.
7. **Paridad multiplataforma:** cualquier cambio en el flujo o en la interfaz `pub(super)` de `platform` debe mantenerse sincronizado entre `src/platform/windows.rs` y `src/platform/linux.rs`.

---

## 7. Comandos de validación y compilación

Ejecutar desde `WinSlimUsbCreator/`:

```powershell
cargo fmt --check
cargo check
cargo test
```

- **Pruebas opcionales con red o hardware real** (marcadas con `#[ignore]`):
  - `cargo test sourceforge_rss_is_readable -- --ignored`
  - `cargo test sourceforge_supports_resume -- --ignored`
  - `cargo test usb_enumeration_is_readable -- --ignored --nocapture` (lista discos, particiones y detección de Ventoy; solo lectura)
  - `cargo test firmware_status_is_readable -- --ignored --nocapture` (Windows: estado TPM/Secure Boot)
- **Compilación de paquetes de distribución:**
  - Windows (`.exe` en `WinSlimUsbCreator/release/WinSlim-USB-Creator.exe`): `.\Compilar.cmd --no-pause`
  - Linux (`.AppImage` en `WinSlimUsbCreator/release/WinSlim-USB-Creator-x86_64.AppImage`): `bash WinSlimUsbCreator/scripts/build-linux.sh`
- **Advertencia:** la preparación real de un USB formatea unidades físicas; no ejecutarla en pruebas automatizadas sin autorización explícita del usuario.

---

## 8. Historial de cambios relevantes

- **2026-11-11:** la descarga alternativa vuelve a abrir el enlace de Drive en el navegador y su mensaje indica que después se use «Cargar ISO local»; el diálogo de «Cargar ISO local» se abre en la carpeta Descargas y el texto de estado (`status-text`) admite dos líneas (`wrap: word-wrap`).
- **2026-11-11:** `scripts/build-windows.ps1` compila con `rustup.exe run stable-x86_64-pc-windows-msvc cargo ...` en lugar del proxy `.cargo\bin\cargo.exe`: si los proxies de rustup son enlaces simbólicos, Windows PowerShell no puede ejecutarlos («No hay ninguna aplicación asociada con el archivo»).
- **2026-11-11:** nuevo `src/segmented.rs`: descarga segmentada multifuente (SourceForge + alternativa de Drive) con reparto dinámico, escritura posicional, reanudación por tramos y MD5 incremental. Medido en una línea de ~50 MB/s: antes 0,83 MB/s (4 conexiones por lotes a SourceForge); ahora ~50 MB/s con la alternativa disponible y ~5 MB/s solo con SourceForge (16 conexiones). Eliminados `download_range`, `download_iso_parallel`, `probe_mirror` y `choose_download_mirror`.
- **2026-11-11:** corregida la detección de USB en Windows: `Get-Disk`/`Get-Partition` devolvían una lista vacía sin elevación (la app se ejecuta como usuario normal), así que no aparecía ninguna unidad. `disks()`, `firmware_security_status()` e `install_ventoy()` pasan a APIs Win32 nativas y el módulo Windows ya no usa PowerShell (sin consolas visibles; detección en milisegundos). Además: se detectan los USB UAS, se limpian los caracteres de control de los números de serie, `iso_on_selected_disk` resuelve el disco físico real, se reutiliza la verificación MD5 reciente de la ISO al preparar, `copy_and_verify` comprueba el MD5 de origen y la lista de discos ya no muestra huecos de particiones sin formato.
- **2026-10-28:** ampliado y estructurado `AI_MAP.md` con la arquitectura completa del repositorio, desglose de módulos/símbolos de `src/main.rs`, contrato multiplataforma `windows.rs`/`linux.rs`, propiedades/modales de `ui/main.slint` e invariantes de seguridad.
- **2026-10-24:** unificados los lanzadores de compilación en `Compilar.cmd` para Windows; corregida la comprobación de disponibilidad de SourceForge (`check_download_available`) eliminando la petición previa redundante, ajustando timeouts a 4/5 s y aceptando respuestas `200`/`206` sin cabeceras de tamaño estrictas.
