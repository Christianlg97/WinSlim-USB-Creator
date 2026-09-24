@echo off
setlocal EnableExtensions EnableDelayedExpansion
chcp 65001 >nul
title WinSlim - Subida de ISO a SourceForge

rem ============================================================
rem CONFIGURACION
rem ============================================================

set "SF_USER=christianlg97"
set "SF_HOST=frs.sourceforge.net"
set "SF_PROJECT=winslim11-isos"
set "SF_PATH=/home/frs/project/%SF_PROJECT%/"

rem ============================================================
rem CABECERA
rem ============================================================

echo.
echo ============================================================
echo              WinSlim - SourceForge ISO Upload
echo ============================================================
echo.
echo Usuario  : %SF_USER%
echo Proyecto : %SF_PROJECT%
echo Servidor : %SF_HOST%
echo Destino  : %SF_PATH%
echo.

rem ============================================================
rem COMPROBAR RSYNC
rem ============================================================

where rsync >nul 2>&1

if errorlevel 1 (
    echo [ERROR] No se encuentra rsync en el sistema.
    echo.
    echo Este script necesita rsync instalado y accesible desde PATH.
    echo.
    echo En Windows puedes instalarlo mediante:
    echo   - Cygwin
    echo   - MSYS2
    echo   - WSL
    echo.
    pause
    exit /b 1
)

rem ============================================================
rem COMPROBAR SSH
rem ============================================================

where ssh >nul 2>&1

if errorlevel 1 (
    echo [ERROR] No se encuentra SSH en el sistema.
    echo.
    echo Instala OpenSSH Client o asegurate de que ssh.exe este
    echo disponible en PATH.
    echo.
    pause
    exit /b 1
)

rem ============================================================
rem BUSCAR ISOS
rem ============================================================

set "ISO_FOUND=0"

for %%F in ("%~dp0*.iso") do (
    if exist "%%~fF" (
        set "ISO_FOUND=1"
    )
)

if "%ISO_FOUND%"=="0" (
    echo [ERROR] No se ha encontrado ninguna ISO junto al script.
    echo.
    echo Carpeta analizada:
    echo %~dp0
    echo.
    pause
    exit /b 1
)

echo ISOs encontradas:
echo.

for %%F in ("%~dp0*.iso") do (
    echo   - %%~nxF
)

echo.
echo ============================================================
echo.

rem ============================================================
rem SUBIR ISOS
rem ============================================================

for %%F in ("%~dp0*.iso") do (

    echo.
    echo ============================================================
    echo Subiendo:
    echo %%~nxF
    echo ============================================================
    echo.

    rem ------------------------------------------------------------
    rem -a  = Archive
    rem -v  = Verbose
    rem -P  = Progress + conservar archivo parcial
    rem --append-verify = continuar una transferencia parcial
    rem                   verificando los datos ya existentes
    rem ------------------------------------------------------------

    rsync -avP --append-verify -e "ssh" ^
        "%%~fF" ^
        "%SF_USER%@%SF_HOST%:%SF_PATH%"

    if errorlevel 1 (
        echo.
        echo ============================================================
        echo [ERROR] La transferencia ha fallado.
        echo ============================================================
        echo.
        echo Archivo:
        echo %%~nxF
        echo.
        echo Puedes ejecutar nuevamente este script.
        echo Rsync intentara continuar la transferencia existente.
        echo.
        pause
        exit /b 1
    )

    echo.
    echo ============================================================
    echo [OK] ISO subida correctamente
    echo ============================================================
    echo.
    echo Archivo:
    echo %%~nxF
    echo.
    echo URL:
    echo https://downloads.sourceforge.net/project/%SF_PROJECT%/%%~nxF
    echo.
)

echo.
echo ============================================================
echo       Todas las ISO se han subido correctamente.
echo ============================================================
echo.
pause
exit /b 0