$ErrorActionPreference = "Stop"

if ($env:OS -ne "Windows_NT") {
    throw "The installer smoke test must run on Windows."
}

$root = Split-Path -Parent $PSScriptRoot
$installerDir = Join-Path $root "src-tauri\target\release\bundle\nsis"
$installers = @(Get-ChildItem -Path $installerDir -Filter "*-setup.exe" -File)
if ($installers.Count -ne 1) {
    throw "Expected one NSIS setup executable in $installerDir; found $($installers.Count)."
}

$runId = [guid]::NewGuid().ToString("N")
$smokeRoot = Join-Path $env:TEMP "receiver-nsis-smoke-$runId"
$installDir = Join-Path $smokeRoot "app"
New-Item -ItemType Directory -Force -Path $smokeRoot | Out-Null
$installedExe = Join-Path $installDir "receiver.exe"
$uninstaller = Join-Path $installDir "uninstall.exe"
$uninstallComplete = $false

try {
    $install = Start-Process -FilePath $installers[0].FullName `
        -ArgumentList @("/S", "/D=$installDir") -WorkingDirectory $smokeRoot -Wait -PassThru
    if ($install.ExitCode -ne 0) {
        throw "NSIS installer exited with code $($install.ExitCode)."
    }
    if (-not (Test-Path $installedExe)) {
        throw "NSIS installer did not install the Receiver executable at $installedExe."
    }
    if (-not (Test-Path $uninstaller)) {
        throw "NSIS installer did not create its uninstaller at $uninstaller."
    }

    & (Join-Path $PSScriptRoot "smoke-windows-app.ps1") -ApplicationPath $installedExe

    $uninstall = Start-Process -FilePath $uninstaller `
        -ArgumentList @("/S", "_?=$installDir") -WorkingDirectory $smokeRoot -Wait -PassThru
    if ($uninstall.ExitCode -ne 0) {
        throw "NSIS uninstaller exited with code $($uninstall.ExitCode)."
    }
    if (Test-Path $installedExe) {
        throw "NSIS uninstaller left the Receiver executable in $installDir."
    }
    $uninstallComplete = $true

    Write-Host "Windows installer smoke test passed: silent install, installed app/backend startup and shutdown, and uninstall."
}
finally {
    if ((-not $uninstallComplete) -and (Test-Path $uninstaller)) {
        try {
            $cleanup = Start-Process -FilePath $uninstaller `
                -ArgumentList @("/S", "_?=$installDir") -WorkingDirectory $smokeRoot -Wait -PassThru
            if ($cleanup.ExitCode -ne 0) { Write-Warning "NSIS cleanup uninstaller exited with code $($cleanup.ExitCode)." }
        } catch {
            Write-Warning "NSIS cleanup failed: $_"
        }
    }
    Remove-Item -LiteralPath $smokeRoot -Recurse -Force -ErrorAction SilentlyContinue
}
