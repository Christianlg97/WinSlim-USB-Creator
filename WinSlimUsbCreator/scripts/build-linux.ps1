param([switch]$CheckOnly)

$ErrorActionPreference = 'Stop'
try {
    $project = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
    $wsl = Get-Command wsl.exe -ErrorAction SilentlyContinue
    if (-not $wsl) { throw 'WSL no está instalado. Instala una distribución Linux x64 para generar el AppImage.' }
    if ($CheckOnly) {
        $linuxProject = (& $wsl.Source --exec wslpath -a $project 2>$null)
    } else {
        $linuxProject = (& $wsl.Source --exec wslpath -a $project 2>&1)
    }
    if ($LASTEXITCODE -ne 0 -or -not $linuxProject) {
        throw 'No hay una distribución WSL operativa. Instala Ubuntu en WSL para generar el AppImage.'
    }
    $linuxScript = ($linuxProject | Select-Object -Last 1).Trim().TrimEnd('/') + '/scripts/build-linux.sh'
    if ($CheckOnly) {
        & $wsl.Source --exec /bin/bash $linuxScript --check >$null 2>$null
        if ($LASTEXITCODE -ne 0) { exit 2 }
        exit 0
    }
    & $wsl.Source --exec /bin/bash $linuxScript
    if ($LASTEXITCODE -ne 0) { throw "La construcción del AppImage falló (código $LASTEXITCODE)." }
    exit 0
} catch {
    if ($CheckOnly) { exit 2 }
    Write-Host "ERROR Linux: $($_.Exception.Message)" -ForegroundColor Red
    exit 1
}
