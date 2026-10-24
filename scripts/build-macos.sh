#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${root}"
target="$(rustc --print host-tuple)"
profile="release"
sidecar="${root}/src-tauri/binaries/receiver-backend-${target}"
app="${root}/dist/SysX.app"
if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
  target_dir="${CARGO_TARGET_DIR}"
  [[ "${target_dir}" = /* ]] || target_dir="${root}/${target_dir}"
  backend_dir="${target_dir}"
  host_dir="${target_dir}"
else
  backend_dir="${root}/rust-backend/target"
  host_dir="${root}/src-tauri/target"
fi
backend="${backend_dir}/${target}/${profile}/receiver-backend"
host="${host_dir}/${target}/${profile}/receiver"

case "${target}" in
  aarch64-apple-darwin|x86_64-apple-darwin) ;;
  *) printf 'Run this script on macOS (got target %s).\n' "${target}" >&2; exit 1 ;;
esac
command -v cmake >/dev/null || { printf 'CMake is required (for example: brew install cmake).\n' >&2; exit 1; }
if [[ "${target}" == x86_64-apple-darwin ]]; then
  command -v nasm >/dev/null || { printf 'NASM is required for Intel builds (for example: brew install nasm).\n' >&2; exit 1; }
fi

cargo build --manifest-path "${root}/rust-backend/Cargo.toml" \
  --features ort/load-dynamic,coreml --bin receiver-backend --target "${target}" --release
mkdir -p "$(dirname "${sidecar}")"
cp "${backend}" "${sidecar}"
chmod 755 "${sidecar}"
# Ad-hoc sign the backend before the host embeds and hashes it: the extracted
# copy then keeps one stable code identity, so the macOS firewall's allow for
# incoming UDP is remembered across launches (Intel links are unsigned).
codesign --force --sign - "${sidecar}"
cargo build --manifest-path "${root}/src-tauri/Cargo.toml" \
  --target "${target}" --release

rm -rf "${app}"
mkdir -p "${app}/Contents/MacOS" "${app}/Contents/Resources"
cp "${host}" "${app}/Contents/MacOS/SysX"
cp "${root}/scripts/macos/Info.plist" "${app}/Contents/Info.plist"
chmod 755 "${app}/Contents/MacOS/SysX"

# App icon from the canonical bundle PNG (sips and iconutil ship with macOS).
iconset="$(mktemp -d)/SysX.iconset"
mkdir -p "${iconset}"
for size in 16 32 128 256 512; do
  sips -z "${size}" "${size}" "${root}/src-tauri/icons/icon.png" \
    --out "${iconset}/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "${double}" "${double}" "${root}/src-tauri/icons/icon.png" \
    --out "${iconset}/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil --convert icns "${iconset}" --output "${app}/Contents/Resources/SysX.icns"
rm -rf "$(dirname "${iconset}")"

# Ad-hoc signature for the bundle as a whole (not a Developer ID; the app is
# still not notarized, which only matters for copies downloaded elsewhere).
codesign --force --sign - "${app}"
codesign --verify --strict "${app}"
printf 'Built %s for %s\n' "${app}" "${target}"
