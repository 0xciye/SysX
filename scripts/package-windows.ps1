param(
    # Reuse the existing build output instead of rebuilding first.
    [switch] $SkipBuild,
    # Path to WinRAR's Rar.exe; found automatically in the usual install folders.
    [string] $RarPath
)
# Builds SysX and packs what another Windows PC needs to run it into one
# archive: SysX.exe (backend + web UI embedded), a LAN firewall helper, and
# HUONG-DAN.txt. The OBS plugin DLL is built separately
# (scripts/build-obs-plugin.ps1). The Visual C++ x64 runtime is installed by
# the user.
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

if (-not $SkipBuild) {
    & (Join-Path $PSScriptRoot "build-windows.ps1")
}

$sysx = Join-Path $root "src-tauri\target\release\SysX.exe"
if (-not (Test-Path $sysx)) { throw "SysX.exe not found: $sysx (run without -SkipBuild)" }

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
  2. Microsoft Edge WebView2 Runtime (Windows 11 co san; Windows 10 thuong
     da co qua Edge). Neu thieu: https://go.microsoft.com/fwlink/p/?LinkId=2124703
     Khong co WebView2 thi SysX tu mo receiver bang cua so Edge thay the.

Chay SysX:
  - Chay SysX.exe. Cua so receiver/preview mo ra, trang config tu mo
    (bang Chrome neu co, neu khong thi trinh duyet mac dinh).
  - Dong cua so receiver = thoat SysX. Chi chay duoc 1 SysX cung luc.
  - Cai dat duoc luu o %LOCALAPPDATA%\SysX.
  - SysX nhan frame tu OBS plugin (sysx-canvas-sender.dll, build rieng) qua UDP 4200.

Chay qua LAN (OBS o may khac):
  - Tren may SysX: chuot phai Mo-cong-UDP-4200.bat -> Run as administrator.
  - Dung day LAN gigabit (crop 640 o 240 FPS can ~150-190 Mbit/s).
'@
Set-Content -Path (Join-Path $stage "HUONG-DAN.txt") -Value $guide -Encoding utf8

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
