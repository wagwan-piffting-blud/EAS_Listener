<#
.SYNOPSIS
Fetches Cepstral voice data for the cep6 engine into a writable directory.

.DESCRIPTION
The Windows counterpart of fetch_voices.sh. Voices already present are left alone, so this is
safe to run every time. The archive is a .7z, which the tar.exe built into Windows 10 and later
reads, so nothing needs installing.

.EXAMPLE
.\fetch_voices.ps1 -Directory C:\eas\data\tts_voices\cep6 Allison
#>
# Voices are the only positional arguments; without this, the first voice would be taken as -Url.
[CmdletBinding(PositionalBinding = $false)]
param(
    [string]$Directory = $env:CEP6_VOICE_DIR,
    [string]$Url = "",
    [string]$Sha256 = "",
    [Parameter(Mandatory = $true, Position = 0, ValueFromRemainingArguments = $true)]
    [string[]]$Voices
)

$ErrorActionPreference = "Stop"
$ArchiveName = "DASDEC_Cepstral_Voices.7z"
$DefaultUrl = "https://uploads.wagspuzzle.space/$ArchiveName"
$DefaultSha256 = "a43312ae597ca5078c584ad0bc4048be82ab07b528c45dfc6dee9b359ccba3a4"
$KnownVoices = @("Allison", "David", "Jean-Pierre", "William")

function Write-Log { param($m) Write-Host "cep6: $m" }
function Stop-WithError { param($m) Write-Host "cep6: $m" -ForegroundColor Red; exit 1 }

if (-not $Url) { $Url = if ($env:CEP6_VOICE_ARCHIVE_URL) { $env:CEP6_VOICE_ARCHIVE_URL } else { $DefaultUrl } }
if (-not $Sha256) { $Sha256 = if ($env:CEP6_VOICE_ARCHIVE_SHA256) { $env:CEP6_VOICE_ARCHIVE_SHA256 } else { $DefaultSha256 } }
if (-not $Directory) { Stop-WithError "No target directory. Pass -Directory DIR or set CEP6_VOICE_DIR." }

# A voice is only usable with all three of these present; a half-extracted directory is not
# treated as installed, so an interrupted run recovers on the next one.
function Test-VoiceInstalled {
    param($Dir)
    foreach ($file in @("settings.txt", "voice.idx", "voice_a.dat")) {
        $path = Join-Path $Dir $file
        if (-not (Test-Path $path -PathType Leaf) -or (Get-Item $path).Length -eq 0) { return $false }
    }
    return $true
}

$wanted = @()
foreach ($voice in $Voices) {
    if ($KnownVoices -notcontains $voice) {
        Stop-WithError "Unknown voice '$voice'. Known voices: $($KnownVoices -join ' ')"
    }
    if (Test-VoiceInstalled (Join-Path $Directory $voice)) {
        Write-Log "$voice is already installed in $Directory."
        continue
    }
    $wanted += $voice
}
if ($wanted.Count -eq 0) { exit 0 }

$tar = Join-Path $env:SystemRoot "System32\tar.exe"
if (-not (Test-Path $tar)) { Stop-WithError "tar.exe is missing; it ships with Windows 10 1803 and later." }

New-Item -ItemType Directory -Path $Directory -Force | Out-Null
$work = Join-Path $Directory ".incoming"
if (Test-Path $work) { Remove-Item -Path $work -Recurse -Force }
New-Item -ItemType Directory -Path $work -Force | Out-Null

try {
    $archive = $env:CEP6_VOICE_ARCHIVE_PATH
    if ($archive) {
        if (-not (Test-Path $archive -PathType Leaf)) { Stop-WithError "CEP6_VOICE_ARCHIVE_PATH=$archive does not exist." }
        Write-Log "using the local archive at $archive"
    } else {
        $archive = Join-Path $work $ArchiveName
        Write-Log "fetching $Url for: $($wanted -join ' ')"
        Write-Log "this is a ~337 MB download and runs once; the archive is discarded afterwards."
        $previous = $ProgressPreference
        $ProgressPreference = "SilentlyContinue"
        try {
            Invoke-WebRequest -Uri $Url -OutFile $archive -UseBasicParsing
        } finally {
            $ProgressPreference = $previous
        }
    }

    if ($Sha256 -and $Sha256 -ne "skip") {
        # .NET directly rather than Get-FileHash, which needs a module that a PowerShell started
        # from a PowerShell 7 environment cannot always load.
        $stream = [System.IO.File]::OpenRead($archive)
        try {
            $hasher = [System.Security.Cryptography.SHA256]::Create()
            $actual = ([System.BitConverter]::ToString($hasher.ComputeHash($stream)) -replace "-", "").ToLower()
        } finally {
            $stream.Dispose()
        }
        if ($actual -ne $Sha256.ToLower()) {
            Stop-WithError "Archive digest $actual does not match the expected $Sha256. Pass -Sha256 <digest> for a re-uploaded archive, or -Sha256 skip to accept it unchecked."
        }
    }

    $stage = Join-Path $work "stage"
    New-Item -ItemType Directory -Path $stage -Force | Out-Null
    foreach ($voice in $wanted) {
        Write-Log "extracting $voice"
        & $tar -xf $archive -C $stage "$voice/*"
        if ($LASTEXITCODE -ne 0) { Stop-WithError "Could not extract $voice from $archive." }
        if (-not (Test-VoiceInstalled (Join-Path $stage $voice))) { Stop-WithError "$voice came out of the archive incomplete." }
        $target = Join-Path $Directory $voice
        if (Test-Path $target) { Remove-Item -Path $target -Recurse -Force }
        Move-Item -Path (Join-Path $stage $voice) -Destination $target
        Write-Log "installed $voice in $target"
    }
} finally {
    Remove-Item -Path $work -Recurse -Force -ErrorAction SilentlyContinue
}
exit 0
