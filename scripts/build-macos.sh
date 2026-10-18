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
  --features ort/load-dynamic --bin receiver-backend --target "${target}" --release
mkdir -p "$(dirname "${sidecar}")"
cp "${backend}" "${sidecar}"
chmod 755 "${sidecar}"
cargo build --manifest-path "${root}/src-tauri/Cargo.toml" \
  --target "${target}" --release

mkdir -p "${app}/Contents/MacOS"
cp "${host}" "${app}/Contents/MacOS/SysX"
cp "${root}/scripts/macos/Info.plist" "${app}/Contents/Info.plist"
chmod 755 "${app}/Contents/MacOS/SysX"
printf 'Built %s for %s\n' "${app}" "${target}"
