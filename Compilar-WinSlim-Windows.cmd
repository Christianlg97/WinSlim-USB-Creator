@echo off
setlocal
title Compilar WinSlim USB Creator para Windows
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0WinSlimUsbCreator\scripts\build-windows.ps1"
set "BUILD_EXIT=%ERRORLEVEL%"
echo.
if "%BUILD_EXIT%"=="0" (
    echo Compilacion de Windows terminada correctamente.
) else (
    echo La compilacion de Windows fallo. Codigo: %BUILD_EXIT%
)
if /I not "%~1"=="--no-pause" pause
exit /b %BUILD_EXIT%
