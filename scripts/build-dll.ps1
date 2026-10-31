param(
    # 'auto' finds the receiver on the LAN at run time; an IPv4 address fixes it.
    [string] $ReceiverIp,
    # Overrides for build-obs-plugin.ps1's libobs and libjpeg-turbo paths.
    [string] $ObsInclude,
    [string] $JpegInclude,
    [string] $JpegLibrary
)
# Only the OBS plugin: build\OBS-plugin\sysx-canvas-sender.dll, replacing the
# previous one. build\SysX is kept.
#
#   powershell -ExecutionPolicy Bypass -File scripts\build-dll.ps1
. (Join-Path $PSScriptRoot "release-common.ps1")

Publish-ObsPlugin $PSBoundParameters
