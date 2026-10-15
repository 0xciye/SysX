#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
pnpm tauri:build --bundles app --no-sign --config src-tauri/tauri.macos.conf.json
