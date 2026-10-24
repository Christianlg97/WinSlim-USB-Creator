# AI_MAP — mapa rápido para asistentes

> **Regla obligatoria:** cualquier asistente que cambie código, scripts, recursos, dependencias, flujos o comportamiento del proyecto debe actualizar este archivo en el mismo cambio. Mantén el documento breve, factual y sincronizado; no copies aquí implementaciones completas.

## Qué es y dónde trabajar

Aplicación de escritorio Rust + Slint que localiza/descarga una ISO de WinSlim, detecta USB, instala o reutiliza Ventoy y copia/verifica la ISO. Soporta Windows 10/11 x64 y Linux x86_64.

- `WinSlimUsbCreator/`: proyecto real de la aplicación.
- `WinSlimUsbCreator/src/main.rs`: estado, callbacks Slint, red/SourceForge/GitHub, descarga segmentada, verificación, copia, logs y pruebas unitarias.
- `WinSlimUsbCreator/src/platform/windows.rs`: discos mediante PowerShell/Windows, elevación Ventoy, montaje y operaciones específicas de Windows.
- `WinSlimUsbCreator/src/platform/linux.rs`: discos con `lsblk`, `udisksctl`, `pkexec`, Ventoy y operaciones Linux.
- `WinSlimUsbCreator/ui/main.slint`: toda la interfaz y sus estados visuales. La lógica funcional debe permanecer en Rust.
- `WinSlimUsbCreator/build.rs`: compila Slint e integra recursos/plataforma.
- `WinSlimUsbCreator/Cargo.toml`: dependencias y perfil release.
- `WinSlimUsbCreator/assets/`: iconos, fuentes y tema Ventoy; `vendor/`: paquetes oficiales de Ventoy integrados.
- `WinSlimUsbCreator/scripts/`: compilación/empaquetado Windows y Linux.
- `Compilar.cmd`: único lanzador CMD; genera el EXE de Windows mediante `scripts/build-windows.ps1`. Admite `--no-pause`.
- `SourceForge_Upload_ISO.cmd`: subida independiente de ISOs a SourceForge.
- `WinSlimUsbCreator/README.md`: comportamiento y guía técnica detallada. `README.md`: descripción general y pipeline de distribución.
- `BugTrack.txt`: histórico/manual de incidencias. No es código ejecutable.

## Flujo principal

1. `main()` crea `MainWindow`, `State` compartido y registra callbacks.
2. Al iniciar: se detectan USB, seguridad firmware y disponibilidad de la ISO en hilos separados.
3. ISO remota: `fetch_latest()` lee el RSS de SourceForge y `parse_latest()` selecciona la ISO raíz más reciente.
4. Disponibilidad: `check_latest_download_available()` hace una única petición parcial mediante `check_download_available()`. Acepta `200` o `206` descargables aunque el espejo omita cabeceras opcionales; rechaza HTML y tamaños explícitos contradictorios. Timeouts: 4 s conexión + 5 s lectura.
5. Descarga: `download_iso*()` elige/compara espejos, reanuda `.part`, usa segmentos si hay MD5 y soporte Range, y verifica tamaño + MD5 antes de publicar el archivo.
6. Alternativa: la release GitHub `Latest_Mirror_URL` contiene un único enlace HTTPS que se abre en el navegador.
7. Preparación: `prepare()` (implementación por plataforma) valida de nuevo disco/ISO, instala o reutiliza Ventoy, copia a temporal, verifica y publica.

## Estados UI importantes (`main.slint`)

- `iso-stage`: `0` ninguna, `1` remota localizada, `2` local, `3` descargada.
- `server-availability`: `-1` no disponible, `0` comprobando, `1` disponible.
- `alternative-availability`: `-1` no disponible, `0` sin comprobar, `1` disponible.
- `busy`, `download-active`, `preparing` bloquean acciones incompatibles.
- Los cambios de UI desde workers deben pasar por `ui(weak, ...)`; no tocar Slint directamente desde un hilo.

## Reglas e invariantes delicadas

- Nunca formatear ni copiar sin revalidar identidad, conexión y estado del disco seleccionado.
- No sustituir una ISO del usuario ni publicar archivos parciales; temporales de descarga usan `.part` y los de USB `.winslim-copy-*.part`.
- Conservar cancelación, recuperación de pánicos (`PanicGuard`) y desbloqueo de `State.busy` en todos los caminos.
- No asumir nombre fijo de ISO: procede del RSS. Solo aceptar `WinSlim*.iso` en la raíz del proyecto.
- La disponibilidad no debe exigir `Content-Length`/`Content-Range`: ciertos espejos válidos usan chunked o ignoran Range. Si declaran tamaño, sí debe coincidir.
- No editar paquetes binarios de `vendor/` ni la ISO raíz salvo petición expresa.
- Mantener paridad Windows/Linux cuando el cambio no sea deliberadamente específico de plataforma.

## Validación rápida

Desde `WinSlimUsbCreator/`:

```powershell
cargo fmt --check
cargo test
cargo check
```

Las pruebas `#[ignore]` requieren red real o hardware y no se ejecutan por defecto. La preparación física de un USB es destructiva: no probarla sin autorización explícita.

## Últimos cambios relevantes

- 2026-10-24: unificados los lanzadores de compilación en `Compilar.cmd`, dedicado a Windows; eliminados `Compilar-WinSlim.cmd` y `Compilar-WinSlim-Windows.cmd`. Linux conserva sus scripts propios dentro de `WinSlimUsbCreator/scripts/`.
- 2026-10-24: corregida la comprobación de disponibilidad de SourceForge. Se eliminó la petición previa redundante para resolver espejo, los timeouts bajaron a 4/5 s y se dejaron de marcar como inválidas respuestas descargables que omiten cabeceras estrictas. Añadidas pruebas de respuestas `200`/`206` flexibles y tamaño contradictorio.
- 2026-10-24: creado este mapa; debe actualizarse con cada cambio futuro.
