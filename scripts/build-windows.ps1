$ErrorActionPreference = "Stop"

if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne "X64") {
    throw "Build the SysX executable on a Windows x64 runner."
}

$root = Split-Path -Parent $PSScriptRoot

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

Push-Location $root
try {
    Invoke-CheckedNative -Command "pnpm" -CommandArguments @("tauri:build", "--no-bundle", "--config", "src-tauri\tauri.windows.conf.json")
    Invoke-CheckedNative -Command "cargo" -CommandArguments @("test", "--manifest-path", "rust-backend\Cargo.toml")
    Invoke-CheckedNative -Command "cargo" -CommandArguments @("test", "--manifest-path", "rust-ui\Cargo.toml")
    Invoke-CheckedNative -Command "cargo" -CommandArguments @("test", "--manifest-path", "src-tauri\Cargo.toml")
    $builtExe = Join-Path $root "src-tauri\target\release\receiver.exe"
    $sysxExe = Join-Path $root "src-tauri\target\release\SysX.exe"
    if (-not (Test-Path $builtExe)) { throw "Tauri did not produce the desktop executable: $builtExe" }
    Copy-Item -LiteralPath $builtExe -Destination $sysxExe -Force
    & (Join-Path $PSScriptRoot "smoke-windows-udp.ps1")
    & (Join-Path $PSScriptRoot "smoke-windows-app.ps1") -ApplicationPath $sysxExe
} finally {
    Pop-Location
}
