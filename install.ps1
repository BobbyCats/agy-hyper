# AGY Hyper — Zero-Barrier Windows Installer (SHA-256 verified, atomic swap)
param(
  [switch]$Observe,
  [switch]$Uninstall,
  [switch]$Purge
)
$ErrorActionPreference = "Stop"

$Repo = if ($env:AGY_HYPER_REPO) { $env:AGY_HYPER_REPO } else { "BobbyCats/agy-hyper" }
$Version = if ($env:AGY_HYPER_VERSION) { $env:AGY_HYPER_VERSION } else { "latest" }
$PluginDir = if ($env:AGY_HYPER_PLUGIN_DIR) { $env:AGY_HYPER_PLUGIN_DIR } else { Join-Path $HOME ".gemini\config\plugins\agy-hyper" }
$DataDir = if ($env:AGY_HYPER_DATA_DIR) { $env:AGY_HYPER_DATA_DIR } else { Join-Path $HOME ".gemini\antigravity\agy-hyper" }

if ($Uninstall -or $env:AGY_HYPER_UNINSTALL -eq "1") {
  Write-Host "`n⚡ Uninstalling AGY Hyper..." -ForegroundColor Cyan
  $BinDir = Join-Path $PluginDir "bin"
  $BinExe = Join-Path $BinDir "agy-hyper.exe"
  if (Test-Path $BinExe) { & $BinExe daemon stop 2>$null | Out-Null }

  $PidFile = Join-Path $DataDir "run\sidecar.pid"
  if (Test-Path $PidFile) {
    $OldPid = (Get-Content -Raw $PidFile).Trim()
    if ($OldPid -match '^\d+$') {
      Stop-Process -Id ([int]$OldPid) -Force -ErrorAction SilentlyContinue
    }
  }

  if (Test-Path $PluginDir) {
    Remove-Item -Recurse -Force $PluginDir
    Write-Host "✓ Removed plugin directory: $PluginDir" -ForegroundColor Green
  }

  $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
  if ($UserPath) {
    $Filtered = ($UserPath -split ';' | Where-Object { $_ -and ($_ -ine $BinDir) }) -join ';'
    if ($Filtered -ne $UserPath) {
      [Environment]::SetEnvironmentVariable("Path", $Filtered, "User")
      Write-Host "✓ Removed $BinDir from User PATH" -ForegroundColor Green
    }
  }

  if ($Purge -or $env:AGY_HYPER_PURGE -eq "1") {
    if (Test-Path $DataDir) {
      Remove-Item -Recurse -Force $DataDir
      Write-Host "✓ Purged runtime state & WAL snapshots: $DataDir" -ForegroundColor Green
    }
  } else {
    Write-Host "ℹ Preserved user config, usage ledger & WAL snapshots at: $DataDir" -ForegroundColor Yellow
    Write-Host "  To remove all state as well, run: Remove-Item -Recurse -Force `"$DataDir`"" -ForegroundColor Yellow
  }
  Write-Host "✓ Uninstall complete." -ForegroundColor Cyan
  exit 0
}
$BaseUrl = if ($Version -eq "latest") {
  "https://github.com/$Repo/releases/latest/download"
} else {
  "https://github.com/$Repo/releases/download/$Version"
}

$Archive = "agy-hyper-x86_64-pc-windows-msvc.zip"
$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("agy-hyper-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $Tmp | Out-Null

try {
  Write-Host "`n⚡ Installing AGY Hyper ($Version)..." -ForegroundColor Cyan
  $ArchivePath = Join-Path $Tmp $Archive
  $SumsPath = Join-Path $Tmp "SHA256SUMS"
  Invoke-WebRequest -Uri "$BaseUrl/$Archive" -OutFile $ArchivePath
  Invoke-WebRequest -Uri "$BaseUrl/SHA256SUMS" -OutFile $SumsPath

  $Expected = (Get-Content $SumsPath | Where-Object { $_ -match "  $([regex]::Escape($Archive))$" } | Select-Object -First 1) -split '\s+' | Select-Object -First 1
  if (-not $Expected) { throw "Checksum entry missing for $Archive" }
  $Actual = (Get-FileHash -Algorithm SHA256 -Path $ArchivePath).Hash.ToLowerInvariant()
  if ($Actual -ne $Expected.ToLowerInvariant()) {
    throw "SHA-256 verification FAILED! Expected $Expected, got $Actual"
  }
  Write-Host "✓ SHA-256 verified ($($Actual.Substring(0,16))…)" -ForegroundColor Green

  $Stage = Join-Path $Tmp "stage"
  Expand-Archive -Path $ArchivePath -DestinationPath $Stage -Force
  $StagedPlugin = Join-Path $Stage "agy-hyper"

  # Ensure Windows configs point to ./bin/agy-hyper.exe before swapping into place
  foreach ($cfg in @("mcp_config.json", "hooks.json")) {
    $cfgPath = Join-Path $StagedPlugin $cfg
    if (Test-Path $cfgPath) {
      $txt = Get-Content -Raw $cfgPath
      $txt = $txt -replace '\./bin/agy-hyper(?![\w.])', './bin/agy-hyper.exe'
      Set-Content -Path $cfgPath -Value $txt -NoNewline
    }
  }

  New-Item -ItemType Directory -Force -Path (Split-Path $PluginDir) | Out-Null
  $OldBin = Join-Path $PluginDir "bin\agy-hyper.exe"
  if (Test-Path $OldBin) { & $OldBin daemon stop 2>$null | Out-Null }

  # Gracefully stop running sidecar before swapping directory
  $PidFile = Join-Path $HOME ".gemini\antigravity\agy-hyper\run\sidecar.pid"
  if (Test-Path $PidFile) {
    $OldPid = (Get-Content -Raw $PidFile).Trim()
    if ($OldPid -match '^\d+$') {
      Stop-Process -Id ([int]$OldPid) -Force -ErrorAction SilentlyContinue
    }
  }

  # Atomic backup-and-swap (restores previous installation if move fails)
  $BackupDir = "$PluginDir.bak.$PID"
  if (Test-Path $PluginDir) { Move-Item -Path $PluginDir -Destination $BackupDir -Force }
  try {
    Move-Item -Path $StagedPlugin -Destination $PluginDir -Force
    if (Test-Path $BackupDir) { Remove-Item -Recurse -Force $BackupDir -ErrorAction SilentlyContinue }
  } catch {
    if (Test-Path $BackupDir) { Move-Item -Path $BackupDir -Destination $PluginDir -Force -ErrorAction SilentlyContinue }
    throw
  }

  # Ensure CLI bin directory is on user PATH so `agy-hyper doctor|ui|bench` works in new terminals
  $BinDir = Join-Path $PluginDir "bin"
  $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
  if (-not ($UserPath -split ';' | Where-Object { $_ -ieq $BinDir })) {
    $NextPath = if ($UserPath) { "$UserPath;$BinDir" } else { $BinDir }
    [Environment]::SetEnvironmentVariable("Path", $NextPath, "User")
    $env:Path = "$env:Path;$BinDir"
  }

  if ($Observe -or $env:AGY_HYPER_OBSERVE -eq "1") {
    & (Join-Path $BinDir "agy-hyper.exe") config profile observe 2>$null | Out-Null
    Write-Host "✓ Profile set to 'observe' (zero-intrusion cockpit only; switch anytime in Hyper Deck)." -ForegroundColor Green
  }

  if (Get-Command agy -ErrorAction SilentlyContinue) {
    & agy plugin enable agy-hyper 2>$null | Out-Null
    Write-Host "✓ Detected Antigravity CLI (agy): enabled agy-hyper plugin." -ForegroundColor Green
    Write-Host "  Tip: run 'agy-hyper cli-setup --apply' to attach the live terminal statusline." -ForegroundColor DarkGray
  }

  & (Join-Path $BinDir "agy-hyper.exe") doctor
  Write-Host "⚡ Installed to $PluginDir" -ForegroundColor Cyan
} finally {
  Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}
