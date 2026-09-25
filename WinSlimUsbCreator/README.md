# WinSlim USB Creator

Aplicación de escritorio para Windows 10/11 x64, escrita en Rust. La interfaz usa Slint y una paleta oscura inspirada únicamente en los colores de las capturas adjuntas. No incorpora navegador ni WebView.

El título es **WinSlim USB Creator**. El icono de ventana y ejecutable combina la W de WinSlim con una memoria USB; se puede regenerar con `scripts/create-icon.ps1`.

Los textos de la interfaz usan [Ubuntu Sans v1.006 de Canonical](https://github.com/canonical/Ubuntu-Sans-fonts/releases/tag/v1.006), incluida en `assets/fonts` bajo la [Ubuntu Font Licence](assets/fonts/UBUNTU-FONT-LICENCE.txt). La barra de estado muestra el avance desde la derecha y deja margen con el borde inferior.

## Compilar con un doble clic

Ejecuta [Compilar-WinSlim.cmd](../Compilar-WinSlim.cmd) desde la raíz del repositorio. Comprueba MSVC, Windows SDK y Rust x64; descarga e instala desde Microsoft o Rust lo que falte, y después ejecuta `cargo build --release --locked`. Cargo obtiene automáticamente las dependencias Rust del proyecto. Al terminar, el ejecutable listo para subir a GitHub Releases queda en `WinSlimUsbCreator\release\WinSlim-USB-Creator.exe`. Los recursos y el paquete de Ventoy están integrados en el `.exe`; no hacen falta archivos auxiliares. La carpeta `release` se genera localmente y está excluida de Git. Una instalación nueva de Build Tools puede necesitar permisos de administrador o un reinicio.

Para publicar una versión, sube únicamente ese `.exe` como archivo de GitHub Releases. El repositorio conserva el código, las fuentes, los recursos y el ZIP de Ventoy necesario para volver a compilar. Las ISO, las salidas de Cargo y la carpeta `release` quedan fuera de Git. El ZIP de Ventoy actual no necesita LFS; si en el futuro un archivo imprescindible supera 100 MiB, añade su ruta concreta a `.gitattributes` con Git LFS antes de incorporarlo al repositorio.

## Estado de esta versión

- En el paso 01 permite descargar la última ISO de WinSlim o cargar una ISO local con el selector de archivos de Windows. La ISO local permite continuar sin conexión a SourceForge.
- Consulta el RSS de archivos de SourceForge para elegir la ISO `WinSlim*.iso` con fecha de publicación más reciente. El nombre no está fijado en el código.
- Muestra el nombre y tamaño de la ISO. Descarga en `Descargas\WinSlim`, muestra progreso, velocidad y tiempo estimado, y verifica tamaño y MD5 con los datos publicados por SourceForge. Conserva el archivo `.part` para reanudar la descarga al pulsar de nuevo **Descargar** tras un corte.
- Enumera discos conectados por USB mediante `Get-Disk` en un proceso oculto, con modelo, capacidad, número de disco, letras y sistemas de archivos. Excluye discos marcados por Windows como disco de arranque o sistema.
- Ofrece MBR y exFAT por defecto, además de GPT y NTFS. Comprueba que el mismo disco USB siga conectado antes de instalar.
- Si detecta las particiones de datos y `VTOYEFI` de Ventoy, evita la instalación y el formateo; añade la ISO y configura el tema gráfico WinSlim sin alterar las demás opciones de Ventoy. Si existe un tema personalizado distinto, lo conserva. Si la instalación parece incompleta, se detiene antes de formatear. En una partición Ventoy FAT32, rechaza una ISO de más de 4 GB sin alterar el USB.
- En una instalación nueva pide confirmación explícita antes del borrado, eleva el instalador oficial de Ventoy mediante UAC y comprueba `cli_done.txt`. La partición de datos recibe la etiqueta **WinSlim USB**, válida para exFAT y NTFS.
- Copia la ISO con lectura y escritura simultáneas usando dos bloques reutilizables de 8 MiB. La copia se sincroniza y verifica por tamaño y MD5 antes de publicar el archivo `.iso`; si falla, se descarta el archivo temporal.
- Instala un tema gráfico de Ventoy en `/ventoy/winslim-theme` con el fondo **WinSlim USB Creator** y la firma de la imagen de referencia. El icono USB se dibuja aparte con una copia proporcional de 50 × 50 píxeles del PNG original: Ventoy muestra las imágenes del tema a su tamaño de archivo. La configuración se guarda en `/ventoy/ventoy.json` solo después de copiar la ISO; si el tema no puede guardarse, la ISO preparada sigue siendo válida y se informa en el registro.
- Configura `es_ES` como idioma inicial de los menús de Ventoy y añade una ayuda discreta para `Enter` y la vuelta al menú anterior. En el submenú de la ISO se vuelve con la opción «Regresar al menú anterior».
- Oculta visualmente el texto de versión y web que Ventoy incorpora al menú gráfico, situándolo en una zona negra del tema y usando color negro. El texto sigue formando parte del cargador oficial; eliminarlo de su código requiere recompilar Ventoy.
- Al finalizar muestra un modal con el USB preparado sobre el fondo desenfocado. Permite cerrar el modal, salir de la aplicación o reiniciar en las opciones avanzadas de Windows para elegir el USB. El arranque directo a una unidad concreta depende del firmware y no se fuerza desde la aplicación.
- Escribe un registro en `%LOCALAPPDATA%\WinSlimUsbCreator\operations.log`, accesible desde el botón con icono de documento situado en la esquina superior derecha de la aplicación. Permite verlo en tiempo real, copiarlo, abrir su carpeta, limpiarlo y guardar una copia donde el usuario elija. Ventoy escribe su propio `cli_log.txt` en `%LOCALAPPDATA%\WinSlimUsbCreator\ventoy-1.1.17\ventoy-1.1.17`; la aplicación incorpora su contenido reciente al registro de diagnóstico.

**Límite actual:** el arranque conserva el menú secundario y las funciones de Ventoy. En el cargador oficial incluido, `Esc` está desactivado dentro de ese submenú y el rótulo «Arrancar en modo wimboot» procede del archivo de idioma de la partición de arranque. Cambiar ambos comportamientos requiere modificar el cargador de Ventoy; el tema y `ventoy.json` no disponen de esas opciones. La instalación real en un USB y el arranque BIOS/UEFI tampoco se han validado en hardware. Se necesita esa validación antes de distribuir la aplicación para uso general.

## Compilar y ejecutar

```powershell
cd WinSlimUsbCreator
..\Compilar-WinSlim.cmd --no-pause
.\release\WinSlim-USB-Creator.exe
```

La aplicación incluye el paquete oficial `ventoy-1.1.17-windows.zip`, descargado de la [versión v1.1.17 de Ventoy](https://github.com/ventoy/Ventoy/releases/tag/v1.1.17). Antes de extraerlo se compara con el SHA-256 `D250E97A7595FDAC4F97DEBC630D7A8DA942319274A76CB32384596B659DBAEB`. El ejecutable x64 se copia desde `altexe` al directorio superior, según las instrucciones del propio paquete. La copia local opcional del código fuente de Ventoy está en `../Codigo fuente Ventoy` y no forma parte del repositorio publicado.

Para probar sin tocar ningún USB:

```powershell
cargo test
cargo test sourceforge_rss_is_readable -- --ignored
cargo test usb_enumeration_is_readable -- --ignored
```

La aplicación y la integración con Ventoy se ofrecen bajo GPLv3 o posterior; consulte [COPYING](COPYING) y las licencias de los componentes de Ventoy en el ZIP incluido.
