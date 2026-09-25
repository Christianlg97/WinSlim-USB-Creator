# WinSlim USB Creator

Aplicación de escritorio para Windows 10/11 x64 y Linux x86_64, escrita en Rust. La interfaz Slint, sus fuentes y sus recursos gráficos son comunes a las dos plataformas. Las operaciones exclusivas de cada sistema están en `src/platform/windows.rs` y `src/platform/linux.rs`. No incorpora navegador ni WebView.

El título es **WinSlim USB Creator**. El icono de ventana y ejecutable combina la W de WinSlim con una memoria USB; se puede regenerar con `scripts/create-icon.ps1`.

Los textos de la interfaz usan [Ubuntu Sans v1.006 de Canonical](https://github.com/canonical/Ubuntu-Sans-fonts/releases/tag/v1.006), incluida en `assets/fonts` bajo la [Ubuntu Font Licence](assets/fonts/UBUNTU-FONT-LICENCE.txt). La barra de estado muestra el avance desde la derecha y deja margen con el borde inferior.

## Compilar con un doble clic

Ejecuta [Compilar-WinSlim.cmd](../Compilar-WinSlim.cmd) desde Windows. Primero genera `WinSlimUsbCreator\release\WinSlim-USB-Creator.exe` con MSVC. Después comprueba si WSL tiene una distribución Linux x86_64 operativa, las herramientas básicas y el paquete de Ventoy; si las tiene, genera también `WinSlimUsbCreator\release\WinSlim-USB-Creator-x86_64.AppImage`. Si WSL no está preparado, omite el AppImage y la compilación de Windows sigue siendo correcta. Para generar solo el `.exe`, utiliza [Compilar-WinSlim-Windows.cmd](../Compilar-WinSlim-Windows.cmd).

En una máquina Linux nativa, ejecuta [scripts/build-linux.sh](scripts/build-linux.sh) para generar únicamente el AppImage. El mismo script se usa dentro de WSL cuando el CMD de Windows detecta que está listo. La carpeta `release` se genera localmente y está excluida de Git.

La construcción Linux necesita Rust/Cargo, GCC, `pkg-config`, `curl`, `sha256sum` y las bibliotecas de desarrollo requeridas por Slint. En Ubuntu, instala previamente Rust mediante rustup y las herramientas base con `sudo apt install build-essential pkg-config curl libfontconfig1-dev libxkbcommon-dev libwayland-dev`. El script Linux verifica el SHA-256 del Ventoy incluido, compila el mismo proyecto Rust con el backend Linux, descarga `linuxdeploy` oficial y empaqueta el binario, icono y metadatos de escritorio en el AppImage.

En ejecución, Linux usa `lsblk` para detectar discos, `udisksctl` para montar la partición de datos y `pkexec` para instalar Ventoy. NTFS requiere también `mkfs.ntfs` de `ntfs-3g`; exFAT es el formato predeterminado. El paquete oficial `ventoy-1.1.17-linux.tar.gz` está integrado en el binario Linux. La interfaz usa el mismo archivo Slint, icono y fuentes que Windows; las decoraciones de ventana, diálogos del sistema y el fondo desenfocado del modal pueden variar según el escritorio y Wayland.

Para publicar una versión, sube el `.exe` y el `.AppImage` como archivos de GitHub Releases. El repositorio conserva el código, las fuentes, los recursos y los paquetes oficiales de Ventoy de ambas plataformas. Las ISO, las salidas de Cargo y la carpeta `release` quedan fuera de Git. Los paquetes de Ventoy actuales no necesitan LFS; si en el futuro un archivo imprescindible supera 100 MiB, añade su ruta concreta a `.gitattributes` con Git LFS antes de incorporarlo al repositorio.

## Estado de esta versión

- En el paso 01 permite descargar la última ISO de WinSlim o cargar una ISO local con el selector de archivos del sistema. La ISO local permite continuar sin conexión a SourceForge.
- Los botones del paso 01 cambian según el estado: al inicio destaca la búsqueda; al localizar una ISO destacan la descarga directa y la alternativa, y buscar de nuevo o cargar un archivo local quedan como opciones secundarias. Con una ISO local o descargada, las acciones de cambio de origen son discretas. Durante la descarga aparece «Cancelar descarga», que conserva la ISO local seleccionada si no se completa el cambio. Con ancho suficiente los cuatro botones de la ISO localizada ocupan una fila; en ventanas estrechas se organizan en dos.
- Consulta el RSS de archivos de SourceForge para elegir la ISO `WinSlim*.iso` con fecha de publicación más reciente. El nombre no está fijado en el código.
- Tras localizar la última ISO, ofrece «Descarga directa de la ISO» en la aplicación y «Descarga alternativa de la ISO» en el navegador predeterminado. La segunda opción consulta al pulsarla las notas de la release [`Latest_Mirror_URL`](https://github.com/Christianlg97/WinSlim_Mirroring/releases/tag/Latest_Mirror_URL) de GitHub y abre el enlace HTTPS publicado allí; el enlace de descarga no está fijado en el código. Esa release debe mantenerse actualizada con la ISO correspondiente, ya que las notas actuales solo contienen un enlace y no permiten comprobar el nombre ni el tamaño del archivo antes de abrirlo.
- Comprueba en segundo plano al abrir la ventana, y de nuevo cada tres minutos cuando está libre, si la ISO más reciente se puede obtener del servidor. El paso 01 muestra un indicador verde o rojo según el resultado y gris mientras se realiza la primera comprobación.
- Antes de iniciar una descarga consulta los espejos que SourceForge ofrece para esa ISO. Si hay varios, compara hasta ocho con una muestra de 512 KiB y usa el que responda más rápido; si solo hay uno o la consulta falla, conserva la selección automática de SourceForge. La velocidad de una muestra breve no garantiza la velocidad de toda la ISO.
- Muestra el nombre y tamaño de la ISO en GiB, calculados con 1024³ bytes por GiB como en las propiedades de Windows. Descarga en `Descargas\WinSlim` con hasta cuatro conexiones y fragmentos de 32 MiB cuando SourceForge proporciona MD5 y el espejo confirma rangos HTTP; en los demás casos continúa con una conexión. Muestra progreso, velocidad y tiempo estimado, y verifica tamaño y MD5 con los datos publicados por SourceForge. Conserva el archivo `.part` para reanudar la descarga tras un corte; al cancelar elimina también los fragmentos temporales.
- Detecta las unidades USB automáticamente al abrir la ventana y permite repetir la búsqueda con «Volver a detectar unidades». En Windows usa `Get-Disk`; en Linux usa `lsblk`. Muestra modelo, capacidad, ubicación y sistema de archivos y excluye los discos del sistema.
- La barra inferior comprueba en segundo plano al iniciar el estado de TPM y Secure Boot. TPM disponible aparece en verde y no disponible en naranja; Secure Boot habilitado aparece en rojo y deshabilitado en verde. Si el sistema impide una lectura fiable, muestra «No se pudo comprobar» en gris. Windows consulta el dispositivo TPM detectado por Plug and Play y el estado de Secure Boot del Registro; Linux consulta `/sys/class/tpm` y la variable UEFI `SecureBoot`.
- Ofrece MBR y exFAT por defecto, además de GPT y NTFS. Comprueba que el mismo disco USB siga conectado antes de instalar.
- Si detecta las particiones de datos y `VTOYEFI` de Ventoy, la acción de copiar evita la instalación y el formateo; añade la ISO y configura el tema gráfico WinSlim sin alterar las demás opciones de Ventoy. Si existe un tema personalizado distinto, lo conserva. Si la instalación parece incompleta, se detiene antes de formatear. En una partición Ventoy FAT32, la acción de copiar rechaza una ISO de más de 4 GB sin alterar el USB.
- En una unidad con Ventoy ofrece dos acciones: copiar la ISO conservando los archivos existentes, o formatear y reinstalar Ventoy con el esquema de particiones y sistema de archivos elegidos. La segunda acción muestra una confirmación de borrado y comprueba de nuevo la identidad y el estado del disco antes de modificarlo.
- En una instalación nueva pide confirmación explícita antes del borrado. Windows eleva Ventoy mediante UAC y comprueba `cli_done.txt`; Linux ejecuta el instalador oficial mediante `pkexec` con una ruta USB estable de `/dev/disk/by-id`. La partición de datos recibe la etiqueta **WinSlim USB**, válida para exFAT y NTFS.
- Copia la ISO con lectura y escritura simultáneas usando dos bloques reutilizables de 8 MiB. La copia se sincroniza y verifica por tamaño y MD5 antes de publicar el archivo `.iso`; si falla, se descarta el archivo temporal.
- Instala un tema gráfico de Ventoy en `/ventoy/winslim-theme` con el fondo **WinSlim USB Creator** y la firma de la imagen de referencia. El icono USB se dibuja aparte con una copia proporcional de 50 × 50 píxeles del PNG original: Ventoy muestra las imágenes del tema a su tamaño de archivo. La configuración se guarda en `/ventoy/ventoy.json` solo después de copiar la ISO; si el tema no puede guardarse, la ISO preparada sigue siendo válida y se informa en el registro.
- Configura `es_ES` como idioma inicial de los menús de Ventoy y muestra «[Enter] Entrar» centrado. En el submenú de la ISO se vuelve con la opción «Regresar al menú anterior».
- Oculta visualmente el texto de versión y web que Ventoy incorpora al menú gráfico, situándolo en una zona negra del tema y usando color negro. El texto sigue formando parte del cargador oficial; eliminarlo de su código requiere recompilar Ventoy.
- Al finalizar muestra un modal con el USB preparado. Permite cerrar el modal, salir de la aplicación o reiniciar para elegir el USB: Windows abre Inicio avanzado y Linux solicita la configuración del firmware mediante `systemctl`. El arranque directo a una unidad concreta depende del firmware y no se fuerza desde la aplicación.
- Escribe un registro en `%LOCALAPPDATA%\WinSlimUsbCreator\operations.log`, accesible desde el botón con icono de documento situado en la esquina superior derecha de la aplicación. Permite verlo en tiempo real, copiarlo, abrir su carpeta, limpiarlo y guardar una copia donde el usuario elija. Ventoy escribe su propio `cli_log.txt` en `%LOCALAPPDATA%\WinSlimUsbCreator\ventoy-1.1.17\ventoy-1.1.17`; la aplicación incorpora su contenido reciente al registro de diagnóstico.

**Límite actual:** el arranque conserva el menú secundario y las funciones de Ventoy. En el cargador oficial incluido, `Esc` está desactivado dentro de ese submenú y el rótulo «Arrancar en modo wimboot» procede del archivo de idioma de la partición de arranque. Cambiar ambos comportamientos requiere modificar el cargador de Ventoy; el tema y `ventoy.json` no disponen de esas opciones. La instalación real en un USB y el arranque BIOS/UEFI tampoco se han validado en hardware. Se necesita esa validación antes de distribuir la aplicación para uso general.

## Compilar y ejecutar

```powershell
cd WinSlimUsbCreator
..\Compilar-WinSlim.cmd --no-pause
.\release\WinSlim-USB-Creator.exe
```

En un escritorio Linux x86_64:

```bash
cd WinSlimUsbCreator
bash scripts/build-linux.sh
chmod +x release/WinSlim-USB-Creator-x86_64.AppImage
./release/WinSlim-USB-Creator-x86_64.AppImage
```

La aplicación incluye los paquetes oficiales `ventoy-1.1.17-windows.zip` y `ventoy-1.1.17-linux.tar.gz`, descargados de la [versión v1.1.17 de Ventoy](https://github.com/ventoy/Ventoy/releases/tag/v1.1.17). Antes de extraerlos se comparan con los SHA-256 oficiales de esa versión. En Windows, el ejecutable x64 se copia desde `altexe` al directorio superior, según las instrucciones del paquete. La copia local opcional del código fuente de Ventoy está en `../Codigo fuente Ventoy` y no forma parte del repositorio publicado.

Para probar sin tocar ningún USB:

```powershell
cargo test
cargo test sourceforge_rss_is_readable -- --ignored
cargo test usb_enumeration_is_readable -- --ignored
```

La aplicación y la integración con Ventoy se ofrecen bajo GPLv3 o posterior; consulte [COPYING](COPYING) y las licencias de los componentes de Ventoy en el ZIP incluido.
