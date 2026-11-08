# Copies Microsoft's ConPTY (conpty.dll + OpenConsole.exe) next to maomiterminal.exe.
# portable-pty loads a conpty.dll from the exe's folder before the one in Windows. The inbox
# ConPTY on Windows 10/11 garbles joined emoji (ZWJ, skin tones) in its own output, which
# jumbles Claude Code and other grapheme-aware apps; this newer build handles them.
# The package is pinned by version and SHA-256 and cached in target/conpty.
param(
    # Folder that holds maomiterminal.exe.
    [string]$Dest = "$PSScriptRoot/../target/debug",
    [ValidateSet('x64', 'arm64')]
    [string]$Arch = 'x64'
)
$ErrorActionPreference = 'Stop'

$version = '1.25.260930003'
$sha256 = '02B07B349AF66D801159BDF9E440D4A1CE78BB951F37FC8609731665AFDAE7EE'
$url = "https://api.nuget.org/v3-flatcontainer/microsoft.windows.console.conpty/$version/microsoft.windows.console.conpty.$version.nupkg"

$cache = "$PSScriptRoot/../target/conpty/$version"
$package = "$cache/conpty.zip"
function Get-Sha256($path) { (Get-FileHash -Algorithm SHA256 $path).Hash }
if (-not (Test-Path $package) -or (Get-Sha256 $package) -ne $sha256) {
    New-Item -ItemType Directory -Force $cache | Out-Null
    Invoke-WebRequest -Uri $url -OutFile "$package.part" -UseBasicParsing
    $actual = Get-Sha256 "$package.part"
    if ($actual -ne $sha256) {
        throw "ConPTY package hash mismatch: got $actual, expected $sha256"
    }
    Move-Item -Force "$package.part" $package
}

$extracted = "$cache/x"
if (-not (Test-Path "$extracted/runtimes")) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [System.IO.Compression.ZipFile]::ExtractToDirectory((Resolve-Path $package).Path, $extracted)
}

New-Item -ItemType Directory -Force $Dest | Out-Null
foreach ($file in "$extracted/runtimes/win-$Arch/native/conpty.dll", "$extracted/build/native/runtimes/$Arch/OpenConsole.exe") {
    $target = Join-Path $Dest (Split-Path $file -Leaf)
    # A running MaomiTerminal locks its copy; an identical one needs no copy.
    if (-not (Test-Path $target) -or (Get-Sha256 $target) -ne (Get-Sha256 $file)) {
        Copy-Item -Force $file $Dest
    }
}
Write-Host "ConPTY $version ($Arch) copied to $((Resolve-Path $Dest).Path)"
