```bat
@echo off
setlocal EnableExtensions DisableDelayedExpansion
chcp 65001 >nul
title WinSlim - SourceForge ISO Upload

rem ============================================================
rem RUTA DEL SCRIPT
rem ============================================================

cd /d "%~dp0"

echo.
echo [BOOT] Iniciando WinSlim SourceForge ISO Upload...
echo.

rem ============================================================
rem AUTOELEVACION
rem
rem Se usa CMD /K deliberadamente:
rem aunque el script falle, la consola elevada permanece abierta.
rem ============================================================

if /I "%~1"=="__ADMIN__" goto MAIN

fltmc >nul 2>&1

if not errorlevel 1 goto MAIN

echo [INFO] Se requieren permisos de administrador.
echo [INFO] Solicitando elevacion UAC...
echo.

set "WS_SCRIPT=%~f0"
set "WS_DIR=%~dp0"

powershell.exe -NoProfile -ExecutionPolicy Bypass -Command ^
    "$arg='""' + $env:WS_SCRIPT + '"" __ADMIN__';" ^
    "Start-Process -FilePath $env:ComSpec -ArgumentList '/k',$arg -WorkingDirectory $env:WS_DIR -Verb RunAs"

if errorlevel 1 (
    echo.
    echo ============================================================
    echo [ERROR] No se pudo solicitar elevacion UAC.
    echo ============================================================
    echo.
    pause
)

exit /b 0


rem ============================================================
rem INICIO PRINCIPAL
rem ============================================================

:MAIN

echo [OK] Consola elevada iniciada.
echo.

fltmc >nul 2>&1

if errorlevel 1 (
    echo ============================================================
    echo [ERROR] El script no dispone de permisos de administrador.
    echo ============================================================
    echo.
    pause
    goto END
)

rem ============================================================
rem CONFIGURACION SOURCEFORGE
rem ============================================================

set "SF_USER=christianlg97"
set "SF_HOST=frs.sourceforge.net"
set "SF_PROJECT=winslim11-isos"
set "SF_PATH=/home/frs/project/%SF_PROJECT%/"

rem ============================================================
rem CONFIGURACION CYGWIN
rem ============================================================

set "CYG_ROOT=C:\WSCore\Components\Cygwin64"
set "CYG_BIN=%CYG_ROOT%\bin"

set "CYG_SETUP=%TEMP%\WinSlim_Cygwin_Setup.exe"
set "CYG_CACHE=%TEMP%\WinSlim_Cygwin_Cache"

set "CYG_SETUP_URL=https://cygwin.com/setup-x86_64.exe"
set "CYG_MIRROR=https://mirrors.kernel.org/sourceware/cygwin/"

rem ============================================================
rem CABECERA
rem ============================================================

echo ============================================================
echo              WinSlim - SourceForge ISO Upload
echo ============================================================
echo.
echo Usuario  : %SF_USER%
echo Proyecto : %SF_PROJECT%
echo Servidor : %SF_HOST%
echo Destino  : %SF_PATH%
echo.
echo Cygwin   : %CYG_ROOT%
echo.
echo ============================================================
echo.

rem ============================================================
rem COMPROBAR DEPENDENCIAS
rem ============================================================

echo [INFO] Comprobando dependencias...
echo.

if exist "%CYG_BIN%\rsync.exe" (
    if exist "%CYG_BIN%\ssh.exe" (
        if exist "%CYG_BIN%\cygpath.exe" (
            echo [OK] rsync encontrado.
            echo [OK] OpenSSH encontrado.
            echo [OK] cygpath encontrado.
            echo.
            goto DEPENDENCIES_OK
        )
    )
)

echo [INFO] Faltan dependencias.
echo.
echo Se instalaran automaticamente:
echo.
echo   - Cygwin
echo   - rsync
echo   - OpenSSH
echo.
echo Ruta:
echo   %CYG_ROOT%
echo.

rem ============================================================
rem CREAR DIRECTORIOS
rem ============================================================

if not exist "C:\WSCore" (
    mkdir "C:\WSCore"
)

if not exist "C:\WSCore\Components" (
    mkdir "C:\WSCore\Components"
)

if not exist "%CYG_CACHE%" (
    mkdir "%CYG_CACHE%"
)

rem ============================================================
rem DESCARGAR INSTALADOR DE CYGWIN
rem ============================================================

echo ============================================================
echo Descargando Cygwin
echo ============================================================
echo.

if exist "%CYG_SETUP%" (
    del /f /q "%CYG_SETUP%" >nul 2>&1
)

rem ------------------------------------------------------------
rem METODO 1 - CURL
rem ------------------------------------------------------------

where curl.exe >nul 2>&1

if errorlevel 1 goto DOWNLOAD_POWERSHELL

echo [INFO] Intentando descarga mediante curl...
echo.

curl.exe ^
    -L ^
    --fail ^
    --retry 3 ^
    --retry-delay 2 ^
    --connect-timeout 30 ^
    -o "%CYG_SETUP%" ^
    "%CYG_SETUP_URL%"

if exist "%CYG_SETUP%" goto DOWNLOAD_OK


rem ------------------------------------------------------------
rem METODO 2 - POWERSHELL
rem ------------------------------------------------------------

:DOWNLOAD_POWERSHELL

echo.
echo [INFO] Intentando descarga mediante PowerShell...
echo.

powershell.exe -NoProfile -ExecutionPolicy Bypass -Command ^
    "$ErrorActionPreference='Stop';" ^
    "$ProgressPreference='SilentlyContinue';" ^
    "Invoke-WebRequest -UseBasicParsing -Uri '%CYG_SETUP_URL%' -OutFile '%CYG_SETUP%'"

if not exist "%CYG_SETUP%" (
    echo.
    echo ============================================================
    echo [ERROR] No se pudo descargar Cygwin.
    echo ============================================================
    echo.
    echo URL:
    echo   %CYG_SETUP_URL%
    echo.
    pause
    goto END
)


:DOWNLOAD_OK

echo.
echo [OK] Instalador de Cygwin descargado.
echo.

rem ============================================================
rem COMPROBAR QUE EL INSTALADOR NO ESTE VACIO
rem ============================================================

for %%A in ("%CYG_SETUP%") do set "CYG_SETUP_SIZE=%%~zA"

if "%CYG_SETUP_SIZE%"=="0" (
    echo.
    echo ============================================================
    echo [ERROR] El instalador descargado esta vacio.
    echo ============================================================
    echo.
    pause
    goto END
)

rem ============================================================
rem INSTALAR CYGWIN + RSYNC + OPENSSH
rem ============================================================

echo ============================================================
echo Instalando dependencias
echo ============================================================
echo.
echo Componentes:
echo.
echo   Cygwin
echo   rsync
echo   openssh
echo.
echo Esto solo deberia ser necesario la primera vez.
echo.

"%CYG_SETUP%" ^
    -q ^
    -R "%CYG_ROOT%" ^
    -l "%CYG_CACHE%" ^
    -s "%CYG_MIRROR%" ^
    -O ^
    -P rsync,openssh ^
    -n ^
    -N

set "CYG_INSTALL_RESULT=%ERRORLEVEL%"

echo.
echo [INFO] Cygwin Setup termino con codigo:
echo   %CYG_INSTALL_RESULT%
echo.

rem ============================================================
rem VERIFICAR INSTALACION
rem ============================================================

if not exist "%CYG_BIN%\rsync.exe" (
    echo ============================================================
    echo [ERROR] rsync.exe no fue instalado.
    echo ============================================================
    echo.
    echo Ruta esperada:
    echo   %CYG_BIN%\rsync.exe
    echo.
    echo Log de Cygwin:
    echo   %CYG_ROOT%\var\log\setup.log
    echo.
    pause
    goto END
)

if not exist "%CYG_BIN%\ssh.exe" (
    echo ============================================================
    echo [ERROR] ssh.exe no fue instalado.
    echo ============================================================
    echo.
    echo Ruta esperada:
    echo   %CYG_BIN%\ssh.exe
    echo.
    pause
    goto END
)

if not exist "%CYG_BIN%\cygpath.exe" (
    echo ============================================================
    echo [ERROR] cygpath.exe no fue instalado.
    echo ============================================================
    echo.
    pause
    goto END
)

echo [OK] Cygwin instalado.
echo [OK] rsync instalado.
echo [OK] OpenSSH instalado.
echo [OK] cygpath disponible.
echo.

rem ============================================================
rem LIMPIEZA
rem ============================================================

if exist "%CYG_SETUP%" (
    del /f /q "%CYG_SETUP%" >nul 2>&1
)

if exist "%CYG_CACHE%" (
    rmdir /s /q "%CYG_CACHE%" >nul 2>&1
)


rem ============================================================
rem DEPENDENCIAS DISPONIBLES
rem ============================================================

:DEPENDENCIES_OK

set "PATH=%CYG_BIN%;%PATH%"

echo ============================================================
echo Dependencias disponibles
echo ============================================================
echo.

echo [RSYNC]
"%CYG_BIN%\rsync.exe" --version

echo.
echo [SSH]
"%CYG_BIN%\ssh.exe" -V 2>&1

echo.
echo ============================================================
echo.

rem ============================================================
rem BUSCAR ISOS JUNTO AL SCRIPT
rem ============================================================

dir /b /a-d "%~dp0*.iso" >nul 2>&1

if errorlevel 1 (
    echo ============================================================
    echo [ERROR] No se encontro ninguna ISO.
    echo ============================================================
    echo.
    echo Coloca una ISO en la misma carpeta que este CMD:
    echo.
    echo   %~dp0
    echo.
    pause
    goto END
)

echo ============================================================
echo ISO encontradas
echo ============================================================
echo.

for %%F in ("%~dp0*.iso") do (
    if exist "%%~fF" (
        echo   %%~nxF
    )
)

echo.
echo ============================================================
echo Conexion SourceForge
echo ============================================================
echo.
echo Usuario:
echo   %SF_USER%
echo.
echo Servidor:
echo   %SF_HOST%
echo.
echo Proyecto:
echo   https://sourceforge.net/projects/%SF_PROJECT%/
echo.
echo Destino:
echo   %SF_PATH%
echo.
echo ============================================================
echo.
echo NOTA:
echo.
echo En la primera conexion SSH puede aparecer:
echo.
echo   Are you sure you want to continue connecting?
echo.
echo Escribe:
echo.
echo   yes
echo.
echo Despues introduce tu password de SourceForge si se solicita.
echo.
echo ============================================================
echo.

rem ============================================================
rem SUBIR TODAS LAS ISOS
rem ============================================================

for %%F in ("%~dp0*.iso") do (
    if exist "%%~fF" (
        call :UPLOAD_ISO "%%~fF"

        if errorlevel 1 (
            echo.
            echo ============================================================
            echo [ERROR] La transferencia se ha detenido.
            echo ============================================================
            echo.
            echo Archivo:
            echo   %%~nxF
            echo.
            echo Puedes volver a ejecutar este script.
            echo rsync conservara la transferencia parcial cuando sea
            echo posible y podra continuarla posteriormente.
            echo.
            pause
            goto END
        )
    )
)

echo.
echo ============================================================
echo       TODAS LAS ISOS SE SUBIERON CORRECTAMENTE
echo ============================================================
echo.
echo Proyecto:
echo.
echo   https://sourceforge.net/projects/%SF_PROJECT%/files/
echo.
pause
goto END


rem ============================================================
rem SUBRUTINA DE SUBIDA
rem ============================================================

:UPLOAD_ISO

set "LOCAL_ISO=%~f1"
set "ISO_NAME=%~nx1"

set "ISO_POSIX="
set "CYGPATH_TMP=%TEMP%\WinSlim_CygPath_%RANDOM%_%RANDOM%.tmp"

echo.
echo ============================================================
echo Preparando ISO
echo ============================================================
echo.
echo Archivo:
echo   %ISO_NAME%
echo.
echo Ruta Windows:
echo   %LOCAL_ISO%
echo.

rem ============================================================
rem CONVERTIR RUTA WINDOWS - CYGWIN
rem ============================================================

echo [INFO] Convirtiendo ruta a formato Cygwin...
echo.

"%CYG_BIN%\cygpath.exe" -u "%LOCAL_ISO%" > "%CYGPATH_TMP%" 2>nul

if errorlevel 1 (
    echo ============================================================
    echo [ERROR] cygpath.exe no pudo convertir la ruta.
    echo ============================================================
    echo.
    echo Ruta:
    echo   %LOCAL_ISO%
    echo.
    if exist "%CYGPATH_TMP%" (
        del /f /q "%CYGPATH_TMP%" >nul 2>&1
    )
    exit /b 1
)

if not exist "%CYGPATH_TMP%" (
    echo ============================================================
    echo [ERROR] cygpath no genero salida.
    echo ============================================================
    echo.
    exit /b 1
)

set /p "ISO_POSIX="<"%CYGPATH_TMP%"

del /f /q "%CYGPATH_TMP%" >nul 2>&1

if not defined ISO_POSIX (
    echo ============================================================
    echo [ERROR] La ruta Cygwin obtenida esta vacia.
    echo ============================================================
    echo.
    exit /b 1
)

echo [OK] Ruta convertida.
echo.
echo Ruta Cygwin:
echo   %ISO_POSIX%
echo.

rem ============================================================
rem COMPROBAR ACCESO AL ARCHIVO DESDE CYGWIN
rem ============================================================

"%CYG_BIN%\bash.exe" -lc "test -f '%ISO_POSIX%'"

if errorlevel 1 (
    echo ============================================================
    echo [ERROR] Cygwin no puede acceder a la ISO.
    echo ============================================================
    echo.
    echo Ruta:
    echo   %ISO_POSIX%
    echo.
    exit /b 1
)

echo [OK] ISO accesible desde Cygwin.
echo.

rem ============================================================
rem MOSTRAR DATOS
rem ============================================================

for %%A in ("%LOCAL_ISO%") do set "ISO_SIZE=%%~zA"

echo ============================================================
echo Subiendo ISO a SourceForge
echo ============================================================
echo.
echo Archivo:
echo   %ISO_NAME%
echo.
echo Tamano:
echo   %ISO_SIZE% bytes
echo.
echo Destino:
echo   %SF_USER%@%SF_HOST%:%SF_PATH%
echo.
echo ============================================================
echo.

rem ============================================================
rem RSYNC
rem
rem -a
rem   Archive
rem
rem -v
rem   Verbose
rem
rem -h
rem   Formato de tamanos legible
rem
rem -P
rem   --partial + --progress
rem
rem --append-verify
rem   Reutiliza una transferencia parcial y verifica los datos
rem
rem --protect-args
rem   Mejora el tratamiento de nombres con espacios/caracteres
rem ============================================================

"%CYG_BIN%\rsync.exe" ^
    -avhP ^
    --append-verify ^
    --protect-args ^
    -e "/usr/bin/ssh" ^
    "%ISO_POSIX%" ^
    "%SF_USER%@%SF_HOST%:%SF_PATH%"

set "RSYNC_RESULT=%ERRORLEVEL%"

echo.

if not "%RSYNC_RESULT%"=="0" (
    echo ============================================================
    echo [ERROR] rsync termino con codigo %RSYNC_RESULT%
    echo ============================================================
    echo.
    echo Archivo:
    echo   %ISO_NAME%
    echo.
    exit /b %RSYNC_RESULT%
)

echo ============================================================
echo [OK] ISO subida correctamente
echo ============================================================
echo.
echo Archivo:
echo   %ISO_NAME%
echo.
echo Proyecto:
echo   https://sourceforge.net/projects/%SF_PROJECT%/files/
echo.
echo Descarga directa:
echo   https://downloads.sourceforge.net/project/%SF_PROJECT%/%ISO_NAME%
echo.

exit /b 0


rem ============================================================
rem FINAL
rem ============================================================

:END

echo.
echo ============================================================
echo Fin del proceso.
echo ============================================================
echo.
echo La consola se mantendra abierta.
echo Puedes cerrarla manualmente cuando termines.
echo.

endlocal
```
