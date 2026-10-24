param(
    # Launches SysX.exe (opens the native monitor). Off by default.
    [switch] $AppSmoke,
    # Optionally embed an existing backend. No third-party protection tool
    # is run or required by this build.
    [switch] $UseExistingBackend,
    [string] $BackendPath
)
$ErrorActionPreference = "Stop"

if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne "X64") {
    throw "Build the SysX executable on a Windows x64 machine."
}

$root = Split-Path -Parent $PSScriptRoot
$target = "x86_64-pc-windows-msvc"
$sidecar = Join-Path $root "src-tauri\binaries\receiver-backend-$target.exe"
$env:Path = (Join-Path $env:USERPROFILE ".cargo\bin") + ";" + $env:Path
function Invoke-CheckedNative {
    param(
        [Parameter(Mandatory)] [string] $Command,
        [Parameter(Mandatory)] [string[]] $CommandArguments,
        [string] $WorkingDirectory = $root
    )
    Push-Location $WorkingDirectory
    try {
        & $Command @CommandArguments
        $exitCode = $LASTEXITCODE
    } finally {
        Pop-Location
    }
    if ($exitCode -ne 0) {
        throw "$Command $($CommandArguments -join ' ') exited with code $exitCode"
    }
}

Push-Location $root
try {
    # 1. Backend sidecar, embedded into SysX.exe.
    if ($UseExistingBackend) {
        if ($BackendPath) {
            $existing = (Resolve-Path -LiteralPath $BackendPath -ErrorAction Stop).Path
            $destination = [IO.Path]::GetFullPath($sidecar)
            if ([IO.Path]::GetFullPath($existing) -ne $destination) {
                Copy-Item -LiteralPath $existing -Destination $sidecar -Force
            }
        }
        if (-not (Test-Path -LiteralPath $sidecar)) {
            throw "-UseExistingBackend requires a backend at $sidecar or -BackendPath."
        }
        Write-Host "Using existing backend: $sidecar"
    } else {
        # Pixel Seek does not use ONNX Runtime. Keep the retained development
        # adapter dynamically loaded so the portable build needs no ORT SDK.
        Invoke-CheckedNative -Command "cargo" -CommandArguments @("build", "--manifest-path", "rust-backend\Cargo.toml", "--features", "ort/load-dynamic,directml", "--bin", "receiver-backend", "--target", $target, "--release")
        $backend = Join-Path $root "rust-backend\target\$target\release\receiver-backend.exe"
        Copy-Item -LiteralPath $backend -Destination $sidecar -Force
    }

    # 2. Tests.
    Invoke-CheckedNative -Command "cargo" -CommandArguments @("test", "--manifest-path", "rust-backend\Cargo.toml", "--features", "ort/load-dynamic")
    Invoke-CheckedNative -Command "cargo" -CommandArguments @("test", "--manifest-path", "rust-ui\Cargo.toml")
    Invoke-CheckedNative -Command "cargo" -CommandArguments @("test", "--manifest-path", "src-tauri\Cargo.toml")

    # 3. Portable SysX.exe.
    Invoke-CheckedNative -Command "cargo" -CommandArguments @("build", "--manifest-path", "src-tauri\Cargo.toml", "--release")
    $builtExe = Join-Path $root "src-tauri\target\release\receiver.exe"
    $sysxExe = Join-Path $root "src-tauri\target\release\SysX.exe"
    if (-not (Test-Path $builtExe)) { throw "The native host build did not produce $builtExe" }
    Copy-Item -LiteralPath $builtExe -Destination $sysxExe -Force
    Write-Host "Built $sysxExe"

    # 4. Headless protocol smoke; the app smoke opens windows, so it is opt-in.
    & (Join-Path $PSScriptRoot "smoke-windows-udp.ps1")
    if ($AppSmoke) {
        & (Join-Path $PSScriptRoot "smoke-windows-app.ps1") -ApplicationPath $sysxExe
    }
} finally {
    Pop-Location
}
