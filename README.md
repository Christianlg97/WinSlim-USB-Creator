# 🪟 WinSlim USB Creator

> La primera versión de la herramienta de escritorio para crear USB está en [WinSlimUsbCreator](WinSlimUsbCreator/README.md). La instalación real en USB y el menú de arranque personalizado siguen pendientes de validación.

<div align="center">

[![Windows 11](https://img.shields.io/badge/Target_OS-Windows_11_x64-0078D4?style=for-the-badge&logo=windows11&logoColor=white)](https://microsoft.com)
[![SourceForge](https://img.shields.io/badge/Hosting-SourceForge_FRS-EE5C24?style=for-the-badge&logo=sourceforge&logoColor=white)](https://sourceforge.net/projects/winslim11-isos/)
[![Rsync](https://img.shields.io/badge/Transfer-Rsync_over_SSH-235555?style=for-the-badge&logo=gnu-bash&logoColor=white)](https://rsync.samba.org/)
[![Ventoy Integration](https://img.shields.io/badge/Ecosystem-Ventoy_Ready-1B73BA?style=for-the-badge&logo=linux&logoColor=white)](https://www.ventoy.net)
[![Automation](https://img.shields.io/badge/Pipeline-Zero--Config_Batch-4EAA25?style=for-the-badge&logo=powershell&logoColor=white)](#análisis-técnico-del-script-sourceforge_upload_isocmd)

<p align="center">
  <b>Infraestructura de almacenamiento, orquestación y despliegue automatizado de imágenes ISO custom de WinSlim11 hacia SourceForge FRS, diseñada para alimentar el ecosistema downstream de creación de medios USB booteables con GRUB personalizado basado en Ventoy.</b>
</p>

[Visión General](#-visión-general) •
[Arquitectura](#-arquitectura-del-sistema) •
[Script de Subida](#-análisis-técnico-del-script-sourceforge_upload_isocmd) •
[Nomenclatura ISO](#-convención-de-nomenclatura-de-isos) •
[Herramienta USB](#-winslim-usb-creator-herramienta-de-escritorio) •
[Guía de Uso](#-guía-de-despliegue-y-uso) •
[Troubleshooting](#-resolución-de-problemas)

---

<img width="1199" height="1062" alt="image" src="https://github.com/user-attachments/assets/2963b9d6-c6ba-4b04-85b0-1c806ea6368e" />


</div>

## 📖 Visión General

**WinSlim11** es una distribución personalizada y profundamente optimizada de **Windows 11**, diseñada para maximizar el rendimiento, eliminar telemetría y bloatware innecesario, preservar la estabilidad del sistema operativo y ofrecer una experiencia ultra fluida tanto para entusiastas como para estaciones de trabajo.

Debido al tamaño de las imágenes completas de instalación (típicamente **~9.3 GB** con drivers, runtimes, perfiles optimizados y paquetes acumulativos integrados), la distribución de estos artefactos no es viable a través de repositorios estándar de Git ni cuotas gratuitas de Git LFS (limitadas a 100 MB / 2 GB por archivo y cuotas reducidas de ancho de banda).

Este repositorio (**`WinSlim11_ISOS`**) cumple una doble función estratégica:
1. **Pipeline de Ingesta y Despliegue:** Alojar la lógica de automatización desatendida ([SourceForge_Upload_ISO.cmd](SourceForge_Upload_ISO.cmd)) que autogestiona el entorno de dependencias POSIX (`Cygwin64`, `rsync`, `OpenSSH`), valida los artefactos y realiza transferencias delta resilientes hacia la red CDN de SourceForge.
2. **Backbone de Enlaces Directos para Distribución Automatizada:** Generar y estandarizar los puntos de enlace directos (`downloads.sourceforge.net/project/...`) que consume la herramienta cliente [**WinSlim USB Creator**](WinSlimUsbCreator/README.md): un creador de USB de instalación basado en **Ventoy** con tema **GRUB personalizado**.

---

## 🏛 Arquitectura del Sistema

El flujo integral comprende desde la generación local de la imagen hasta el despliegue final en el hardware del usuario:

```mermaid
flowchart TD
    subgraph LOCAL["1. Entorno de Compilación y Subida (Este Repo)"]
        ISO["WinSlim11_*.iso\n(~9.3 GB)"]
        CMD["SourceForge_Upload_ISO.cmd"]
        CYG["Bootstrap Automático\nCygwin64 + rsync + ssh\n(C:\\WSCore\\Components)"]
        
        ISO --> CMD
        CMD --> CYG
    end

    subgraph CLOUD["2. Red de Distribución (SourceForge FRS CDN)"]
        FRS["SourceForge Storage Server\nfrs.sourceforge.net\n/home/frs/project/winslim11-isos/"]
        MIRRORS["Servidor de descarga\ndownloads.sourceforge.net\n→ master.dl.sourceforge.net"]
        ALT["Descarga alternativa\nGoogle Drive (enlace en la release\nLatest_Mirror_URL de GitHub)"]
        
        CYG -- "Rsync over SSH\n(-avhP --partial-dir)" --> FRS
        FRS --> MIRRORS
    end

    subgraph APP["3. WinSlim USB Creator (Rust + Slint)"]
        CLIENT["WinSlim USB Creator\n(descarga por tramos + Ventoy 1.1.17)"]
        GRUB["Tema GRUB WinSlim\n+ ventoy.json"]
        TARGET_USB["Unidad Pendrive USB\n[VTOYEFI] + [Partición Datos NTFS/exFAT]"]
        
        MIRRORS -- "Tramos HTTP en paralelo\n(verificación MD5)" --> CLIENT
        ALT -. "Tramos HTTP en paralelo\n(si está disponible)" .-> CLIENT
        CLIENT --> GRUB
        GRUB --> TARGET_USB
        CLIENT -- "Copia de la ISO sin extraer" --> TARGET_USB
    end

    classDef localStyle fill:#1e293b,stroke:#38bdf8,stroke-width:2px,color:#f8fafc;
    classDef cloudStyle fill:#0f172a,stroke:#fb923c,stroke-width:2px,color:#f8fafc;
    classDef appStyle fill:#111827,stroke:#4ade80,stroke-width:2px,color:#f8fafc;

    class LOCAL,ISO,CMD,CYG localStyle;
    class CLOUD,FRS,MIRRORS,ALT cloudStyle;
    class APP,CLIENT,GRUB,TARGET_USB appStyle;
```

---

## ⚙ Análisis Técnico del Script `SourceForge_Upload_ISO.cmd`

El archivo [`SourceForge_Upload_ISO.cmd`](SourceForge_Upload_ISO.cmd) es un script de orquestación de alto nivel escrito en Windows Batch y PowerShell. Su arquitectura está dividida en 7 capas funcionales:

```
┌────────────────────────────────────────────────────────┐
│ 1. Auto-Elevación UAC Inteligente (cmd /k + RunAs)     │
├────────────────────────────────────────────────────────┤
│ 2. Definición de Parámetros y Endpoints SourceForge    │
├────────────────────────────────────────────────────────┤
│ 3. Bootstrap y Aprovisionamiento Desatendido de Cygwin │
├────────────────────────────────────────────────────────┤
│ 4. Detección Local de Artefactos (*.iso)               │
├────────────────────────────────────────────────────────┤
│ 5. Normalización POSIX y Verificación Cruzada (Bash)   │
├────────────────────────────────────────────────────────┤
│ 6. Transferencia Resiliente Rsync / SSH                │
├────────────────────────────────────────────────────────┤
│ 7. Generación y Visualización de Endpoints Directos    │
└────────────────────────────────────────────────────────┘
```

### 1. Auto-Elevación UAC y Persistencia de Diagnóstico
El script garantiza privilegios administrativos mediante el token de filtro del sistema de archivos (`fltmc`). Si el script se ejecuta en un contexto no privilegiado:
- Lanza un subproceso mediante `PowerShell Start-Process` con el verbo `RunAs`.
- Utiliza deliberadamente `ComSpec /k`: esto asegura que si ocurre cualquier error durante la descarga o la conexión SSH, la ventana de consola **no se cerrará automáticamente**, permitiendo auditar la salida y los logs de depuración.

```bat
powershell.exe -NoProfile -ExecutionPolicy Bypass -Command ^
    "$arg='""' + $env:WS_SCRIPT + '"" __ADMIN__';" ^
    "Start-Process -FilePath $env:ComSpec -ArgumentList '/k',$arg -WorkingDirectory $env:WS_DIR -Verb RunAs"
```

### 2. Configuración del Endpoint de Almacenamiento
Define la configuración centralizada de la cuenta y los destinos en SourceForge:
* **`SF_USER`**: `christianlg97`
* **`SF_HOST`**: `frs.sourceforge.net`
* **`SF_PROJECT`**: `winslim11-isos`
* **`SF_PATH`**: `/home/frs/project/winslim11-isos/`

### 3. Bootstrap Autónomo del Entorno POSIX (`Cygwin64`)
Para evitar requerir que el operador configure manualmente herramientas complejas como Git Bash, WSL o clientes SFTP externos, el script incorpora un mecanismo de despliegue portable:
* **Ubicación Base:** `C:\WSCore\Components\Cygwin64`
* **Doble Estrategia de Descarga:**
  1. *Primaria:* Utiliza `curl.exe` nativo de Windows 10/11 con parámetros de resiliencia (`--fail`, `--retry 3`, `--retry-delay 2`, `--connect-timeout 30`).
  2. *Fallback Secundario:* En caso de anomalía, conmuta a `PowerShell Invoke-WebRequest -UseBasicParsing`.
* **Instalación Silenciosa:** Invoca `setup-x86_64.exe` sin interfaz gráfica (`-q`), instalando únicamente los paquetes esenciales: `rsync` y `openssh`.
* **Limpieza Automática:** Elimina los instaladores y carpetas temporales de caché en `%TEMP%`.

### 4. Detección de Archivos y Normalización POSIX
* Localiza todas las imágenes `*.iso` presentes en el directorio del script.
* Convierte las rutas absolutas de Windows (ej. `C:\Users\...\archivo.iso`) a rutas virtuales POSIX (ej. `/cygdrive/c/Users/...`) usando `cygpath.exe -u`.
* Ejecuta una comprobación de existencia directa dentro del entorno Bash (`bash.exe -lc "test -f '<ISO_POSIX>'"`), garantizando que las capas de permisos y rutas sean totalmente válidas antes de iniciar transferencias de red pesadas.

### 5. Motor de Transferencia con Rsync sobre SSH
La subida de archivos de ~9.3 GB a través de redes WAN requiere un protocolo con verificación de bloques y reanudación automática. El script ejecuta:

```bat
"%CYG_BIN%\rsync.exe" ^
    -avhP ^
    --partial-dir=.rsync-partial ^
    --protect-args ^
    -e "/usr/bin/ssh" ^
    "%ISO_POSIX%" ^
    "%SF_USER%@%SF_HOST%:%SF_PATH%"
```

#### Parámetros Clave de Rsync:
| Parámetro | Función | Importancia en WinSlim11 |
| :--- | :--- | :--- |
| `-a` (`--archive`) | Modo archivo recursivo, preserva marcas de tiempo y permisos. | Mantiene los metadatos idénticos al compilado original. |
| `-v` (`--verbose`) | Detalle informativo en pantalla. | Monitoreo en tiempo real del progreso. |
| `-h` (`--human-readable`) | Formato de unidades legibles (MB, GB, KB/s). | Claridad visual del rendimiento de subida. |
| `-P` | Combina `--progress` y `--partial`. | Si la conexión se interrumpe al 80%, el archivo parcial no se descarta. |
| `--partial-dir=.rsync-partial` | **Crucial:** Guarda la transferencia interrumpida en la carpeta oculta `.rsync-partial` del servidor y la usa como base al reanudar; rsync verifica la suma de todo el archivo al terminar. | La ISO solo aparece con su nombre definitivo cuando está completa, así que nunca se publica (ni la descarga la aplicación) una ISO a medias. |
| `--protect-args` | Previene que el shell remoto interprete espacios o caracteres especiales en el nombre. | Compatibilidad garantizada con nombres complejos de compilación. |
| `-e "/usr/bin/ssh"` | Canaliza el flujo a través del cliente OpenSSH de Cygwin. | Cifrado seguro y autenticación estándar por claves SSH o contraseña. |

---

## 🏷 Convención de Nomenclatura de ISOs

Las compilaciones de WinSlim11 siguen una nomenclatura estructurada y determinista. Tomando como referencia la versión DEV actual:

$$\Large \texttt{WinSlim11\_ESx64\_R1.5\_P-2.2.2\_240926\_Rev62\_DEV.iso}$$

```
WinSlim11 _ ESx64 _ R1.5 _ P-2.2.2 _ 240926 _ Rev62 _ DEV .iso
   │         │       │        │        │       │       │
   │         │       │        │        │       │       └── Rama / Canal (DEV / RELEASE)
   │         │       │        │        │       └────────── Revisión interna del build engine
   │         │       │        │        └────────────────── Fecha de compilación (DDMMYY: 24 Sep 2026)
   │         │       │        └─────────────────────────── Perfil de optimización WinSlim (Preset v2.2.2)
   │         │       └──────────────────────────────────── Versión mayor/menor de WinSlim11 (Release 1.5)
   │         └──────────────────────────────────────────── Idioma (Español) y Arquitectura (x64 / AMD64)
   └────────────────────────────────────────────────────── Familia del Sistema Operativo
```

### URLs de Acceso Resultantes

Una vez completada la subida por el script, los enlaces quedan accesibles públicamente:

* **Portal del Repositorio en SourceForge:**
  `https://sourceforge.net/projects/winslim11-isos/files/`
* **Endpoint de Descarga Directa (redirige al servidor de descarga):**
  `https://downloads.sourceforge.net/project/winslim11-isos/<NOMBRE_ARCHIVO_ISO>`
* **Ejemplo para la compilación actual:**
  `https://downloads.sourceforge.net/project/winslim11-isos/WinSlim11_ESx64_R1.5_P-2.2.2_240926_Rev62_DEV.iso`

> [!NOTE]
> El endpoint `downloads.sourceforge.net/project/...` responde con un `302 Found` hacia un espejo de descarga. Los espejos que sirven la ISO varían: el 11/11/2026 solo respondía `master.dl.sourceforge.net` y el 12/11/2026 respondieron ocho. Cada conexión con SourceForge es lenta (de unos cientos de KB/s a ~1 MB/s), así que WinSlim USB Creator descarga por tramos con varias conexiones y espejos a la vez y, si está disponible, suma la descarga alternativa.
>
> Una ISO recién subida aparece en el RSS unos minutos antes de poder descargarse: mientras tanto, su URL de descarga responde `404`.

---

## 🚀 WinSlim USB Creator: Herramienta de Escritorio

Los enlaces que publica este repositorio los consume [**WinSlim USB Creator**](WinSlimUsbCreator/README.md), una aplicación de escritorio nativa (Rust + Slint, sin navegador integrado) para **Windows 10/11 x64** y **Linux x86_64** que prepara un USB de instalación de WinSlim11 en tres pasos. El código está en [`WinSlimUsbCreator/`](WinSlimUsbCreator/) y el detalle técnico completo en su [README](WinSlimUsbCreator/README.md).

### Qué hace la aplicación

1. **Obtener la ISO (paso 01):**
   - Consulta el RSS de SourceForge y elige la ISO `WinSlim*.iso` de la raíz del proyecto con la fecha de publicación más reciente. El nombre no está fijado en el código y siempre se propone la última versión publicada.
   - **Descarga directa:** descarga la ISO dentro de la aplicación por tramos y desde varias fuentes a la vez: SourceForge y, si está disponible, la descarga alternativa de Google Drive publicada en la release [`Latest_Mirror_URL`](https://github.com/Christianlg97/WinSlim_Mirroring/releases/tag/Latest_Mirror_URL). Usa hasta 16 conexiones simultáneas, se reanuda tras un corte y solo acepta la ISO si su **MD5** coincide con el que publica SourceForge.
   - **Descarga alternativa:** abre el enlace de Google Drive en el navegador.
   - **ISO local:** permite elegir una imagen ya descargada y continuar sin conexión.
   - Indica si la ISO está disponible en el servidor principal, solo en la descarga alternativa o en ninguno.
2. **Elegir el USB (paso 02):** detecta las unidades USB sin permisos de administrador y excluye los discos del sistema.
3. **Preparar el USB (paso 03):**
   - Instala el paquete **oficial de Ventoy 1.1.17** incluido en la aplicación (comprobado por SHA-256 antes de usarlo), con el esquema de particiones (MBR o GPT) y el sistema de archivos (exFAT o NTFS) elegidos: partición de arranque `VTOYEFI` y partición de datos con la etiqueta **WinSlim USB**. Pide confirmar el borrado y vuelve a comprobar la identidad del disco justo antes de modificarlo.
   - Si el USB ya tiene Ventoy, puede copiar la ISO sin formatear ni borrar los demás archivos.
   - Copia la ISO tal cual, sin extraer su contenido, y la verifica por MD5 releyéndola del USB sin la caché del sistema, lo que detecta pendrives defectuosos o de capacidad falsa.
   - Instala el tema GRUB **WinSlim** en `/ventoy/winslim-theme` y configura `ventoy/ventoy.json` con el menú gráfico, el idioma `es_ES` y el texto de versión de Ventoy oculto. Conserva el resto de la configuración existente y no sustituye un tema personalizado distinto.
4. **Al terminar:** muestra el estado de TPM y Secure Boot, avisa de desactivar Secure Boot para evitar «Security Violation [0x0]» y permite reiniciar hacia el menú de arranque (Inicio avanzado en Windows, configuración del firmware en Linux).

```mermaid
sequenceDiagram
    autonumber
    actor User as Usuario
    participant App as WinSlim USB Creator
    participant SF as SourceForge
    participant ALT as Descarga alternativa (Google Drive)
    participant Disk as Unidad USB

    App->>SF: Consulta el RSS (última WinSlim*.iso y su MD5)
    App->>SF: Comprueba que la ISO se puede descargar
    User->>App: Descarga directa de la ISO
    par Tramos en paralelo
        App->>SF: Peticiones Range
    and
        App->>ALT: Peticiones Range (si está disponible)
    end
    App->>App: Verifica tamaño y MD5
    User->>App: Elige USB, esquema de partición y sistema de archivos
    App->>Disk: Instala Ventoy 1.1.17 o reutiliza el existente
    App->>Disk: Copia la ISO y la verifica por MD5 sin caché
    App->>Disk: Tema GRUB WinSlim y ventoy.json
    Disk-->>User: USB listo para arrancar e instalar
```

### Pendiente del plan original

Estas ideas del planteamiento inicial **no están implementadas**:
- Elegir edición o canal (estable, DEV, perfiles concretos) en lugar de la última ISO publicada.
- Configurar en `ventoy.json` la omisión de los requisitos de Windows 11 (TPM 2.0, Secure Boot, 4 GB de RAM y cuenta Microsoft).
- Alias legibles para las ISOs en el menú de arranque y opciones de instalación desatendida.
- Validar la instalación real en USB y el arranque BIOS/UEFI en hardware (ver el límite actual en el [README de la aplicación](WinSlimUsbCreator/README.md)).

---

## 🛠 Guía de Despliegue y Uso

### Prerrequisitos
* Sistema Operativo: **Windows 10 / Windows 11** (x64).
* Conexión a Internet activa (para descargar Cygwin en la primera ejecución y para la subida).
* Espacio en disco:
  * ~300 MB libres en `C:\` para la instalación de Cygwin en `C:\WSCore\Components\Cygwin64`.
* Cuenta en SourceForge con permisos de escritura en el proyecto `winslim11-isos`.
* *(Recomendado)* Llave pública SSH agregada en el perfil de SourceForge (`Account Settings > SSH Keys`), para evitar ingresar la contraseña en cada subida.

### Procedimiento de Subida Paso a Paso

1. **Colocar la ISO:**
   Copia o mueve la imagen ISO compilada (ej. `WinSlim11_ESx64_R1.5_P-2.2.2_240926_Rev62_DEV.iso`) dentro del mismo directorio donde reside [`SourceForge_Upload_ISO.cmd`](SourceForge_Upload_ISO.cmd).

2. **Ejecutar el Script:**
   Haz doble clic sobre [`SourceForge_Upload_ISO.cmd`](SourceForge_Upload_ISO.cmd) o ejecútalo desde un terminal:
   ```cmd
   .\SourceForge_Upload_ISO.cmd
   ```

3. **Concesión UAC:**
   Acepta la ventana del Control de Cuentas de Usuario (UAC) para permitir la elevación.

4. **Instalación Inicial de Dependencias (Solo primera vez):**
   Si Cygwin64 no se encuentra instalado en `C:\WSCore`, el script descargará el instalador oficial, aprovisionará `rsync` y `openssh`, y verificará los binarios de manera totalmente desatendida.

5. **Aceptación de Clave SSH:**
   En la primera conexión a `frs.sourceforge.net`, OpenSSH preguntará:
   ```text
   The authenticity of host 'frs.sourceforge.net (...)' can't be established.
   Are you sure you want to continue connecting (yes/no/[fingerprint])?
   ```
   Escribe **`yes`** y presiona <kbd>Enter</kbd>.

6. **Autenticación:**
   Introduce tu contraseña de SourceForge (o tu passphrase de clave SSH si está protegida).

7. **Monitoreo y Finalización:**
   Rsync mostrará una barra de progreso interactiva con porcentaje, velocidad en MB/s y tiempo estimado. Al finalizar, se imprimirán los enlaces de descarga directa generados.

---

## 🔍 Resolución de Problemas

> [!NOTE]
> La consola se mantiene deliberadamente abierta al finalizar (`CMD /K` y `pause`) para asegurar que puedas examinar los códigos de retorno en caso de anomalías.

### 1. `Host key verification failed`
* **Causa:** La firma del servidor de SourceForge cambió o existe una entrada corrupta en el archivo `known_hosts`.
* **Solución:** Abre la terminal de Cygwin en `C:\WSCore\Components\Cygwin64\bin` y ejecuta:
  ```bash
  ssh-keygen -R frs.sourceforge.net
  ```

### 2. `Connection refused` o `Connection timed out` en el puerto 22
* **Causa:** El cortafuegos de tu red o router bloquea conexiones salientes por el puerto SSH estándar (22).
* **Solución:** Comprueba que el puerto 22 saliente TCP esté habilitado en Windows Defender Firewall y en tu router.

### 3. Transferencia interrumpida a mitad de camino
* **Causa:** Microcortes de ISP, cierre accidental o suspensión del equipo.
* **Solución:** Simplemente vuelve a ejecutar el script. Gracias a `-P` y `--partial-dir=.rsync-partial`, `rsync` reutiliza lo ya subido y solo envía lo que falta. Al reanudar, antes de continuar compara por bloques la parte guardada en el servidor, así que puede tardar unos minutos en reanudar el progreso.

### 4. `No se encontró ninguna ISO`
* **Causa:** El archivo `.iso` no se encuentra en la misma carpeta que el script `SourceForge_Upload_ISO.cmd`.
* **Solución:** Asegúrate de que el archivo tenga la extensión `.iso` (no `.iso.tmp` ni nombres ocultos) y esté en la raíz del repositorio.

---

## 📁 Estructura del Repositorio

```text
WinSlim11_ISOS/
├── .gitattributes               # Finales de línea y políticas Git LFS de resguardo
├── .gitignore                   # Exclusión estricta de binarios *.iso de Git
├── README.md                    # Documentación maestra y especificaciones técnicas
├── BugTrack.txt                 # Mejoras y errores pendientes de la herramienta USB
├── SourceForge_Upload_ISO.cmd   # Orquestador automatizado de subidas a SourceForge FRS
├── Compilar.cmd                 # Único lanzador CMD; compila el .exe de Windows
├── WinSlimUsbCreator/           # Código fuente de WinSlim USB Creator (Rust + Slint)
└── WinSlim11_*.iso              # Imágenes ISO compiladas (almacenadas localmente / ignoradas en Git)
```

### Política de Exclusiones Git
* [`.gitignore`](.gitignore): Contiene la regla `*.iso` para proteger el repositorio de commits accidentales que superen el límite estricto de 100 MB de GitHub.
* [`.gitattributes`](.gitattributes): Define reglas Git LFS para futuros recursos binarios imprescindibles; las ISO locales permanecen ignoradas.

---

## 👤 Autor y Licencias

* **Creador y Desarrollador:** Christian González ([@Christianlg97](https://github.com/Christianlg97))
* **Proyecto SourceForge:** [winslim11-isos](https://sourceforge.net/projects/winslim11-isos/)
* **Tecnologías Asociadas:** Windows 11, Cygwin Project, Rsync (GNU GPL), OpenSSH (BSD), Ventoy (GPLv3).

---
<div align="center">
  <sub>Diseñado con precisión para el ecosistema WinSlim11 • 2026</sub>
</div>
