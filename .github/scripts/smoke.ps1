# End-to-end test of the real binary against real NTFS junctions and a second drive.
# Runs on GitHub's windows-latest runners (C: system drive, D: temp drive).
param([Parameter(Mandatory)][string]$Exe)

$ErrorActionPreference = 'Stop'
$Exe = (Resolve-Path $Exe).Path

function Fail($msg) { Write-Host "SMOKE FAILURE: $msg" -ForegroundColor Red; exit 1 }
function Run([string[]]$cmdArgs) {
    $out = & $Exe @cmdArgs 2>&1 | Out-String
    return [pscustomobject]@{ Code = $LASTEXITCODE; Out = $out }
}
function NewGame($root) {
    Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force "$root\Game\cache\deep" | Out-Null
    Set-Content "$root\Game\a.txt" 'alpha'
    [IO.File]::WriteAllBytes("$root\Game\cache\deep\b.bin", (New-Object byte[] (1MB)))
    New-Item -ItemType Directory -Force "$root\Game\empty" | Out-Null
    return "$root\Game"
}
function DestOf($src) { return Join-Path 'D:\Rehome' $src.Substring(3) }

if (-not (Test-Path 'D:\')) { Fail 'no D: drive on this runner' }
$base = Join-Path $env:LOCALAPPDATA 'RehomeSmoke'

Write-Host '--- version / help'
if ((Run @('--version')).Code -ne 0) { Fail '--version' }

Write-Host '--- check changes nothing'
$src = NewGame "$base\one"
$r = Run @('check', $src, '--to', 'D:\')
Write-Host $r.Out
if ($r.Code -ne 0) { Fail "check failed" }
if ((Get-Item $src).LinkType) { Fail 'check must not create a junction' }

Write-Host '--- unsafe paths are refused'
foreach ($bad in @('C:\Windows', $env:LOCALAPPDATA, 'C:\Program Files')) {
    $r = Run @('check', $bad, '--to', 'D:\')
    if ($r.Code -eq 0) { Fail "should have refused $bad" }
}

Write-Host '--- move'
$r = Run @('move', $src, '--to', 'D:\', '--yes')
Write-Host $r.Out
if ($r.Code -ne 0) { Fail "move failed" }
if ((Get-Item $src).LinkType -ne 'Junction') { Fail 'source is not a junction' }
if ((Get-Content "$src\a.txt") -ne 'alpha') { Fail 'content not readable through the junction' }
if ((Get-Item "$src\cache\deep\b.bin").Length -ne 1MB) { Fail 'big file wrong size' }
$dest = DestOf $src
if (-not (Test-Path "$dest\.rehome\origin.txt")) { Fail 'marker missing' }
if (Test-Path "$src.rehome-old") { Fail 'old folder not cleaned up' }
if (Test-Path "$dest.rehome-partial") { Fail 'partial folder not cleaned up' }

Write-Host '--- already-moved folder is refused'
if ((Run @('move', $src, '--to', 'D:\', '--yes')).Code -eq 0) { Fail 'second move should be refused' }

Write-Host '--- undo'
$r = Run @('undo', $src, '--yes')
Write-Host $r.Out
if ($r.Code -ne 0) { Fail 'undo failed' }
if ((Get-Item $src).LinkType) { Fail 'still a junction after undo' }
if ((Get-Content "$src\a.txt") -ne 'alpha') { Fail 'content lost by undo' }
if (Test-Path $dest) { Fail 'relocated copy not removed after undo' }
if (Test-Path "$src\.rehome") { Fail 'marker leaked into restored folder' }

Write-Host '--- folder in use is refused, untouched, then succeeds once released'
$src2 = NewGame "$base\two"
$handle = [IO.File]::Open("$src2\a.txt", 'Open', 'ReadWrite', 'None')   # no sharing: like a running app
try {
    $r = Run @('move', $src2, '--to', 'D:\', '--yes')
    Write-Host $r.Out
    if ($r.Code -eq 0) { Fail 'move of an in-use folder should have been refused' }
    if ($r.Out -notmatch 'still using') { Fail 'in-use message missing' }
    if ((Get-Item $src2).LinkType) { Fail 'in-use folder must stay a real folder' }
    if ((Get-Item $src2).PSIsContainer -ne $true) { Fail 'folder vanished' }
    if (Test-Path "$src2.rehome-old") { Fail 'leftover .rehome-old' }
    if (Test-Path "$(DestOf $src2).rehome-partial") { Fail 'leftover partial copy' }
} finally { $handle.Dispose() }
$r = Run @('move', $src2, '--to', 'D:\', '--yes')
Write-Host $r.Out
if ($r.Code -ne 0) { Fail 'move after releasing the file should succeed' }
if ((Get-Item $src2).LinkType -ne 'Junction') { Fail 'second folder not a junction' }
if ((Run @('undo', $src2, '--yes')).Code -ne 0) { Fail 'undo of second folder' }

Write-Host '--- recover with nothing pending'
if ((Run @('recover')).Code -ne 0) { Fail 'recover' }

Remove-Item -Recurse -Force $base -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force 'D:\Rehome' -ErrorAction SilentlyContinue
Write-Host 'SMOKE TEST PASSED' -ForegroundColor Green
