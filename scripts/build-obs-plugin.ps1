param(
    # libobs headers (OBS source release) plus the folder containing obsconfig.h.
    [string] $ObsInclude = "work/obs-source/obs-studio-32.2.2/libobs;work/obs-config",
    # Folder with an obs.lib import library.
    [string] $ObsLibDir = "work/obs-runtime/bin/64bit",
    [string] $JpegInclude = "C:/libjpeg-turbo64/include",
    [string] $JpegLibrary = "C:/libjpeg-turbo64/lib/jpeg-static.lib"
)
# Builds the OBS sender plugin on its own (separate from the SysX package) and
# copies the DLL to dist-package\OBS-plugin\. The receiver IP/port are
# compile-time constants at the top of
# obs-plugin\sysx-canvas-sender\sysx_canvas_sender.cpp (kReceiverHost,
# kReceiverPort); edit them before building for a LAN setup.
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$vcvars = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
$cmake = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe"
foreach ($required in $vcvars, $cmake) {
    if (-not (Test-Path $required)) { throw "Visual Studio 2022 Build Tools component missing: $required" }
}

$source = Join-Path $root "obs-plugin\sysx-canvas-sender"
$build = Join-Path $root "work\obs-build-release"
Push-Location $root
try {
    $configure = "`"$cmake`" -S `"$source`" -B `"$build`" -G `"NMake Makefiles`" -DCMAKE_BUILD_TYPE=Release " +
        "-DOBS_INCLUDE_DIR=`"$ObsInclude`" -DOBS_LIB_DIR=`"$ObsLibDir`" " +
        "-DJPEG_INCLUDE_DIR=`"$JpegInclude`" -DJPEG_LIBRARY=`"$JpegLibrary`""
    cmd.exe /c "`"$vcvars`" >nul 2>nul && $configure && `"$cmake`" --build `"$build`" --target sysx-canvas-sender"
    if ($LASTEXITCODE -ne 0) { throw "OBS plugin build failed with code $LASTEXITCODE" }
} finally {
    Pop-Location
}

$dll = Join-Path $build "sysx-canvas-sender.dll"
$outDir = Join-Path $root "dist-package\OBS-plugin"
New-Item -ItemType Directory -Force $outDir | Out-Null
Copy-Item $dll (Join-Path $outDir "sysx-canvas-sender.dll") -Force
Copy-Item $dll (Join-Path $source "sysx-canvas-sender.dll") -Force
Write-Host "OBS plugin: $(Join-Path $outDir 'sysx-canvas-sender.dll')"
Write-Host "Install: close OBS, copy it to C:\Program Files\obs-studio\obs-plugins\64bit\, start OBS."
