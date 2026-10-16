param(
    [ValidateSet("StageBackend", "Finalize")]
    [string] $Phase = "StageBackend",
    [string] $ProtectedBackendPath,
    [string] $OutputDirectory = "dist-vmprotect"
)

# VMProtect workflow for the two native images in SysX. The final distribution
# contains one SysX.exe; the backend is protected first, then embedded into the
# host so it is not left as an unprotected sidecar beside the shipped file.
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$target = "x86_64-pc-windows-msvc"
$rawBackend = Join-Path $root "src-tauri\binaries\receiver-backend-$target.exe"
$stage = Join-Path $root $OutputDirectory

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

if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne "X64") {
    throw "VMProtect preparation requires a Windows x64 build machine."
}

New-Item -ItemType Directory -Force $stage | Out-Null

if ($Phase -eq "StageBackend") {
    # This uses the same backend build helper as the normal desktop pipeline,
    # including an optional TensorRT feature when the SDK environment is set.
    Invoke-CheckedNative -Command "node" -CommandArguments @(
        (Join-Path $root "scripts\build-rust-backend.mjs"), "--release"
    )
    if (-not (Test-Path -LiteralPath $rawBackend)) {
        throw "Backend build did not produce $rawBackend"
    }
    $raw = Join-Path $stage "receiver-backend.raw.exe"
    Copy-Item -LiteralPath $rawBackend -Destination $raw -Force
    Write-Host "Backend staged for VMProtect: $raw"
    Write-Host "Protect receiver-backend.raw.exe with VMProtect and save the result as receiver-backend.protected.exe."
    Write-Host "Then run: .\scripts\prepare-vmprotect.ps1 -Phase Finalize -ProtectedBackendPath `"$stage\receiver-backend.protected.exe`""
    exit 0
}

if (-not $ProtectedBackendPath) {
    throw "Finalize requires -ProtectedBackendPath pointing to the VMProtect-protected backend."
}
$protected = (Resolve-Path -LiteralPath $ProtectedBackendPath -ErrorAction Stop).Path
if ([IO.Path]::GetExtension($protected).ToLowerInvariant() -ne ".exe") {
    throw "The protected backend must be a Windows .exe."
}

Invoke-CheckedNative -Command (Join-Path $root "scripts\build-windows.ps1") -CommandArguments @(
    "-UseExistingBackend", "-BackendPath", $protected
)

$built = Join-Path $root "src-tauri\target\release\SysX.exe"
if (-not (Test-Path -LiteralPath $built)) {
    throw "The final native build did not produce $built"
}
$final = Join-Path $stage "SysX.exe"
Copy-Item -LiteralPath $built -Destination $final -Force
$hash = (Get-FileHash -LiteralPath $final -Algorithm SHA256).Hash
Write-Host "VMProtect input: $final"
Write-Host "SHA-256: $hash"
Write-Host "Only SysX.exe is needed in the final one-file distribution."
