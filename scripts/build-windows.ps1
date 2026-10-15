param(
    # Launches SysX.exe (opens the monitor and config windows). Off by default.
    [switch] $AppSmoke
)
$ErrorActionPreference = "Stop"

if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne "X64") {
    throw "Build the SysX executable on a Windows x64 machine."
}

$root = Split-Path -Parent $PSScriptRoot
$target = "x86_64-pc-windows-msvc"
$env:Path = (Join-Path $env:USERPROFILE ".cargo\bin") + ";" + $env:Path
# Trunk rejects NO_COLOR=1.
Remove-Item Env:NO_COLOR -ErrorAction SilentlyContinue

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
    # 1. Web UI (monitor + config) -> dist/, embedded by src-tauri/build.rs.
    Invoke-CheckedNative -Command "trunk" -CommandArguments @("build", "--release") -WorkingDirectory (Join-Path $root "rust-ui")

    # 2. Backend sidecar, embedded into SysX.exe.
    Invoke-CheckedNative -Command "cargo" -CommandArguments @("build", "--manifest-path", "rust-backend\Cargo.toml", "--bin", "receiver-backend", "--target", $target, "--release")
    $backend = Join-Path $root "rust-backend\target\$target\release\receiver-backend.exe"
    $sidecar = Join-Path $root "src-tauri\binaries\receiver-backend-$target.exe"
    Copy-Item -LiteralPath $backend -Destination $sidecar -Force

    # 3. Tests.
    Invoke-CheckedNative -Command "cargo" -CommandArguments @("test", "--manifest-path", "rust-backend\Cargo.toml")
    Invoke-CheckedNative -Command "cargo" -CommandArguments @("test", "--manifest-path", "rust-ui\Cargo.toml")
    Invoke-CheckedNative -Command "cargo" -CommandArguments @("test", "--manifest-path", "src-tauri\Cargo.toml")

    # 4. Portable SysX.exe.
    Invoke-CheckedNative -Command "cargo" -CommandArguments @("build", "--manifest-path", "src-tauri\Cargo.toml", "--release")
    $builtExe = Join-Path $root "src-tauri\target\release\receiver.exe"
    $sysxExe = Join-Path $root "src-tauri\target\release\SysX.exe"
    if (-not (Test-Path $builtExe)) { throw "The native host build did not produce $builtExe" }
    Copy-Item -LiteralPath $builtExe -Destination $sysxExe -Force
    Write-Host "Built $sysxExe"

    # 5. Headless protocol smoke; the app smoke opens windows, so it is opt-in.
    & (Join-Path $PSScriptRoot "smoke-windows-udp.ps1")
    if ($AppSmoke) {
        & (Join-Path $PSScriptRoot "smoke-windows-app.ps1") -ApplicationPath $sysxExe
    }
} finally {
    Pop-Location
}
