@echo off
setlocal
title Compilar WinSlim USB Creator
set "LINUX_STATUS=omitido"
call "%~dp0Compilar-WinSlim-Windows.cmd" --no-pause
set "BUILD_EXIT=%ERRORLEVEL%"
if not "%BUILD_EXIT%"=="0" goto finish

powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0WinSlimUsbCreator\scripts\build-linux.ps1" -CheckOnly
if errorlevel 1 (
    echo.
    echo WSL no esta listo para compilar el AppImage. Se omite Linux.
    goto finish
)

echo.
echo WSL y las herramientas Linux estan disponibles. Compilando el AppImage...
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0WinSlimUsbCreator\scripts\build-linux.ps1"
set "BUILD_EXIT=%ERRORLEVEL%"
if "%BUILD_EXIT%"=="0" (set "LINUX_STATUS=completado") else (set "LINUX_STATUS=fallido")
:finish
echo.
if "%BUILD_EXIT%"=="0" (
    if "%LINUX_STATUS%"=="completado" (
        echo Compilaciones de Windows y Linux terminadas correctamente.
    ) else (
        echo Compilacion de Windows terminada correctamente. Linux omitido.
    )
) else (
    if "%LINUX_STATUS%"=="fallido" (
        echo Windows se compilo correctamente, pero fallo el AppImage. Codigo: %BUILD_EXIT%
    ) else (
        echo La compilacion de Windows fallo. Codigo: %BUILD_EXIT%
    )
)
if /I not "%~1"=="--no-pause" pause
exit /b %BUILD_EXIT%
