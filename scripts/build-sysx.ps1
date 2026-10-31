param(
    # Runtime DLL folder. Default: $env:SYSX_ORT_DIR, else <repo>\runtime-deps.
    [string] $DependenciesDir,
    # Built-in models (and its onnx\, openvino\, tensorrt\ subfolders).
    # Default: rust-backend\models.
    [string] $ModelsDir,
    # Also run the backend, UI and host test suites.
    [switch] $Test
)
# Only SysX: build\SysX\ (SysX.exe + runtime DLLs), replacing the previous
# one. The OBS plugin in build\OBS-plugin is kept.
#
#   powershell -ExecutionPolicy Bypass -File scripts\build-sysx.ps1
. (Join-Path $PSScriptRoot "release-common.ps1")

Publish-SysX -DependenciesDir $DependenciesDir -ModelsDir $ModelsDir -Test:$Test
