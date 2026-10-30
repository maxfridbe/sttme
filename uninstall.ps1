#Requires -Version 5.1
<#
.SYNOPSIS
    Uninstaller for sttme on Windows.

.EXAMPLE
    irm https://raw.githubusercontent.com/maxfridbe/sttme/main/uninstall.ps1 | iex

.EXAMPLE
    .\uninstall.ps1 -Purge
#>
[CmdletBinding()]
param(
    [string]$BinDir = (Join-Path $env:LOCALAPPDATA "Programs\sttme"),
    [switch]$Purge
)

$ErrorActionPreference = "Stop"

$RunRegPath = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
$ShortcutPath = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\sttme.lnk"
$ConfigDir = Join-Path $env:APPDATA "sttme"
$CacheDir = Join-Path $env:LOCALAPPDATA "sttme"

Write-Host ""
Write-Host "Uninstalling sttme..." -ForegroundColor Cyan

$procs = Get-Process -Name "sttme" -ErrorAction SilentlyContinue
if ($procs) {
    $procs | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 350
    Write-Host "  [OK] Stopped running sttme tray process" -ForegroundColor Green
}

if (Test-Path $RunRegPath) {
    Remove-ItemProperty -Path $RunRegPath -Name "sttme" -ErrorAction SilentlyContinue
    Write-Host "  [OK] Removed Windows Sign-In startup entry" -ForegroundColor Green
}

if (Test-Path $ShortcutPath) {
    Remove-Item -Path $ShortcutPath -Force -ErrorAction SilentlyContinue
    Write-Host "  [OK] Removed Start Menu shortcut" -ForegroundColor Green
}

$exePath = Join-Path $BinDir "sttme.exe"
if (Test-Path $exePath) {
    Remove-Item -Path $exePath -Force
    Write-Host "  [OK] Removed $exePath" -ForegroundColor Green
}
if ((Test-Path $BinDir) -and -not (Get-ChildItem -Path $BinDir -ErrorAction SilentlyContinue)) {
    Remove-Item -Path $BinDir -Force -ErrorAction SilentlyContinue
}

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath) {
    $filtered = ($userPath -split ";" | Where-Object { $_ -and ($_.TrimEnd('\') -ne $BinDir.TrimEnd('\')) }) -join ";"
    if ($filtered -ne $userPath) {
        [Environment]::SetEnvironmentVariable("Path", $filtered, "User")
        Write-Host "  [OK] Removed $BinDir from User PATH" -ForegroundColor Green
    }
}

if ($Purge) {
    if (Test-Path $ConfigDir) {
        Remove-Item -Path $ConfigDir -Recurse -Force -ErrorAction SilentlyContinue
        Write-Host "  [OK] Purged config directory ($ConfigDir)" -ForegroundColor Green
    }
    if (Test-Path $CacheDir) {
        Remove-Item -Path $CacheDir -Recurse -Force -ErrorAction SilentlyContinue
        Write-Host "  [OK] Purged model & history cache ($CacheDir)" -ForegroundColor Green
    }
} else {
    Write-Host "  -> Kept config ($ConfigDir) and cached Whisper models ($CacheDir). Pass -Purge to remove."
}

Write-Host "  [OK] sttme uninstalled." -ForegroundColor Green
Write-Host ""
