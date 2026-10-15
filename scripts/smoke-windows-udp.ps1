$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
$sidecar = Join-Path $root "src-tauri\binaries\receiver-backend-x86_64-pc-windows-msvc.exe"
$sample = Join-Path $root "src\assets\red-cube-192.jpg"
if (-not (Test-Path $sidecar)) { throw "Windows backend sidecar not found: $sidecar" }
if (-not (Test-Path $sample)) { throw "Sample frame not found: $sample" }

$runId = [guid]::NewGuid().ToString("N")
$stdoutPath = Join-Path $env:TEMP "receiver-udp-$runId.out"
$stderrPath = Join-Path $env:TEMP "receiver-udp-$runId.err"
$listener = $null

try {
    $listener = Start-Process -FilePath $sidecar `
        -ArgumentList @("udp-listen", "--host", "127.0.0.1", "--count", "1") `
        -PassThru -NoNewWindow -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath

    $readyDeadline = (Get-Date).AddSeconds(120)
    $ready = $false
    while ((Get-Date) -lt $readyDeadline -and -not $listener.HasExited) {
        if (Test-Path $stdoutPath) {
            $ready = (Get-Content $stdoutPath -Raw -ErrorAction SilentlyContinue) -match '"event"\s*:\s*"listening".*"port"\s*:\s*4200'
            if ($ready) { break }
        }
        Start-Sleep -Milliseconds 100
    }
    if (-not $ready) {
        $errorText = if (Test-Path $stderrPath) { Get-Content $stderrPath -Raw } else { "" }
        throw "Backend did not bind its default UDP port 4200 within 120 seconds. $errorText"
    }

    $senderOutput = & $sidecar udp-send-image $sample --host 127.0.0.1 2>&1
    if ($LASTEXITCODE -ne 0) { throw "Sample sender failed: $senderOutput" }
    if (-not $listener.WaitForExit(15000)) { throw "UDP receiver did not complete after the sample frame was sent." }

    $received = if (Test-Path $stdoutPath) { Get-Content $stdoutPath -Raw } else { "" }
    if ($listener.ExitCode -ne 0 -or $received -notmatch '"event"\s*:\s*"frame".*"width"\s*:\s*192.*"height"\s*:\s*192') {
        throw "UDP reassembly smoke test failed. Receiver output: $received"
    }

    Write-Host "Windows UDP smoke test passed: default port 4200 received and reassembled a 192x192 sample frame."
    Write-Host $received.Trim()
}
finally {
    if ($listener -and -not $listener.HasExited) { Stop-Process -Id $listener.Id -Force -ErrorAction SilentlyContinue }
    Remove-Item $stdoutPath, $stderrPath -Force -ErrorAction SilentlyContinue
}
