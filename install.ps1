#Requires -Version 5.1
<#
.SYNOPSIS
    Installer & updater for sttme (Windows System Tray local Whisper push-to-talk).

.DESCRIPTION
    Installs sttme.exe into %LOCALAPPDATA%\Programs\sttme, verifies SHA-256 against
    GitHub Releases (or uses a bundled sttme.exe from an extracted release zip),
    adds sttme to the User PATH, pre-downloads the Whisper GGML model, creates a
    Start Menu shortcut, enables Launch at Windows Sign-In, and starts the tray app.

.EXAMPLE
    irm https://raw.githubusercontent.com/maxfridbe/sttme/main/install.ps1 | iex

.EXAMPLE
    .\install.ps1 -Model small
#>
[CmdletBinding()]
param(
    [string]$BinDir = (Join-Path $env:LOCALAPPDATA "Programs\sttme"),
    [string]$Model = "base",
    [string]$Version = "latest",
    [switch]$FromSource,
    [switch]$NoStartup,
    [switch]$NoLaunch,
    [switch]$Uninstall,
    [switch]$Purge
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$Repo = "maxfridbe/sttme"
$AssetName = "sttme-windows-x86_64.zip"
$RunRegPath = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
$ShortcutPath = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\sttme.lnk"
$ConfigDir = Join-Path $env:APPDATA "sttme"
$ConfigFile = Join-Path $ConfigDir "config.toml"
$CacheDir = Join-Path $env:LOCALAPPDATA "sttme"

function Write-Step([string]$Msg) {
    Write-Host "  -> " -ForegroundColor Cyan -NoNewline
    Write-Host $Msg
}

function Write-Ok([string]$Msg) {
    Write-Host "  [OK] " -ForegroundColor Green -NoNewline
    Write-Host $Msg
}

function Write-Warn([string]$Msg) {
    Write-Host "  [WARN] " -ForegroundColor Yellow -NoNewline
    Write-Host $Msg
}

function Stop-RunningSttme {
    $procs = Get-Process -Name "sttme" -ErrorAction SilentlyContinue
    if ($procs) {
        Write-Step "Stopping running sttme tray instance..."
        $procs | Stop-Process -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 350
    }
}

if ($Uninstall) {
    Write-Host ""
    Write-Host "Uninstalling sttme..." -ForegroundColor Cyan
    Stop-RunningSttme

    if (Test-Path $RunRegPath) {
        Remove-ItemProperty -Path $RunRegPath -Name "sttme" -ErrorAction SilentlyContinue
        Write-Ok "Removed Windows Sign-In startup entry"
    }

    if (Test-Path $ShortcutPath) {
        Remove-Item -Path $ShortcutPath -Force -ErrorAction SilentlyContinue
        Write-Ok "Removed Start Menu shortcut"
    }

    $exePath = Join-Path $BinDir "sttme.exe"
    if (Test-Path $exePath) {
        Remove-Item -Path $exePath -Force
        Write-Ok "Removed $exePath"
    }
    if ((Test-Path $BinDir) -and -not (Get-ChildItem -Path $BinDir -ErrorAction SilentlyContinue)) {
        Remove-Item -Path $BinDir -Force -ErrorAction SilentlyContinue
    }

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($userPath) {
        $filtered = ($userPath -split ";" | Where-Object { $_ -and ($_.TrimEnd('\') -ne $BinDir.TrimEnd('\')) }) -join ";"
        if ($filtered -ne $userPath) {
            [Environment]::SetEnvironmentVariable("Path", $filtered, "User")
            Write-Ok "Removed $BinDir from User PATH"
        }
    }

    if ($Purge) {
        if (Test-Path $ConfigDir) {
            Remove-Item -Path $ConfigDir -Recurse -Force -ErrorAction SilentlyContinue
            Write-Ok "Purged config directory ($ConfigDir)"
        }
        if (Test-Path $CacheDir) {
            Remove-Item -Path $CacheDir -Recurse -Force -ErrorAction SilentlyContinue
            Write-Ok "Purged model & history cache ($CacheDir)"
        }
    } else {
        Write-Step "Kept config ($ConfigDir) and cached Whisper models ($CacheDir). Pass -Purge to remove."
    }

    Write-Ok "sttme uninstalled."
    return
}

Write-Host ""
Write-Host "  sttme — Local Whisper Push-to-Talk for Windows (System Tray)" -ForegroundColor Cyan
Write-Host "  https://github.com/$Repo" -ForegroundColor DarkGray
Write-Host ""

Stop-RunningSttme
New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
$DestExe = Join-Path $BinDir "sttme.exe"

$LocalBundledExe = if ($PSScriptRoot) { Join-Path $PSScriptRoot "sttme.exe" } else { $null }

if ($FromSource) {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw "Rust 'cargo' not found in PATH. Install Rust via https://rustup.rs or omit -FromSource."
    }
    $env:GGML_NATIVE = "OFF"
    $env:GGML_AVX = "ON"
    $env:GGML_AVX2 = "ON"
    $env:GGML_FMA = "ON"
    $env:GGML_F16C = "ON"
    if ($PSScriptRoot -and (Test-Path (Join-Path $PSScriptRoot "Cargo.toml"))) {
        Write-Step "Building sttme.exe from local source with AVX2 Whisper backend..."
        Push-Location $PSScriptRoot
        try {
            & cargo build --release --locked
            if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
            Copy-Item -Path "target\release\sttme.exe" -Destination $DestExe -Force
        } finally {
            Pop-Location
        }
    } else {
        $tmpSrc = Join-Path ([System.IO.Path]::GetTempPath()) ("sttme-src-" + [guid]::NewGuid().ToString("N"))
        try {
            Write-Step "Cloning https://github.com/$Repo.git..."
            & git clone --depth 1 "https://github.com/$Repo.git" $tmpSrc
            if ($LASTEXITCODE -ne 0) { throw "git clone failed" }
            Push-Location $tmpSrc
            try {
                & cargo build --release --locked
                if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
                Copy-Item -Path "target\release\sttme.exe" -Destination $DestExe -Force
            } finally {
                Pop-Location
            }
        } finally {
            Remove-Item -Path $tmpSrc -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
    Write-Ok "Installed source build to $DestExe"
} elseif ($LocalBundledExe -and (Test-Path $LocalBundledExe) -and ($Version -eq "latest") -and ($LocalBundledExe -ne $DestExe)) {
    Write-Step "Installing bundled sttme.exe from extracted archive..."
    Copy-Item -Path $LocalBundledExe -Destination $DestExe -Force
    Write-Ok "Installed $DestExe"
} else {
    $baseUrl = if ($Version -eq "latest") {
        "https://github.com/$Repo/releases/latest/download"
    } else {
        $tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }
        "https://github.com/$Repo/releases/download/$tag"
    }

    $tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("sttme-install-" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null
    try {
        $zipPath = Join-Path $tmpDir $AssetName
        $sumsPath = Join-Path $tmpDir "SHA256SUMS"

        Write-Step "Downloading $AssetName ($Version) from GitHub Releases..."
        Invoke-WebRequest -Uri "$baseUrl/$AssetName" -OutFile $zipPath -UseBasicParsing

        try {
            Invoke-WebRequest -Uri "$baseUrl/SHA256SUMS" -OutFile $sumsPath -UseBasicParsing
            $expectedLine = Get-Content $sumsPath | Where-Object { $_ -match [regex]::Escape($AssetName) } | Select-Object -First 1
            if ($expectedLine) {
                $expectedHash = ($expectedLine -split '\s+')[0].Trim().ToLowerInvariant()
                $actualHash = (Get-FileHash -Path $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
                if ($expectedHash -ne $actualHash) {
                    throw "SHA-256 mismatch for $AssetName! Expected $expectedHash, got $actualHash"
                }
                Write-Ok "Verified SHA-256 ($($actualHash.Substring(0, 16))...)"
            }
        } catch {
            if ($_.Exception.Message -like "SHA-256 mismatch*") { throw }
            Write-Warn "SHA256SUMS check skipped: $($_.Exception.Message)"
        }

        $extractDir = Join-Path $tmpDir "extracted"
        Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force
        $foundExe = Get-ChildItem -Path $extractDir -Filter "sttme.exe" -Recurse | Select-Object -First 1
        if (-not $foundExe) {
            throw "sttme.exe not found inside $AssetName"
        }
        Copy-Item -Path $foundExe.FullName -Destination $DestExe -Force
        Write-Ok "Installed $DestExe"
    } finally {
        Remove-Item -Path $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# Add BinDir to User PATH if needed
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
$pathParts = if ($userPath) { $userPath -split ";" } else { @() }
$alreadyInPath = $pathParts | Where-Object { $_.TrimEnd('\') -ieq $BinDir.TrimEnd('\') }
if (-not $alreadyInPath) {
    $newPath = if ($userPath) { "$userPath;$BinDir" } else { $BinDir }
    [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    Write-Ok "Added $BinDir to User PATH"
}
if (($env:Path -split ";") -notcontains $BinDir) {
    $env:Path = "$BinDir;$env:Path"
}

# Write default config.toml if absent
if (-not (Test-Path $ConfigFile)) {
    New-Item -ItemType Directory -Path $ConfigDir -Force | Out-Null
    $chosenModel = if ($Model -and $Model -ne "none") { $Model } else { "base" }
    @"
[transcription]
mode = "local"
model = "$chosenModel"
language = "en"
energy_threshold = 0.008

[target]
hotkey = "f10"
alacritty_only = false
strip_alacritty_enter = true
input_method = "type"
typing_delay_ms = 2

[ui]
audio_beeps = true
desktop_notifications = true
waveform_overlay = true

[history]
limit = 20
"@ | Set-Content -Path $ConfigFile -Encoding UTF8
    Write-Ok "Created default configuration at $ConfigFile"
}

# Pre-download Whisper GGML model
if ($Model -and $Model -ne "none") {
    Write-Step "Ensuring Whisper GGML '$Model' model is cached locally..."
    & $DestExe download-model $Model
    if ($LASTEXITCODE -ne 0) {
        Write-Warn "Model pre-download returned non-zero; sttme will retry automatically when launched."
    } else {
        Write-Ok "Whisper '$Model' model cached and ready"
    }
}

# Create Start Menu shortcut
try {
    $wsh = New-Object -ComObject WScript.Shell
    $shortcut = $wsh.CreateShortcut($ShortcutPath)
    $shortcut.TargetPath = $DestExe
    $shortcut.WorkingDirectory = $BinDir
    $shortcut.Description = "sttme — Local Whisper Push-to-Talk (Hold F10)"
    $shortcut.Save()
    Write-Ok "Created Start Menu shortcut ($ShortcutPath)"
} catch {
    Write-Warn "Could not create Start Menu shortcut: $($_.Exception.Message)"
}

# Register Launch at Windows Sign-In
if (-not $NoStartup) {
    New-Item -Path $RunRegPath -Force -ErrorAction SilentlyContinue | Out-Null
    Set-ItemProperty -Path $RunRegPath -Name "sttme" -Value "`"$DestExe`""
    Write-Ok "Enabled Launch at Windows Sign-In (HKCU\...\Run\sttme)"
}

# Launch tray app now
if (-not $NoLaunch) {
    Start-Process -FilePath $DestExe -WorkingDirectory $BinDir
    Write-Ok "Launched sttme into the Windows System Tray!"
}

Write-Host ""
Write-Host "  Ready! Hold " -NoNewline
Write-Host "F10" -ForegroundColor Green -NoNewline
Write-Host " in any window, speak naturally, and release " -NoNewline
Write-Host "F10" -ForegroundColor Green -NoNewline
Write-Host " to type."
Write-Host "  Right-click the sttme tray icon anytime to change Hotkey, Whisper Model, HUD, or Startup settings." -ForegroundColor DarkGray
Write-Host ""
