#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
    echo 'Este script debe ejecutarse en Linux o WSL.' >&2
    exit 1
fi
project="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project"

if [[ "$(uname -m)" != "x86_64" ]]; then
    echo 'Se necesita una distribución Linux x86_64 para el AppImage.' >&2
    exit 1
fi
for tool in cargo rustc gcc pkg-config curl sha256sum install mktemp; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "Falta $tool en Linux. Instala Rust y las herramientas de desarrollo de tu distribución." >&2
        exit 1
    fi
done
if [[ ! -f vendor/ventoy-1.1.17-linux.tar.gz ]]; then
    echo 'Falta el paquete oficial de Ventoy para Linux.' >&2
    exit 1
fi
expected='7fb4ed08cef6a6b4d39dd19260d8c80291a78dfdf9af7d461571e23cbbc43805'
actual="$(sha256sum vendor/ventoy-1.1.17-linux.tar.gz | cut -d' ' -f1)"
if [[ "$actual" != "$expected" ]]; then
    echo 'El SHA-256 del paquete Linux de Ventoy no coincide.' >&2
    exit 1
fi

if [[ "${1:-}" == "--check" ]]; then
    exit 0
fi

project_id="$(printf '%s' "$project" | sha256sum | cut -c1-12)"
export CARGO_TARGET_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/winslim-usb-creator/target-$project_id"
mkdir -p "$CARGO_TARGET_DIR"
echo '== Compilando WinSlim USB Creator para Linux x86_64 =='
cargo build --release --locked
binary="$CARGO_TARGET_DIR/release/winslim-usb-creator"
[[ -x "$binary" ]] || { echo 'Cargo no produjo el ejecutable Linux.' >&2; exit 1; }

tool_dir="$CARGO_TARGET_DIR/tools"
mkdir -p "$tool_dir" "$project/release"
linuxdeploy="$tool_dir/linuxdeploy-x86_64.AppImage"
if [[ ! -s "$linuxdeploy" ]]; then
    echo '== Descargando linuxdeploy oficial =='
    curl --fail --location --retry 3 \
        'https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage' \
        --output "$linuxdeploy.tmp"
    chmod +x "$linuxdeploy.tmp"
    mv "$linuxdeploy.tmp" "$linuxdeploy"
fi

stage="$(mktemp -d "$CARGO_TARGET_DIR/appimage.XXXXXX")"
trap 'rm -rf -- "$stage"' EXIT
install -Dm755 "$binary" "$stage/AppDir/usr/bin/winslim-usb-creator"
install -Dm644 packaging/linux/winslim-usb-creator.desktop \
    "$stage/AppDir/usr/share/applications/winslim-usb-creator.desktop"
install -Dm644 assets/icon.png "$stage/AppDir/usr/share/icons/hicolor/256x256/apps/winslim-usb-creator.png"
cd "$stage"
echo '== Creando AppImage =='
APPIMAGE_EXTRACT_AND_RUN=1 "$linuxdeploy" --appdir AppDir --output appimage
shopt -s nullglob
images=( ./*.AppImage )
[[ ${#images[@]} -eq 1 ]] || { echo 'linuxdeploy no generó un único AppImage.' >&2; exit 1; }
install -Dm755 "${images[0]}" "$project/release/WinSlim-USB-Creator-x86_64.AppImage.tmp"
mv -f "$project/release/WinSlim-USB-Creator-x86_64.AppImage.tmp" \
    "$project/release/WinSlim-USB-Creator-x86_64.AppImage"
echo "AppImage para GitHub Releases: $project/release/WinSlim-USB-Creator-x86_64.AppImage"
