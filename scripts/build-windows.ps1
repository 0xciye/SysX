$ErrorActionPreference = "Stop"

if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne "X64") {
    throw "Build the Receiver installer on a Windows x64 runner."
}

$root = Split-Path -Parent $PSScriptRoot
$python = Join-Path $root ".venv\Scripts\python.exe"
$binaryDir = Join-Path $root "src-tauri\binaries"
$sidecar = Join-Path $binaryDir "receiver-backend-x86_64-pc-windows-msvc.exe"

function Invoke-CheckedNative {
    param(
        [Parameter(Mandatory)] [string] $Command,
        [Parameter(Mandatory)] [string[]] $CommandArguments
    )

    & $Command @CommandArguments
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) {
        throw "$Command exited with code $exitCode"
    }
}

if (-not (Test-Path $python)) {
    Invoke-CheckedNative -Command "py" -CommandArguments @("-3.12", "-m", "venv", (Join-Path $root ".venv"))
}
Invoke-CheckedNative -Command $python -CommandArguments @("-m", "pip", "install", "--upgrade", "pip")
Invoke-CheckedNative -Command $python -CommandArguments @("-m", "pip", "install", "-e", "backend[dev,package]")
Invoke-CheckedNative -Command $python -CommandArguments @("-m", "pytest", "backend\tests")

Push-Location $root
try {
    Invoke-CheckedNative -Command $python -CommandArguments @(
        "-m", "PyInstaller", "--noconfirm", "--clean", "--onefile", "--console", "--name", "receiver-backend",
        "--paths", "backend\src", "--collect-all", "ultralytics", "--collect-all", "torch", "--collect-all", "cv2",
        "backend\entrypoint.py"
    )
} finally {
    Pop-Location
}

New-Item -ItemType Directory -Force -Path $binaryDir | Out-Null
Copy-Item (Join-Path $root "dist\receiver-backend.exe") $sidecar -Force
& (Join-Path $PSScriptRoot "smoke-windows-udp.ps1")
Push-Location $root
try {
    Invoke-CheckedNative -Command "pnpm" -CommandArguments @("tauri:build", "--config", "src-tauri\tauri.windows.conf.json")
    & (Join-Path $PSScriptRoot "smoke-windows-installer.ps1")
} finally {
    Pop-Location
}
