<#
Builds the Windows host and assembles DisplaySwarm-<version>-windows-x64.zip in the repo root.

  powershell -File packaging/windows/package.ps1 [-Version 0.3.1] [-SkipBuild]

Needs the same environment as a normal build: MSVC, LLVM (LIBCLANG_PATH) and the BtbN
"win64-gpl-shared" FFmpeg, unpacked, with FFMPEG_DIR pointing at its root (bin/ holds the DLLs).

Licensing: the FFmpeg build is GPL (it contains libx264), so the zip is a GPL distribution.
DisplaySwarm itself is GPL-3.0, which is compatible. The zip therefore carries FFmpeg's
LICENSE.txt and the README points to the FFmpeg source. Do not swap in a non-GPL FFmpeg
without re-checking that libx264 (the software encoder fallback) is still available.
#>
param(
    [string]$Version,
    [switch]$SkipBuild
)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
if (-not $Version) {
    $Version = (Select-String -Path "$root\host\Cargo.toml" -Pattern '^version = "(.*)"' | Select-Object -First 1).Matches[0].Groups[1].Value
}
if (-not $env:FFMPEG_DIR) { throw 'FFMPEG_DIR is not set (the unpacked BtbN win64-gpl-shared build)' }
$ffbin = Join-Path $env:FFMPEG_DIR 'bin'

if (-not $SkipBuild) {
    Push-Location "$root\host"
    try {
        cargo build --release --locked
        if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    } finally { Pop-Location }
}
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { "$root\host\target" }
$rel = Join-Path $target 'release'

# FFmpeg DLLs: the transitive imports of our two executables, found by reading their PE import
# tables, so the zip carries exactly what loads (and nothing the build does not link).
function Get-Imports([string]$file) {
    $tool = Get-Command llvm-objdump.exe -ErrorAction SilentlyContinue
    if ($tool) {
        & $tool.Source -p $file | ForEach-Object { if ($_ -match 'DLL Name:\s*(\S+)') { $Matches[1] } }
        return
    }
    $tool = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
    if (-not $tool) { throw 'need llvm-objdump or dumpbin on PATH to list DLL imports' }
    & $tool.Source /dependents $file | ForEach-Object { if ($_ -match '^\s+(\S+\.dll)\s*$') { $Matches[1] } }
}
$have = @{}
Get-ChildItem $ffbin -Filter *.dll | ForEach-Object { $have[$_.Name.ToLower()] = $_.FullName }
$need = [ordered]@{}
$queue = [System.Collections.Generic.Queue[string]]::new()
'displayswarm.exe', 'displayswarm-hostd.exe' | ForEach-Object { $queue.Enqueue("$rel\$_") }
while ($queue.Count) {
    foreach ($dll in Get-Imports $queue.Dequeue()) {
        $key = $dll.ToLower()
        if ($have.ContainsKey($key) -and -not $need.Contains($key)) {
            $need[$key] = $have[$key]
            $queue.Enqueue($have[$key])
        }
    }
}
if (-not $need.Count) { throw "no FFmpeg DLLs are imported by the build; is FFMPEG_DIR right? ($ffbin)" }

$name = "DisplaySwarm-$Version-windows-x64"
$stage = Join-Path ([System.IO.Path]::GetTempPath()) ("ds-pkg-" + [guid]::NewGuid().ToString('N'))
$dir = Join-Path $stage $name
New-Item -ItemType Directory -Path $dir | Out-Null
try {
    Copy-Item "$rel\displayswarm.exe", "$rel\displayswarm-hostd.exe" $dir
    $need.Values | ForEach-Object { Copy-Item $_ $dir }
    Copy-Item "$root\LICENSE" "$dir\LICENSE.txt"
    Copy-Item "$root\NOTICE" "$dir\NOTICE.txt"
    Copy-Item "$env:FFMPEG_DIR\LICENSE.txt" "$dir\FFmpeg-LICENSE.txt"
    Copy-Item "$PSScriptRoot\README-Windows.txt" $dir
    $zip = Join-Path $root "$name.zip"
    if (Test-Path $zip) { Remove-Item $zip }
    Compress-Archive -Path $dir -DestinationPath $zip
    Write-Host "FFmpeg DLLs: $(($need.Keys) -join ', ')"
    Write-Host "built $zip"
} finally {
    Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
}
