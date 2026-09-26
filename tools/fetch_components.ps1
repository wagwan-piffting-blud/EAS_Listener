<#
.SYNOPSIS
Fetches the pinned third-party binaries listed in components.json into this directory.

.DESCRIPTION
The listener resolves each binary as: config.json key, then this directory, then PATH. Anything
placed here by this script is found automatically with no configuration.

Downloads are verified against the SHA-256 digests published by each upstream. Nothing is
redistributed by this project; the files come from their own projects' servers.

.EXAMPLE
.\fetch_components.ps1
.EXAMPLE
.\fetch_components.ps1 -Component ffmpeg -Force
.EXAMPLE
.\fetch_components.ps1 -Component cep6
.EXAMPLE
.\fetch_components.ps1 -InstallRoot C:\ProgramData\eas-listener\north -Component ffmpeg
#>
[CmdletBinding()]
param(
    [string]$Component = "",
    [switch]$Force,
    [switch]$List,
    # Installs into <InstallRoot>\tools (and data under <InstallRoot>) instead of beside this
    # script: what the listener passes when it cannot write here.
    [string]$InstallRoot = ""
)

$ErrorActionPreference = "Stop"
$ScriptDir = $PSScriptRoot
# Data a component needs beside its binaries (a voice, say) is laid out from the install root,
# the directory above this one, the same way the listener resolves it.
if (-not $InstallRoot) { $InstallRoot = Split-Path $ScriptDir -Parent }
$ToolsDir = Join-Path $InstallRoot "tools"
New-Item -ItemType Directory -Path $ToolsDir -Force | Out-Null
$ManifestPath = Join-Path $ScriptDir "components.json"

function Write-Step  { param($m) Write-Host "components: $m" }
function Write-Warn  { param($m) Write-Host "components: $m" -ForegroundColor Yellow }
function Write-Fail  { param($m) Write-Host "components: $m" -ForegroundColor Red }

if (-not (Test-Path $ManifestPath)) {
    Write-Fail "manifest not found at $ManifestPath"
    exit 1
}

$manifest = Get-Content $ManifestPath -Raw | ConvertFrom-Json

# The manifest keys platforms the way rustc names targets, so the same file serves every OS.
$arch = if ([Environment]::Is64BitOperatingSystem) { "x86_64" } else { "x86" }
if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64" -or $env:PROCESSOR_ARCHITEW6432 -eq "ARM64") {
    $arch = "aarch64"
}
$platformKey = "windows-$arch"

if ($List) {
    Write-Step "platform: $platformKey"
    foreach ($name in $manifest.components.PSObject.Properties.Name) {
        $c = $manifest.components.$name
        Write-Host ("  {0,-10} {1,-9} {2,-22} {3}" -f $name, $c.requirement, $c.license, $c.install)
    }
    exit 0
}

function Get-BinaryNames {
    param($Component)
    # Windows binaries carry .exe; the manifest stores the bare names.
    return $Component.provides | ForEach-Object { "$_.exe" }
}

function Get-DataEntries {
    param($Component)
    if ($null -eq $Component.data) { return @() }
    return @($Component.data)
}

# Records which pinned download a component came from, so bumping the pin in the manifest
# replaces what an earlier run installed instead of keeping it because the files exist.
function Get-StampPath {
    param($Name)
    return Join-Path $ToolsDir ".$Name.sha256"
}

# The component's own files plus the platform's: a platform whose download lacks what the others'
# archives carry -- Speechify's bare Windows executable, without Tom -- lists it itself.
function Get-Files {
    param($Component, $Platform)
    $files = @()
    if ($null -ne $Component.files) { $files += @($Component.files) }
    if ($null -ne $Platform -and $null -ne $Platform.files) { $files += @($Platform.files) }
    return $files
}

function Test-AlreadyInstalled {
    param($Name, $Component, $Platform)
    $binDir = if ($Component.layout -eq "directory") { Join-Path $ToolsDir $Name } else { $ToolsDir }
    foreach ($binary in (Get-BinaryNames $Component)) {
        if (-not (Test-Path (Join-Path $binDir $binary))) { return $false }
    }
    foreach ($file in (Get-Files $Component $Platform)) {
        if (-not (Test-Path (Join-Path $InstallRoot ($file.to -replace "/", "\")) -PathType Leaf)) { return $false }
    }
    foreach ($entry in (Get-DataEntries $Component)) {
        if (-not (Test-Path (Join-Path $InstallRoot $entry.to) -PathType Container)) { return $false }
    }
    # No stamp means an install from before stamps existed; its files are trusted as they are.
    $stamp = Get-StampPath $Name
    if ((Test-Path $stamp) -and ((Get-Content $stamp -Raw).Trim() -ne $Platform.sha256.ToLower())) {
        Write-Step "$Name is pinned to a different build than the one installed; replacing it"
        return $false
    }
    return $true
}

# .NET directly rather than Get-FileHash, which needs a module that a PowerShell started from a
# PowerShell 7 environment cannot always load.
function Get-Sha256 {
    param($Path)
    $stream = [System.IO.File]::OpenRead($Path)
    try {
        $sha = [System.Security.Cryptography.SHA256]::Create()
        return ([System.BitConverter]::ToString($sha.ComputeHash($stream)) -replace "-", "").ToLower()
    } finally {
        $stream.Dispose()
    }
}

# Downloads the pinned file into $Directory and checks its digest. Returns the path, or $null.
function Get-PinnedDownload {
    param($Name, $Component, $Platform, $Directory)

    $fileName = Split-Path $Platform.url -Leaf
    $path = Join-Path $Directory $fileName

    Write-Step "downloading $Name $($Component.version)"
    Write-Step "  $($Platform.url)"
    $previous = $ProgressPreference
    $ProgressPreference = "SilentlyContinue"
    try {
        Invoke-WebRequest -Uri $Platform.url -OutFile $path -UseBasicParsing
    } finally {
        $ProgressPreference = $previous
    }

    $actual = Get-Sha256 $path
    $expected = $Platform.sha256.ToLower()
    if ($actual -ne $expected) {
        Write-Fail "SHA-256 mismatch for $fileName"
        Write-Fail "  expected $expected"
        Write-Fail "  actual   $actual"
        return $null
    }
    Write-Step "  sha256 verified"
    return $path
}

function New-TempDirectory {
    $path = Join-Path ([System.IO.Path]::GetTempPath()) ("eas-components-" + [Guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $path -Force | Out-Null
    return $path
}

# A release asset that is the executable itself, with no archive around it.
function Install-BinaryComponent {
    param($Name, $Component, $Platform)

    $tempRoot = New-TempDirectory
    try {
        $downloaded = Get-PinnedDownload -Name $Name -Component $Component -Platform $Platform -Directory $tempRoot
        if ($null -eq $downloaded) { return $false }

        $binary = (Get-BinaryNames $Component) | Select-Object -First 1
        Copy-Item -Path $downloaded -Destination (Join-Path $ToolsDir $binary) -Force
        Write-Step "  installed $binary"
        return $true
    } finally {
        Remove-Item -Path $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Install-ArchiveComponent {
    param($Name, $Component, $Platform)

    $tempRoot = New-TempDirectory
    try {
        $archivePath = Get-PinnedDownload -Name $Name -Component $Component -Platform $Platform -Directory $tempRoot
        if ($null -eq $archivePath) { return $false }

        $extractDir = Join-Path $tempRoot "extracted"
        New-Item -ItemType Directory -Path $extractDir -Force | Out-Null
        if ($Platform.archive -ne "zip") {
            Write-Fail "unsupported archive type '$($Platform.archive)' for $Name on this platform"
            return $false
        }
        Expand-Archive -Path $archivePath -DestinationPath $extractDir -Force

        # A program that needs the libraries and data shipped beside it keeps its whole folder,
        # as tools\<name>\, rather than having its executable lifted out on its own.
        if ($Component.layout -eq "directory") {
            $binary = (Get-BinaryNames $Component) | Select-Object -First 1
            $found = Get-ChildItem -Path $extractDir -Filter $binary -Recurse -File | Select-Object -First 1
            if ($null -eq $found) {
                Write-Fail "  $binary was not present in the archive"
                return $false
            }
            $target = Join-Path $ToolsDir $Name
            if (Test-Path $target) { Remove-Item -Path $target -Recurse -Force }
            Copy-Item -Path $found.DirectoryName -Destination $target -Recurse -Force
            Write-Step "  installed $Name\ with $binary"
            return $true
        }

        # The layout inside upstream archives is not part of their contract, so search for the
        # binaries rather than assuming a bin/ subdirectory.
        $installed = 0
        foreach ($binary in (Get-BinaryNames $Component)) {
            $found = Get-ChildItem -Path $extractDir -Filter $binary -Recurse -File | Select-Object -First 1
            if ($null -eq $found) {
                Write-Fail "  $binary was not present in the archive"
                return $false
            }
            Copy-Item -Path $found.FullName -Destination (Join-Path $ToolsDir $binary) -Force
            Write-Step "  installed $binary"
            $installed++
        }

        # Matched by the end of their path for the same reason the binaries are searched for.
        foreach ($entry in (Get-DataEntries $Component)) {
            $suffix = "\" + ($entry.from -replace "/", "\")
            $found = Get-ChildItem -Path $extractDir -Recurse -Directory |
                Where-Object { $_.FullName.EndsWith($suffix, [StringComparison]::OrdinalIgnoreCase) } |
                Select-Object -First 1
            if ($null -eq $found) {
                Write-Fail "  $($entry.from)/ was not present in the archive"
                return $false
            }
            $target = Join-Path $InstallRoot ($entry.to -replace "/", "\")
            if (Test-Path $target) { Remove-Item -Path $target -Recurse -Force }
            New-Item -ItemType Directory -Path (Split-Path $target -Parent) -Force | Out-Null
            Copy-Item -Path $found.FullName -Destination $target -Recurse -Force
            Write-Step "  installed $($entry.to)/"
        }

        return ($installed -gt 0)
    } finally {
        Remove-Item -Path $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# Single files the component needs from somewhere other than its own download -- Piper's voice
# model, say -- each pinned by digest and installed under the install root.
function Install-Files {
    param($Name, $Component, $Platform)
    foreach ($file in (Get-Files $Component $Platform)) {
        $target = Join-Path $InstallRoot ($file.to -replace "/", "\")
        if ((Test-Path $target -PathType Leaf) -and (-not $Force)) { continue }
        $tempRoot = New-TempDirectory
        try {
            $downloaded = Join-Path $tempRoot "file"
            Write-Step "  downloading $($file.to)"
            $previous = $ProgressPreference
            $ProgressPreference = "SilentlyContinue"
            try {
                Invoke-WebRequest -Uri $file.url -OutFile $downloaded -UseBasicParsing
            } finally {
                $ProgressPreference = $previous
            }
            $actual = Get-Sha256 $downloaded
            if ($actual -ne $file.sha256.ToLower()) {
                Write-Fail "  SHA-256 mismatch for $($file.to): expected $($file.sha256), got $actual"
                return $false
            }
            New-Item -ItemType Directory -Path (Split-Path $target -Parent) -Force | Out-Null
            Move-Item -Path $downloaded -Destination $target -Force
            Write-Step "  installed $($file.to)"
        } finally {
            Remove-Item -Path $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
    return $true
}

function Write-Provenance {
    param($Installed)

    $sourcesPath = Join-Path $ToolsDir "SOURCES.txt"
    $lines = New-Object System.Collections.Generic.List[string]
    $lines.Add("Third-party binaries installed in this directory.")
    $lines.Add("Recorded $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss zzz') on $platformKey.")
    $lines.Add("")
    $lines.Add("These were downloaded from their own upstream projects and are NOT redistributed")
    $lines.Add("by EAS_Listener. Each remains under its own license, shown below.")
    $lines.Add("")

    foreach ($entry in $Installed) {
        $c = $entry.Component
        $lines.Add("$($entry.Name)  ($($c.license))")
        if ($c.version)     { $lines.Add("  version:  $($c.version)") }
        $lines.Add("  project:  $($c.project_url)")
        $lines.Add("  source:   $($c.source_url)")
        if ($entry.Url)     { $lines.Add("  url:      $($entry.Url)") }
        if ($entry.Sha256)  { $lines.Add("  sha256:   $($entry.Sha256)") }
        $lines.Add("  note:     $($c.license_note)")
        $lines.Add("")
    }

    Set-Content -Path $sourcesPath -Value $lines -Encoding UTF8
    Write-Step "provenance written to $sourcesPath"
}

$names = $manifest.components.PSObject.Properties.Name
if ($Component) {
    if ($names -notcontains $Component) {
        Write-Fail "unknown component '$Component'. Known: $($names -join ', ')"
        exit 1
    }
    $names = @($Component)
}

$installed = New-Object System.Collections.Generic.List[object]
$failed = New-Object System.Collections.Generic.List[string]
$manual = New-Object System.Collections.Generic.List[string]

# Note: PowerShell variable names are case-insensitive, so this loop must not use $component --
# that is the same variable as the [string]$Component parameter and the object would be coerced
# to a string on assignment.
foreach ($name in $names) {
    $spec = $manifest.components.$name

    if ($spec.install -in @("archive", "binary")) {
        $platform = $spec.platforms.$platformKey
        if ($null -eq $platform) {
            Write-Warn "$name has no build pinned for $platformKey; install it yourself and set its *_PATH key"
            if ($spec.feature) { Write-Host "    $($spec.feature)" }
            $manual.Add($name)
            continue
        }

        $record = [pscustomobject]@{ Name = $name; Component = $spec; Url = $platform.url; Sha256 = $platform.sha256 }
        if ((Test-AlreadyInstalled -Name $name -Component $spec -Platform $platform) -and (-not $Force)) {
            Write-Step "$name is already present; pass -Force to re-download"
            $installed.Add($record)
            continue
        }

        # The platform's own download decides: an archive is unpacked and anything else is the
        # executable itself, whatever the install type says.
        $ok = if (-not $platform.archive) {
            Install-BinaryComponent -Name $name -Component $spec -Platform $platform
        } else {
            Install-ArchiveComponent -Name $name -Component $spec -Platform $platform
        }
        if ($ok) { $ok = Install-Files -Name $name -Component $spec -Platform $platform }
        if ($ok) {
            Set-Content -Path (Get-StampPath $name) -Value $platform.sha256.ToLower() -Encoding ASCII -NoNewline
            $installed.Add($record)
        } else {
            $failed.Add($name)
        }
        continue
    }

    # "system" is a distro or Homebrew package elsewhere; Windows has no such thing to call.
    if ($spec.install -in @("manual", "system")) {
        $platform = $spec.platforms.$platformKey
        Write-Warn "$name must be installed manually."
        if ($platform.installer_url) { Write-Host "    Installer:  $($platform.installer_url)" }
        if ($platform.instructions)  { Write-Host "    $($platform.instructions)" }
        Write-Host "    Without it: $($spec.feature)"
        $manual.Add($name)
        continue
    }

    Write-Warn "$name has an unrecognised install type '$($spec.install)'; skipping"
}

if ($installed.Count -gt 0) { Write-Provenance $installed }

Write-Host ""
Write-Step "installed: $(if ($installed.Count) { ($installed.Name) -join ', ' } else { 'none' })"
if ($manual.Count -gt 0) { Write-Step "needs manual install: $($manual -join ', ')" }
if ($failed.Count -gt 0) {
    Write-Fail "failed: $($failed -join ', ')"
    exit 1
}
exit 0
