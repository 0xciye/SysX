param(
    # Runtime DLL folder. Default: $env:SYSX_ORT_DIR, else <repo>\runtime-deps.
    [string] $DependenciesDir,
    # Built-in models (and its onnx\, openvino\, tensorrt\ subfolders).
    # Default: rust-backend\models.
    [string] $ModelsDir,
    # OBS plugin destination: 'auto' (LAN discovery, default) or an IPv4 address.
    [string] $ReceiverIp,
    # Also run the backend, UI and host test suites.
    [switch] $Test
)
# Everything: deletes build\, then builds build\SysX (SysX.exe + runtime
# DLLs) and build\OBS-plugin (sysx-canvas-sender.dll) and zips both into
# build\SysX-<date>.zip.
#
#   powershell -ExecutionPolicy Bypass -File scripts\build-full.ps1
. (Join-Path $PSScriptRoot "release-common.ps1")

if (Test-Path $BuildDir) { Remove-Item $BuildDir -Recurse -Force }
$pluginArgs = @{}
if ($ReceiverIp) { $pluginArgs.ReceiverIp = $ReceiverIp }
Publish-ObsPlugin $pluginArgs
Publish-SysX -DependenciesDir $DependenciesDir -ModelsDir $ModelsDir -Test:$Test

$zip = Join-Path $BuildDir ("SysX-{0}.zip" -f (Get-Date -Format "yyyyMMdd-HHmm"))
Compress-Archive -Path $Stage, $PluginStage -DestinationPath $zip -CompressionLevel Optimal
Write-Host ""
Write-Host "Zip: $zip (SysX\ + OBS-plugin\)"
