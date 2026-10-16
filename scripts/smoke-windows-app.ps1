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

if (Get-NetUDPEndpoint -LocalPort 4200 -ErrorAction SilentlyContinue) {
    throw "UDP 4200 is already occupied. Leave the existing process untouched and run this smoke on a free machine."
}

# Enumerate real top-level windows: the normal monitor belongs to SysX/Wry,
# while the fallback belongs to Edge. Never close an unrelated browser tab.
if (-not ("SysXSmokeWindows" -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public sealed class SysXSmokeWindow {
    public long Handle;
    public int ProcessId;
}
public static class SysXSmokeWindows {
    private delegate bool EnumWindow(IntPtr hwnd, IntPtr data);
    [DllImport("user32.dll")] private static extern bool EnumWindows(EnumWindow callback, IntPtr data);
    [DllImport("user32.dll")] private static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] private static extern int GetWindowText(IntPtr hwnd, StringBuilder text, int size);
    [DllImport("user32.dll")] private static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll", SetLastError = true)] private static extern bool PostMessage(IntPtr hwnd, uint message, IntPtr wparam, IntPtr lparam);
    public static SysXSmokeWindow[] Find() {
        var windows = new List<SysXSmokeWindow>();
        EnumWindows((hwnd, data) => {
            var title = new StringBuilder(512);
            GetWindowText(hwnd, title, title.Capacity);
            if (IsWindowVisible(hwnd) && title.ToString() == "SysX") {
                uint pid;
                GetWindowThreadProcessId(hwnd, out pid);
                windows.Add(new SysXSmokeWindow { Handle = hwnd.ToInt64(), ProcessId = (int)pid });
            }
            return true;
        }, IntPtr.Zero);
        return windows.ToArray();
    }
    public static bool Close(long hwnd) {
        return PostMessage(new IntPtr(hwnd), 0x0010, IntPtr.Zero, IntPtr.Zero); // WM_CLOSE
    }
}
'@
}
$existingWindows = @([SysXSmokeWindows]::Find() | ForEach-Object { $_.Handle })
if ($existingWindows.Count -gt 0) {
    throw "An existing SysX monitor is open. Run the smoke after closing that instance normally."
}
$app = $null
$testBackendIds = @()
$monitor = @()

try {
    # Suppress only the automatic config tab. A connected config page keeps
    # the current host alive by design after its monitor closes.
    $app = Start-Process -FilePath $appExe -ArgumentList "--smoke-test" -WorkingDirectory $appDir -PassThru
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

        $backend = Get-CimInstance Win32_Process -Filter "ParentProcessId = $($app.Id)" -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -like "receiver-backend*" } |
            Select-Object -First 1
        if ($backend) {
            $testBackendIds = @($backend.ProcessId)
            $udpListener = Get-NetUDPEndpoint -LocalPort 4200 -ErrorAction SilentlyContinue |
                Where-Object { $_.OwningProcess -eq $backend.ProcessId } |
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
    Write-Host "SysX launched its bundled backend (PID $($backend.ProcessId)), which bound UDP port 4200."

    # Accept either the host-owned Wry/WebView2 window or the dedicated Edge
    # fallback, but close only a new window belonging to this launch.
    $monitorDeadline = (Get-Date).AddSeconds(30)
    $monitor = @()
    while ((Get-Date) -lt $monitorDeadline) {
        $app.Refresh()
        if ($app.HasExited) {
            throw "SysX exited before its monitor window opened."
        }
        $edgeIds = @(
            Get-CimInstance Win32_Process -Filter "Name = 'msedge.exe'" -ErrorAction SilentlyContinue |
                Where-Object { $_.CommandLine -like "*SysX\monitor-window*" -and $_.CommandLine -like "*--app=*monitor.html*" } |
                Select-Object -ExpandProperty ProcessId
        )
        $monitor = @(
            [SysXSmokeWindows]::Find() | Where-Object {
                $existingWindows -notcontains $_.Handle -and
                ($_.ProcessId -eq $app.Id -or $edgeIds -contains $_.ProcessId)
            }
        )
        if ($monitor.Count -gt 0) { break }
        Start-Sleep -Milliseconds 250
    }
    if ($monitor.Count -eq 0) {
        throw "SysX did not open its monitor app window within 30 seconds."
    }
    # Let the page connect before closing it.
    Start-Sleep -Seconds 5
    foreach ($window in $monitor) {
        if (-not [SysXSmokeWindows]::Close($window.Handle)) {
            throw "Could not send WM_CLOSE to the SysX monitor."
        }
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
    foreach ($window in $monitor) {
        $null = [SysXSmokeWindows]::Close($window.Handle)
    }
    if ($app) {
        $app.Refresh()
        if (-not $app.HasExited) {
            Stop-Process -Id $app.Id -Force -ErrorAction SilentlyContinue
        }
    }
    foreach ($backendId in $testBackendIds) {
        if (Get-Process -Id $backendId -ErrorAction SilentlyContinue) {
            Stop-Process -Id $backendId -Force -ErrorAction SilentlyContinue
        }
    }
}
