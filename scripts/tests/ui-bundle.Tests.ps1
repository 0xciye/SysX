# Standalone fixture regression; no browser, compiler or third-party modules.
$ErrorActionPreference = 'Stop'
$finalize = Join-Path $PSScriptRoot '../finalize-ui-bundle.ps1'
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('sysx-ui-fixture-' + [Guid]::NewGuid())
New-Item -ItemType Directory $fixture | Out-Null
try {
    $index = Join-Path $fixture 'index.html'
    [IO.File]::WriteAllText((Join-Path $fixture 'sysx-ui.js'), 'export default async function init() {}')
    [IO.File]::WriteAllBytes((Join-Path $fixture 'sysx-ui_bg.wasm'), [byte[]](0,97,115,109,1,0,0,0))
    # Reproduce the actual broken output: Trunk leaves its Rust link intact.
    [IO.File]::WriteAllText($index, '<html><body>Waiting for the Rust receiver to start<link data-trunk rel="rust" href="Cargo.toml"></body></html>')
    $rejected = $false
    try { & $finalize -Directory $fixture } catch { $rejected = $_.Exception.Message -match 'bootstrap is missing' }
    if (-not $rejected) { throw 'Broken HTML placeholder was accepted.' }

    $valid = @'
<html><head><link rel="preload" href="./sysx-ui_bg.wasm" as="fetch" integrity="sha384-before-stripping"></head>
<body><script type="module">import init from './sysx-ui.js'; const wasm = await init({module_or_path: './sysx-ui_bg.wasm'});</script></body></html>
'@
    [IO.File]::WriteAllText($index, $valid)
    & $finalize -Directory $fixture
    $algorithm = [Security.Cryptography.SHA384]::Create()
    try { $hash = [Convert]::ToBase64String($algorithm.ComputeHash([IO.File]::ReadAllBytes((Join-Path $fixture 'sysx-ui_bg.wasm')))) }
    finally { $algorithm.Dispose() }
    $updated = [IO.File]::ReadAllText($index)
    if (-not $updated.Contains("integrity=`"sha384-$hash`"")) { throw 'Final Wasm SRI was not refreshed.' }
    & $finalize -Directory $fixture
    if ([IO.File]::ReadAllText($index) -ne $updated) { throw 'Finalization is not idempotent.' }

    Remove-Item -LiteralPath (Join-Path $fixture 'sysx-ui.js')
    $rejected = $false
    try { & $finalize -Directory $fixture } catch { $rejected = $_.Exception.Message -match 'asset missing' }
    if (-not $rejected) { throw 'Missing loader JS was accepted.' }
    Write-Output 'UI bundle regressions passed: missing bootstrap, final Wasm SRI, idempotence, missing loader.'
} finally { Remove-Item -LiteralPath $fixture -Recurse -Force }
