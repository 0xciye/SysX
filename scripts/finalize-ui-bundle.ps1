param([Parameter(Mandatory)] [string] $Directory)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path -LiteralPath $Directory).Path
$index = Join-Path $root 'index.html'
$html = [IO.File]::ReadAllText($index)
# Trunk leaves the Rust link in place when inject_scripts=false. Such output
# serves the static boot placeholder forever, even with a healthy UDP backend.
if ($html -match '<link\b[^>]*\bdata-trunk\b' -or
    $html -notmatch '(?is)<script\b[^>]*type\s*=\s*["'']module["''][^>]*>.*?\bimport\s+init\b.*?\bawait\s+init\s*\(') {
    throw 'UI bootstrap is missing. Enable Trunk inject_scripts before packaging.'
}
foreach ($name in @('sysx-ui.js', 'sysx-ui_bg.wasm')) {
    if (-not (Test-Path -LiteralPath (Join-Path $root $name) -PathType Leaf)) {
        throw "UI bootstrap asset missing: $name"
    }
}
# Wasm metadata stripping runs after Trunk calculates its preload SRI. Refresh
# those hashes against final bytes before the host embeds/hashes the UI bundle.
$updated = [regex]::Replace($html, '(?is)<link\b[^>]*>', [System.Text.RegularExpressions.MatchEvaluator]{
    param($tag)
    $href = [regex]::Match($tag.Value, '\bhref\s*=\s*["'']([^"'']+)["'']')
    $sri = [regex]::Match($tag.Value, '\bintegrity\s*=\s*["'']sha384-[^"'']+["'']')
    if (-not $href.Success -or -not $sri.Success) { return $tag.Value }
    $relative = $href.Groups[1].Value
    if ($relative -match '(^/|:|\\|\?|#)' -or $relative.Split('/') -contains '..') {
        throw "UI integrity asset must be local: $relative"
    }
    $path = Join-Path $root $relative
    $hash = [Security.Cryptography.SHA384]::Create()
    try { $digest = [Convert]::ToBase64String($hash.ComputeHash([IO.File]::ReadAllBytes($path))) }
    finally { $hash.Dispose() }
    return $tag.Value.Remove($sri.Index, $sri.Length).Insert($sri.Index, "integrity=`"sha384-$digest`"")
})
[IO.File]::WriteAllText($index, $updated, [Text.UTF8Encoding]::new($false))
Write-Host 'UI bootstrap and final asset integrity verified.'
