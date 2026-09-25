param([switch]$CheckOnly)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'Continue'

function Write-Step([string]$Text) { Write-Host "`n== $Text ==" -ForegroundColor Cyan }
function Fail([string]$Text) { throw $Text }

try {
    $project = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
    if (-not [Environment]::Is64BitOperatingSystem) { Fail 'Se necesita Windows x64.' }
    if (-not (Test-Path (Join-Path $project 'Cargo.toml'))) { Fail 'No se encontro Cargo.toml.' }
    if (-not (Test-Path (Join-Path $project 'vendor\ventoy-1.1.17-windows.zip'))) { Fail 'Falta el paquete de Ventoy incluido en el proyecto.' }

    Write-Step 'Comprobando las herramientas de C++ y Windows SDK'
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    $vcPath = $null
    if (Test-Path $vswhere) {
        $vcPath = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    }
    $sdkRoot = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\Include'
    $hasSdk = (Test-Path $sdkRoot) -and @((Get-ChildItem $sdkRoot -Directory -ErrorAction SilentlyContinue)).Count -gt 0
    if (-not $vcPath -or -not $hasSdk) {
        Write-Host 'Faltan Visual Studio Build Tools o Windows SDK. Se instalaran desde Microsoft.' -ForegroundColor Yellow
        $installer = Join-Path $env:TEMP 'winslim-vs-buildtools.exe'
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        Invoke-WebRequest -Uri 'https://aka.ms/vs/17/release/vs_buildtools.exe' -OutFile $installer -UseBasicParsing
        $p = Start-Process -FilePath $installer -ArgumentList @('--quiet','--wait','--norestart','--add','Microsoft.VisualStudio.Workload.VCTools','--includeRecommended') -Verb RunAs -Wait -PassThru
        if ($p.ExitCode -eq 3010) { Fail 'Build Tools se instalo, pero Windows solicita reiniciar. Reinicia y vuelve a ejecutar el CMD.' }
        if ($p.ExitCode -ne 0) { Fail "La instalacion de Build Tools fallo. Codigo: $($p.ExitCode)" }
        if (-not (Test-Path $vswhere)) { Fail 'No se encontro vswhere despues de instalar Build Tools.' }
        $vcPath = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        $hasSdk = (Test-Path $sdkRoot) -and @((Get-ChildItem $sdkRoot -Directory -ErrorAction SilentlyContinue)).Count -gt 0
        if (-not $vcPath -or -not $hasSdk) { Fail 'Build Tools o Windows SDK siguen sin estar disponibles.' }
    }
    Write-Host "MSVC: $vcPath"
    Write-Host "Windows SDK: $sdkRoot"

    Write-Step 'Comprobando Rust'
    $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
    $rustup = Join-Path $cargoBin 'rustup.exe'
    $cargo = Join-Path $cargoBin 'cargo.exe'
    if (-not (Test-Path $rustup) -or -not (Test-Path $cargo)) {
        Write-Host 'Rust no esta instalado. Descargando rustup oficial...' -ForegroundColor Yellow
        $installer = Join-Path $env:TEMP 'winslim-rustup-init.exe'
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        Invoke-WebRequest -Uri 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe' -OutFile $installer -UseBasicParsing
        & $installer -y --default-host x86_64-pc-windows-msvc --default-toolchain stable --profile minimal --no-modify-path
        if ($LASTEXITCODE -ne 0) { Fail "La instalacion de Rust fallo. Codigo: $LASTEXITCODE" }
        if (-not (Test-Path $rustup) -or -not (Test-Path $cargo)) { Fail 'Rust se instalo, pero cargo o rustup no aparecieron en el perfil.' }
    }
    $installed = @(& $rustup toolchain list)
    if (-not ($installed | Where-Object { $_.StartsWith('stable-x86_64-pc-windows-msvc') })) {
        & $rustup toolchain install stable-x86_64-pc-windows-msvc --profile minimal
        if ($LASTEXITCODE -ne 0) { Fail "No se pudo instalar la toolchain de Rust. Codigo: $LASTEXITCODE" }
    }
    Write-Host (& $rustup run stable-x86_64-pc-windows-msvc rustc --version)

    if ($CheckOnly) { Write-Host 'Dependencias disponibles.' -ForegroundColor Green; exit 0 }

    Write-Step 'Compilando WinSlim USB Creator'
    Push-Location $project
    try {
        & $cargo '+stable-x86_64-pc-windows-msvc' build --release --locked
        if ($LASTEXITCODE -ne 0) { Fail "Cargo fallo. Codigo: $LASTEXITCODE" }
    } finally { Pop-Location }
    $result = Join-Path $project 'target\release\winslim-usb-creator.exe'
    if (-not (Test-Path $result)) { Fail 'Cargo termino sin generar el ejecutable esperado.' }
    $releaseDir = Join-Path $project 'release'
    $published = Join-Path $releaseDir 'WinSlim-USB-Creator.exe'
    $staged = Join-Path $releaseDir 'WinSlim-USB-Creator.exe.tmp'
    New-Item -ItemType Directory -Path $releaseDir -Force | Out-Null
    try {
        Copy-Item -LiteralPath $result -Destination $staged -Force
        $sourceHash = (Get-FileHash -LiteralPath $result -Algorithm SHA256).Hash
        $stagedHash = (Get-FileHash -LiteralPath $staged -Algorithm SHA256).Hash
        if ($sourceHash -ne $stagedHash) { Fail 'La copia del ejecutable no coincide con el original.' }
        Move-Item -LiteralPath $staged -Destination $published -Force
    } finally {
        if (Test-Path -LiteralPath $staged) { Remove-Item -LiteralPath $staged -Force }
    }
    Write-Host "`nEjecutable para GitHub Releases: $published" -ForegroundColor Green
    exit 0
} catch {
    Write-Host "`nERROR: $($_.Exception.Message)" -ForegroundColor Red
    exit 1
}
