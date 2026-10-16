param(
    [ValidatePattern('^(?:[0-9]{1,3}\.){3}[0-9]{1,3}$')]
    [string] $ReceiverIp = '192.168.88.59',
    # libobs headers (OBS source release) plus the folder containing obsconfig.h.
    [string] $ObsInclude = "work/obs-source/obs-studio-32.2.2/libobs;work/obs-config",
    [string] $JpegInclude = "C:/libjpeg-turbo64/include",
    [string] $JpegLibrary = "C:/libjpeg-turbo64/lib/jpeg-static.lib",
    [switch] $BuildSelfTest
)
# Builds the OBS sender plugin on its own (separate from the SysX package) and
# copies the DLL to dist-package\OBS-plugin\. The receiver IP/port are
# compile-time configuration: ReceiverIp selects the destination IP; the
# source kReceiverPort constant selects the port. No runtime INI is emitted.
$ErrorActionPreference = "Stop"
$receiverAddress = $null
if (-not [System.Net.IPAddress]::TryParse($ReceiverIp, [ref]$receiverAddress) -or
    $receiverAddress.AddressFamily -ne [System.Net.Sockets.AddressFamily]::InterNetwork -or
    $receiverAddress.ToString() -ne $ReceiverIp) {
    throw 'ReceiverIp must be a canonical IPv4 address, for example 192.168.88.59.'
}
$root = Split-Path -Parent $PSScriptRoot
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path -LiteralPath $vswhere)) { throw 'Visual Studio Installer/vswhere.exe is required.' }
$installation = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if ($LASTEXITCODE -ne 0 -or -not $installation) { throw 'Visual Studio x64 C++ tools not found.' }
$vcvars = Join-Path $installation 'VC\Auxiliary\Build\vcvars64.bat'
$cmake = (Get-Command cmake.exe -ErrorAction Stop).Source
foreach ($required in $vcvars, $cmake) {
    if (-not (Test-Path $required)) { throw "Visual Studio 2022 Build Tools component missing: $required" }
}

$source = Join-Path $root "obs-plugin\sysx-canvas-sender"
$build = Join-Path $root "work\obs-build-release"
Push-Location $root
try {
    $configure = "`"$cmake`" -S `"$source`" -B `"$build`" -G `"NMake Makefiles`" -DCMAKE_BUILD_TYPE=Release " +
        "-DOBS_INCLUDE_DIR=`"$ObsInclude`" -DSYSX_RECEIVER_IP=`"$ReceiverIp`" " +
        "-DJPEG_INCLUDE_DIR=`"$JpegInclude`" -DJPEG_LIBRARY=`"$JpegLibrary`""
    $targets = if ($BuildSelfTest) { 'sysx-canvas-sender sysx-sender-selftest' } else { 'sysx-canvas-sender' }
    cmd.exe /c "`"$vcvars`" >nul 2>nul && $configure && `"$cmake`" --build `"$build`" --target $targets"
    if ($LASTEXITCODE -ne 0) { throw "OBS plugin build failed with code $LASTEXITCODE" }
} finally {
    Pop-Location
}

$dll = Join-Path $build "sysx-canvas-sender.dll"
$outDir = Join-Path $root "dist-package\OBS-plugin"
New-Item -ItemType Directory -Force $outDir | Out-Null
Copy-Item $dll (Join-Path $outDir "sysx-canvas-sender.dll") -Force
Copy-Item $dll (Join-Path $source "sysx-canvas-sender.dll") -Force
$pdb = Join-Path $build 'sysx-canvas-sender.pdb'
if (-not (Test-Path -LiteralPath $pdb -PathType Leaf)) { throw "OBS PDB missing: $pdb" }
$symbolDir = Join-Path $root 'dist-symbols\obs'
New-Item -ItemType Directory -Force $symbolDir | Out-Null
Copy-Item -LiteralPath $pdb -Destination $symbolDir -Force
Write-Host "OBS plugin: $(Join-Path $outDir 'sysx-canvas-sender.dll')"
Write-Host "Install: close OBS, copy it to C:\Program Files\obs-studio\obs-plugins\64bit\, start OBS."
