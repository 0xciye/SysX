# Shared by build-full.ps1, build-sysx.ps1 and build-dll.ps1. Everything they
# produce goes to <repo>\build:
#   build\SysX\SysX.exe                      the app (models embedded)
#   build\SysX\*.dll                         runtime libraries SysX loads beside the exe
#   build\OBS-plugin\sysx-canvas-sender.dll  the OBS sender plugin
#   build\SysX-<date>.zip                    build-full.ps1 only (both folders)
$ErrorActionPreference = "Stop"
$script:Root = Split-Path -Parent $PSScriptRoot
$script:Target = "x86_64-pc-windows-msvc"
$script:BuildDir = Join-Path $Root "build"
$script:Stage = Join-Path $BuildDir "SysX"
$script:PluginStage = Join-Path $BuildDir "OBS-plugin"
$env:Path = (Join-Path $env:USERPROFILE ".cargo\bin") + ";" + $env:Path

# Every Runtime/Device in Settings needs these, whether or not the build
# machine has the hardware: ONNX Runtime + DirectML, and OpenVINO with its
# CPU/GPU/NPU plugins and the IR and ONNX readers (one OpenVINO release).
$script:RequiredDlls = @(
    "onnxruntime.dll", "onnxruntime_providers_shared.dll", "DirectML.dll",
    "openvino.dll", "openvino_c.dll", "openvino_ir_frontend.dll", "openvino_onnx_frontend.dll",
    "openvino_intel_cpu_plugin.dll", "openvino_intel_gpu_plugin.dll", "openvino_intel_npu_plugin.dll",
    "tbb12.dll"
)

function Invoke-Checked {
    param([Parameter(Mandatory)] [string] $Command, [Parameter(Mandatory)] [string[]] $Arguments)
    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Command $($Arguments -join ' ') exited with code $LASTEXITCODE"
    }
}

function Test-TensorRtBuild {
    [bool](($env:TENSORRT_PATH -or $env:TENSORRT_INCLUDE_DIR) -and $env:CUDA_PATH)
}

# The DLL folder: -DependenciesDir, else $env:SYSX_ORT_DIR, else <repo>\runtime-deps.
function Resolve-DependenciesDir([string] $Dir) {
    if (-not $Dir) { $Dir = $env:SYSX_ORT_DIR }
    if (-not $Dir) { $Dir = Join-Path $Root "runtime-deps" }
    if (-not (Test-Path -LiteralPath $Dir -PathType Container)) {
        throw "DLL folder not found: $Dir (pass -DependenciesDir or set SYSX_ORT_DIR)."
    }
    (Resolve-Path -LiteralPath $Dir).Path
}

# The DLLs SysX loads from $Dir, checked for completeness and versions.
function Get-RuntimeDlls([string] $Dir) {
    $tensorRt = Test-TensorRtBuild
    $all = @(Get-ChildItem -LiteralPath $Dir -Filter *.dll -File)
    $wanted = @($all | Where-Object {
        $_.Name -like "onnxruntime*.dll" -or $_.Name -eq "DirectML.dll" -or
        $_.Name -like "openvino*.dll" -or $_.Name -like "tbb*.dll" -or
        ($tensorRt -and $_.Name -like "nvinfer*.dll")
    })
    $missing = @($RequiredDlls | Where-Object { $wanted.Name -notcontains $_ })
    if ($missing.Count -gt 0) { throw "Missing from ${Dir}: $($missing -join ', ')" }
    # The NPU compiler DLLs carry their own version inside an OpenVINO release.
    $ovVersions = @($wanted | Where-Object { $_.Name -like "openvino*.dll" -and $RequiredDlls -contains $_.Name } |
        ForEach-Object { $_.VersionInfo.ProductVersion -replace '^(\d+\.\d+\.\d+).*', '$1' } | Sort-Object -Unique)
    if ($ovVersions.Count -gt 1) { throw "OpenVINO DLLs come from different releases: $($ovVersions -join ', ')" }
    $ort = [version]((Get-Item (Join-Path $Dir "onnxruntime.dll")).VersionInfo.ProductVersion -replace '^(\d+\.\d+).*', '$1')
    if ($ort -lt [version]"1.22") { throw "onnxruntime.dll is ONNX Runtime $ort; SysX needs 1.22 or newer." }
    $ignored = @($all | Where-Object { $wanted.Name -notcontains $_.Name })
    if ($ignored.Count -gt 0) { Write-Host "Not shipped (SysX does not load them): $($ignored.Name -join ', ')" }
    $wanted
}

# Lists the models build.rs will embed and points it at $Dir.
function Use-Models([string] $Dir) {
    if (-not $Dir) { $Dir = Join-Path $Root "rust-backend\models" }
    if (-not (Test-Path -LiteralPath $Dir -PathType Container)) { throw "Models folder not found: $Dir" }
    $Dir = (Resolve-Path -LiteralPath $Dir).Path
    $folders = @($Dir) + @(Get-ChildItem -LiteralPath $Dir -Directory | ForEach-Object FullName)
    $files = @($folders | ForEach-Object { Get-ChildItem -LiteralPath $_ -File } |
        Where-Object { $_.Extension.ToLowerInvariant() -in @(".onnx", ".engine", ".plan", ".xml", ".bin") })
    # <name>_fp16.* / <name>_int8.* (no suffix: FP16), as builtin_models.rs groups them.
    $artifacts = foreach ($file in $files) {
        $stem = $file.BaseName
        $precision = "FP16"
        if ($stem -match '^(.*)[_.-](fp16|int8)$') { $stem = $Matches[1]; $precision = $Matches[2].ToUpperInvariant() }
        [pscustomobject]@{ Name = $stem; Precision = $precision; Ext = $file.Extension.ToLowerInvariant() }
    }
    $models = @()
    $skipped = @()
    foreach ($group in ($artifacts | Group-Object Name)) {
        if (-not ($group.Group | Where-Object Ext -eq ".onnx")) { $skipped += $group.Name; continue }
        $row = [ordered]@{ Model = $group.Name }
        foreach ($precision in @("FP16", "INT8")) {
            $ext = @($group.Group | Where-Object Precision -eq $precision | ForEach-Object Ext)
            $kinds = @()
            if ($ext -contains ".onnx") { $kinds += "ONNX" }
            if ($ext -contains ".xml" -and $ext -contains ".bin") { $kinds += "OpenVINO" }
            if ($ext -contains ".engine" -or $ext -contains ".plan") { $kinds += "TensorRT" }
            $row[$precision] = if ($kinds) { $kinds -join " + " } else { "-" }
        }
        $models += [pscustomobject]$row
    }
    Write-Host "Models in ${Dir}:"
    if ($models.Count -eq 0) {
        Write-Warning "No .onnx model found: SysX will build with Pixel Seek only."
    } else {
        $models | Format-Table -AutoSize | Out-String | Write-Host
    }
    if ($skipped.Count -gt 0) { Write-Warning "Skipped (no .onnx in any precision): $($skipped -join ', ')" }
    if (($models | Where-Object { "$($_.FP16) $($_.INT8)" -match "TensorRT" }) -and -not (Test-TensorRtBuild)) {
        Write-Warning "TensorRT engines are embedded, but this build has no TensorRT adapter (set TENSORRT_PATH and CUDA_PATH to build it); the TensorRT runtime will not start."
    }
    $env:SYSX_EMBEDDED_MODELS = $Dir
}

# Builds the backend (models embedded) and the host; returns the host exe.
function Build-SysXExe([switch] $Test) {
    if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne "X64") {
        throw "Build SysX on a Windows x64 machine."
    }
    foreach ($tool in @("cargo", "cmake", "nasm")) {
        if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
            throw "$tool was not found. Install it (Rust, CMake and NASM are build prerequisites)."
        }
    }
    Push-Location $Root
    try {
        $features = @("ort/load-dynamic", "directml")
        if (Test-TensorRtBuild) { $features += "tensorrt-shim" }
        Invoke-Checked cargo @("build", "--manifest-path", "rust-backend\Cargo.toml", "--release",
            "--bin", "receiver-backend", "--target", $Target, "--features", ($features -join ","))
        $binaries = Join-Path $Root "src-tauri\binaries"
        New-Item -ItemType Directory -Force $binaries | Out-Null
        Get-ChildItem -LiteralPath $binaries -Filter *.dll -File | Remove-Item -Force
        Copy-Item (Join-Path $Root "rust-backend\target\$Target\release\receiver-backend.exe") `
            (Join-Path $binaries "receiver-backend-$Target.exe") -Force
        if ($Test) {
            Invoke-Checked cargo @("test", "--manifest-path", "rust-backend\Cargo.toml", "--features", "ort/load-dynamic")
            Invoke-Checked cargo @("test", "--manifest-path", "rust-ui\Cargo.toml")
            Invoke-Checked cargo @("test", "--manifest-path", "src-tauri\Cargo.toml")
        }
        Invoke-Checked cargo @("build", "--manifest-path", "src-tauri\Cargo.toml", "--release")
    } finally {
        Pop-Location
    }
    $exe = Join-Path $Root "src-tauri\target\release\receiver.exe"
    if (-not (Test-Path -LiteralPath $exe)) { throw "The host build did not produce $exe" }
    # x86-64 PE check.
    $bytes = [IO.File]::ReadAllBytes($exe)
    $pe = [BitConverter]::ToInt32($bytes, 0x3C)
    if ($bytes[0] -ne 0x4D -or $bytes[1] -ne 0x5A -or [BitConverter]::ToUInt16($bytes, $pe + 4) -ne 0x8664) {
        throw "SysX.exe is not an x86-64 PE image."
    }
    $exe
}

function Write-Readme {
    @"
SysX
====
Giu tat ca file .dll cung thu muc voi SysX.exe.
Yeu cau: Microsoft Visual C++ Redistributable 2015-2022 (x64)
  https://aka.ms/vs/17/release/vc_redist.x64.exe
SysX nhan frame tu OBS plugin (sysx-canvas-sender.dll) qua UDP 4200.
Cai dat luu o %LOCALAPPDATA%\SysX.

Thu vien di kem (giay phep cua nha cung cap):
  onnxruntime*.dll  Microsoft ONNX Runtime (MIT)
  DirectML.dll      Microsoft DirectML (Microsoft redistributable license)
  openvino*.dll     Intel OpenVINO (Apache-2.0)
  tbb*.dll          oneTBB (Apache-2.0)
  nvinfer*.dll      NVIDIA TensorRT (NVIDIA SLA), chi khi build co TensorRT
"@ | Set-Content -Path (Join-Path $Stage "HUONG-DAN.txt") -Encoding utf8
}

# build\SysX: SysX.exe, the runtime DLLs and HUONG-DAN.txt; the old folder is deleted.
function Publish-SysX([string] $DependenciesDir, [string] $ModelsDir, [switch] $Test) {
    $dlls = Get-RuntimeDlls (Resolve-DependenciesDir $DependenciesDir)
    Use-Models $ModelsDir
    $exe = Build-SysXExe -Test:$Test
    if (Test-Path $Stage) { Remove-Item $Stage -Recurse -Force }
    New-Item -ItemType Directory -Force $Stage | Out-Null
    Copy-Item $exe (Join-Path $Stage "SysX.exe")
    $dlls | Copy-Item -Destination $Stage -Force
    Write-Readme
    Show-Folder $Stage
}

# build\OBS-plugin\sysx-canvas-sender.dll through build-obs-plugin.ps1
# (which also keeps its own copies in dist-package and the plugin source);
# the old folder is deleted.
function Publish-ObsPlugin([System.Collections.IDictionary] $PluginArgs) {
    $splat = @{}
    foreach ($key in $PluginArgs.Keys) { $splat[$key] = $PluginArgs[$key] }
    & (Join-Path $PSScriptRoot "build-obs-plugin.ps1") @splat
    $dll = Join-Path $Root "work\obs-build-release\sysx-canvas-sender.dll"
    if (-not (Test-Path -LiteralPath $dll)) { throw "The OBS plugin build did not produce $dll" }
    if (Test-Path $PluginStage) { Remove-Item $PluginStage -Recurse -Force }
    New-Item -ItemType Directory -Force $PluginStage | Out-Null
    Copy-Item $dll $PluginStage
    @"
SysX OBS plugin
===============
Dong OBS, chep sysx-canvas-sender.dll vao
  C:\Program Files\obs-studio\obs-plugins\64bit\
roi mo lai OBS.
"@ | Set-Content -Path (Join-Path $PluginStage "HUONG-DAN.txt") -Encoding utf8
    Show-Folder $PluginStage
}

function Show-Folder([string] $Dir) {
    Write-Host ""
    Write-Host "Output: $Dir"
    Get-ChildItem $Dir -File | Sort-Object Name | ForEach-Object {
        "  {0,14:N0}  {1}" -f $_.Length, $_.Name
    } | Write-Host
}
