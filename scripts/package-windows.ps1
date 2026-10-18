param(
    # Reuse the existing build output instead of rebuilding first.
    [switch] $SkipBuild,
    # Path to WinRAR's Rar.exe; found automatically in the usual install folders.
    [string] $RarPath,
    # Optional existing executable. VMProtect is a separate user-managed step.
    # When supplied, this file is copied into the package as SysX.exe.
    [string] $ExecutablePath
)
# Builds SysX and packs what another Windows PC needs to run it into one
# archive: SysX.exe (backend + native Settings embedded), a LAN firewall helper, and
# HUONG-DAN.txt. The OBS plugin DLL is built separately
# (scripts/build-obs-plugin.ps1). The Visual C++ x64 runtime is installed by
# the user.
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

if ($ExecutablePath -and -not $SkipBuild) {
    throw "-ExecutablePath is for an existing final executable; pass -SkipBuild with it."
}
if (-not $SkipBuild) {
    & (Join-Path $PSScriptRoot "build-windows.ps1")
}

$sysx = if ($ExecutablePath) {
    (Resolve-Path -LiteralPath $ExecutablePath -ErrorAction Stop).Path
} else {
    Join-Path $root "src-tauri\target\release\SysX.exe"
}
if (-not (Test-Path -LiteralPath $sysx)) {
    throw "SysX.exe not found: $sysx (run without -SkipBuild or pass -ExecutablePath)"
}

function Assert-PeX64 {
    param([Parameter(Mandatory)] [string] $Path)
    $bytes = [IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 0x40 -or $bytes[0] -ne 0x4D -or $bytes[1] -ne 0x5A) {
        throw "The selected executable is not a valid PE image: $Path"
    }
    $peOffset = [BitConverter]::ToInt32($bytes, 0x3C)
    if ($peOffset -lt 0 -or $peOffset + 6 -gt $bytes.Length) {
        throw "The selected executable has an invalid PE header: $Path"
    }
    if ($bytes[$peOffset] -ne 0x50 -or $bytes[$peOffset + 1] -ne 0x45 -or
        $bytes[$peOffset + 2] -ne 0 -or $bytes[$peOffset + 3] -ne 0) {
        throw "The selected executable is missing the PE signature: $Path"
    }
    $machine = [BitConverter]::ToUInt16($bytes, $peOffset + 4)
    if ($machine -ne 0x8664) {
        throw "The selected executable is not x86-64 PE (machine 0x$('{0:X4}' -f $machine)): $Path"
    }
}

Assert-PeX64 -Path $sysx
$stamp = Get-Date -Format "yyyyMMdd-HHmm"
$outDir = Join-Path $root "dist-package"
$stage = Join-Path $outDir "SysX"
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force $stage | Out-Null
Copy-Item $sysx (Join-Path $stage "SysX.exe")

$firewall = @'
@echo off
rem Allow SysX to receive OBS frames from another computer on the LAN (UDP 4200).
rem Right-click -> Run as administrator, on the computer that runs SysX.
netsh advfirewall firewall add rule name="SysX UDP 4200" dir=in action=allow protocol=UDP localport=4200 profile=private
pause
'@
Set-Content -Path (Join-Path $stage "Mo-cong-UDP-4200.bat") -Value $firewall -Encoding ascii

$guide = @'
SysX - huong dan cai dat tren may khac
=====================================

Yeu cau (nguoi dung tu cai):
  1. Microsoft Visual C++ Redistributable 2015-2022 (x64):
     https://aka.ms/vs/17/release/vc_redist.x64.exe
  Receiver and Settings use native Slint; no browser or WebView runtime is required.

Chay SysX:
  - Chay SysX.exe. Cua so Preview va Settings native mo trong ung dung.
  - Dung Settings trong cua so app de chinh va Apply Changes.
  - Dong cua so app de thoat SysX. Chi chay duoc 1 SysX cung luc.
  - Cai dat duoc luu o %LOCALAPPDATA%\SysX.
  - SysX nhan frame tu OBS plugin (sysx-canvas-sender.dll, build rieng) qua UDP 4200.

Chay qua LAN (OBS o may khac):
  - Tren may SysX: chuot phai Mo-cong-UDP-4200.bat -> Run as administrator.
  - Dung day LAN gigabit (crop 640 o 240 FPS can ~150-190 Mbit/s).
'@
Set-Content -Path (Join-Path $stage "HUONG-DAN.txt") -Value $guide -Encoding utf8

$allowedNames = @("SysX.exe", "Mo-cong-UDP-4200.bat", "HUONG-DAN.txt")
$unexpected = @(Get-ChildItem -LiteralPath $stage -Recurse -File | Where-Object {
    $allowedNames -notcontains $_.Name
})
if ($unexpected.Count -gt 0) {
    throw "Release package contains unexpected files: $($unexpected.FullName -join ', ')"
}
if (@(Get-ChildItem -LiteralPath $stage -Recurse -File | Where-Object {
    $_.Extension -in @('.pdb', '.map', '.rs') -or $_.Name -like 'receiver-backend*.exe'
}).Count -gt 0) {
    throw "Release package contains debug/source/backend artifacts."
}
Write-Host "Package PE/layout audit passed: x86-64 SysX.exe and no sidecar/debug/source artifacts."

# Archive: WinRAR's Rar.exe (RAR is a proprietary format; 7-Zip cannot write it).
if (-not $RarPath) {
    $RarPath = @("$env:ProgramFiles\WinRAR\Rar.exe", "${env:ProgramFiles(x86)}\WinRAR\Rar.exe") |
        Where-Object { Test-Path $_ } | Select-Object -First 1
}
if ($RarPath -and (Test-Path $RarPath)) {
    $archive = Join-Path $outDir "SysX-$stamp.rar"
    Push-Location $outDir
    try {
        & $RarPath a -r -ep1 -m5 -idq $archive "SysX\*"
        if ($LASTEXITCODE -ne 0) { throw "Rar.exe exited with code $LASTEXITCODE" }
    } finally { Pop-Location }
} else {
    $archive = Join-Path $outDir "SysX-$stamp.zip"
    Compress-Archive -Path $stage -DestinationPath $archive -Force
    Write-Warning "WinRAR (Rar.exe) not found: created a .zip instead. Install WinRAR or pass -RarPath to get a .rar."
}
Write-Host "Package: $archive"
Get-ChildItem $stage -Recurse -File | ForEach-Object { "  {0,10:N0}  {1}" -f $_.Length, $_.FullName.Substring($stage.Length + 1) }
