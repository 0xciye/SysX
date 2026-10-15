param([string] $ApplicationPath)
$ErrorActionPreference = "Stop"

if ($env:OS -ne "Windows_NT") {
    throw "The desktop runtime smoke test must run on Windows."
}

$root = Split-Path -Parent $PSScriptRoot
$releaseDir = Join-Path $root "src-tauri\target\release"
$appExe = if ($ApplicationPath) { $ApplicationPath } else { Join-Path $releaseDir "SysX.exe" }
if (-not (Test-Path $appExe)) { throw "SysX desktop executable not found: $appExe" }
$appExe = (Resolve-Path -LiteralPath $appExe).Path
$appDir = Split-Path -Parent $appExe

$existingBackendIds = @(
    Get-Process -Name "receiver-backend*" -ErrorAction SilentlyContinue |
        Select-Object -ExpandProperty Id
)
$app = $null
$testBackendIds = @()

try {
    $app = Start-Process -FilePath $appExe -WorkingDirectory $appDir -PassThru
    # Windows PowerShell 5.1 only reports ExitCode if the process handle was opened.
    $null = $app.Handle
    $startupDeadline = (Get-Date).AddSeconds(120)
    $backend = $null
    $udpListener = $null

    while ((Get-Date) -lt $startupDeadline) {
        $app.Refresh()
        if ($app.HasExited) {
            throw "SysX exited during startup with code $($app.ExitCode)."
        }

        $backend = Get-Process -Name "receiver-backend*" -ErrorAction SilentlyContinue |
            Where-Object { $existingBackendIds -notcontains $_.Id } |
            Select-Object -First 1
        if ($backend) {
            $udpListener = Get-NetUDPEndpoint -LocalPort 4200 -ErrorAction SilentlyContinue |
                Where-Object { $_.OwningProcess -eq $backend.Id } |
                Select-Object -First 1
            if ($udpListener) { break }
        }
        Start-Sleep -Milliseconds 250
    }

    if (-not $backend) {
        throw "SysX did not launch its bundled Rust backend within 120 seconds."
    }
    if (-not $udpListener) {
        throw "SysX backend did not bind the default UDP port 4200 within 120 seconds."
    }
    $testBackendIds = @($backend.Id)
    Write-Host "SysX launched its bundled backend (PID $($backend.Id)), which bound UDP port 4200."

    # The receiver window is an Edge app window using SysX's own profile
    # (`--user-data-dir=...\SysX\monitor-window`). Closing it must stop SysX.
    $monitorDeadline = (Get-Date).AddSeconds(30)
    $monitor = @()
    while ((Get-Date) -lt $monitorDeadline) {
        $app.Refresh()
        if ($app.HasExited) {
            throw "SysX exited before its monitor window opened."
        }
        $monitor = @(
            Get-CimInstance Win32_Process -Filter "Name = 'msedge.exe'" -ErrorAction SilentlyContinue |
                Where-Object { $_.CommandLine -like "*SysX\monitor-window*" -and $_.CommandLine -like "*--app=*monitor.html*" }
        )
        if ($monitor.Count -gt 0) { break }
        Start-Sleep -Milliseconds 250
    }
    if ($monitor.Count -eq 0) {
        throw "SysX did not open its monitor app window within 30 seconds."
    }
    # Let the page connect before closing it.
    Start-Sleep -Seconds 5
    foreach ($process in $monitor) {
        & taskkill.exe /PID $process.ProcessId /T | Out-Null
    }
    if (-not $app.WaitForExit(20000)) {
        throw "SysX did not exit after its monitor window was closed."
    }

    $shutdownDeadline = (Get-Date).AddSeconds(10)
    do {
        $remainingBackend = @(
            Get-Process -Name "receiver-backend*" -ErrorAction SilentlyContinue |
                Where-Object { $testBackendIds -contains $_.Id }
        )
        if ($remainingBackend.Count -eq 0) { break }
        Start-Sleep -Milliseconds 200
    } while ((Get-Date) -lt $shutdownDeadline)

    if ($remainingBackend.Count -gt 0) {
        throw "SysX exited but left its bundled backend running."
    }

    Write-Host "Windows desktop smoke test passed: app launch, backend startup, monitor window, and shutdown on close."
}
finally {
    if ($app) {
        $app.Refresh()
        if (-not $app.HasExited) {
            & taskkill.exe /PID $app.Id /T /F | Out-Null
        }
    }
    foreach ($backendId in $testBackendIds) {
        if (Get-Process -Id $backendId -ErrorAction SilentlyContinue) {
            & taskkill.exe /PID $backendId /T /F | Out-Null
        }
    }
}
