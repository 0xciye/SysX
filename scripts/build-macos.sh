#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
machine="$(uname -m)"
case "$machine" in
  arm64) target="aarch64-apple-darwin" ;;
  x86_64) target="x86_64-apple-darwin" ;;
  *) echo "Unsupported macOS architecture: $machine" >&2; exit 1 ;;
esac

python="$root/.venv/bin/python"
if [[ ! -x "$python" ]]; then
  python3 -m venv "$root/.venv"
fi
"$python" -m pip install --upgrade pip
"$python" -m pip install -e "$root/backend[dev,package]"
"$python" -m pytest "$root/backend/tests"

(
  cd "$root"
  "$python" -m PyInstaller --noconfirm --clean --onefile --console --name receiver-backend \
    --paths backend/src --collect-all ultralytics --collect-all torch --collect-all cv2 \
    backend/entrypoint.py
)

binary_dir="$root/src-tauri/binaries"
mkdir -p "$binary_dir"
cp "$root/dist/receiver-backend" "$binary_dir/receiver-backend-$target"
chmod +x "$binary_dir/receiver-backend-$target"
"$binary_dir/receiver-backend-$target" --help >/dev/null

cd "$root"
pnpm tauri:build --bundles app --no-sign --config src-tauri/tauri.macos.conf.json
