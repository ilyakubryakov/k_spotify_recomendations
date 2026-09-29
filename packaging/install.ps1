<#
.SYNOPSIS
    spotify-agent installer for Windows -- strictly per-user, no administrator.

.DESCRIPTION
    Installs to %LOCALAPPDATA%\Programs\spotify-agent, adds that directory to
    the *user* PATH (HKCU, never the machine PATH), and can register a Task
    Scheduler entry that runs as the current user.

    By default it downloads a prebuilt binary from GitHub Releases and verifies
    it against the release's SHA256SUMS. Re-running it upgrades in place.

    Nothing here requires elevation. The script refuses to run elevated so a
    "Run as administrator" reflex cannot put the OAuth token store somewhere
    your normal session cannot read.

.PARAMETER Prefix
    Install directory. Default: %LOCALAPPDATA%\Programs\spotify-agent

.PARAMETER Schedule
    Register a scheduled task: Hourly, Daily or Weekly.

.PARAMETER Uninstall
    Remove the binary, the scheduled task and the PATH entry.

.PARAMETER FromSource
    Build with cargo instead of downloading a release asset.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File packaging\install.ps1

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File packaging\install.ps1 -Schedule Daily
#>
[CmdletBinding()]
param(
    [string]   $Prefix,
    # Deliberately NOT [ValidateSet]: when this script is piped through `iex`
    # the param block is evaluated in the caller's scope, and an attribute
    # validator rejects its own empty default with
    # "the attribute cannot be added because variable Schedule ... would no
    # longer be valid" -- which breaks the one-line install. Validated below.
    [string]   $Schedule,
    [switch]   $Uninstall,
    [switch]   $FromSource,
    [string]   $AssetBase = $env:SPOTIFY_AGENT_ASSET_BASE,
    [string]   $Repo      = $(if ($env:SPOTIFY_AGENT_REPO) { $env:SPOTIFY_AGENT_REPO } else { 'ilyakubryakov/k_spotify_recomendations' }),
    [string]   $Version   = $(if ($env:SPOTIFY_AGENT_VERSION) { $env:SPOTIFY_AGENT_VERSION } else { 'latest' }),
    [switch]   $NoVerify
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$App       = 'spotify-agent'
$TaskName  = 'spotify-agent daily curation'

# $PSScriptRoot is empty when this script is piped through `iex`, and
# Split-Path would throw on it under StrictMode. In that case there is no
# checkout to build from anyway, so an empty repo dir is the correct answer.
$RepoDir = if ($PSScriptRoot) { Split-Path -Parent $PSScriptRoot } else { '' }

if (-not $Prefix) {
    $Prefix = Join-Path $env:LOCALAPPDATA "Programs\$App"
}
$Target    = Join-Path $Prefix "$App.exe"
$ConfigDir = Join-Path $env:APPDATA "$App\config"
$DataDir   = Join-Path $env:APPDATA "$App\data"

function Write-Ok   ($m) { Write-Host "OK  $m" -ForegroundColor Green }
function Write-Warn ($m) { Write-Host "!   $m" -ForegroundColor Yellow }
function Write-Info ($m) { Write-Host "    $m" -ForegroundColor DarkGray }
# `throw` rather than `exit`: when this script is piped through `iex` it runs in
# the caller's scope, so `exit` would close the user's shell over something as
# ordinary as a 404. An unhandled throw still yields exit code 1 under
# `powershell -File`, which is what CI and the docs rely on.
function Die        ($m) { Write-Host "error: $m" -ForegroundColor Red; throw $m }

# ---------------------------------------------------------------------------
# Refuse elevation
# ---------------------------------------------------------------------------
# An elevated install writes the token store and SQLite cache under the
# Administrator profile, so the next non-elevated run would find no
# credentials and ask you to log in again -- a confusing failure to debug.
if ($Schedule -and $Schedule -notin @('Hourly', 'Daily', 'Weekly')) {
    Die "unknown -Schedule '$Schedule' (expected Hourly, Daily or Weekly)"
}

$identity  = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
if ($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator) -and
    -not $env:SPOTIFY_AGENT_ALLOW_ADMIN) {
    Die @"
this installer must NOT be run as administrator; $App is a per-user tool.
       Close this elevated window and run it from a normal PowerShell prompt.
       (set SPOTIFY_AGENT_ALLOW_ADMIN=1 to override)
"@
}

# ---------------------------------------------------------------------------
# Architecture
# ---------------------------------------------------------------------------
$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    'AMD64' { 'x86_64' }
    'ARM64' { 'aarch64' }
    default { 'x86_64' }
}
$Triple = "$arch-pc-windows-msvc"

# ---------------------------------------------------------------------------
# Scheduled task helpers
# ---------------------------------------------------------------------------
function Remove-AgentTask {
    $existing = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if ($existing) {
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Write-Ok "removed scheduled task '$TaskName'"
    }
}

function Register-AgentTask([string]$Interval) {
    $trigger = switch ($Interval) {
        'Hourly' {
            # No native hourly trigger; a daily trigger repeating every hour is
            # the documented equivalent.
            $t = New-ScheduledTaskTrigger -Once -At (Get-Date).Date.AddHours(7) `
                    -RepetitionInterval (New-TimeSpan -Hours 1)
            $t
        }
        'Daily'  { New-ScheduledTaskTrigger -Daily -At 7:30am }
        'Weekly' { New-ScheduledTaskTrigger -Weekly -DaysOfWeek Monday -At 7:30am }
    }

    $action = New-ScheduledTaskAction -Execute $Target -Argument '--cron generate' `
                -WorkingDirectory $Prefix

    # RunLevel Limited = the current user's normal token: registering and
    # running this needs no administrator rights.
    $settings = New-ScheduledTaskSettingsSet `
        -AllowStartIfOnBatteries `
        -DontStopIfGoingOnBatteries `
        -StartWhenAvailable `
        -RunOnlyIfNetworkAvailable `
        -ExecutionTimeLimit (New-TimeSpan -Hours 1) `
        -MultipleInstances IgnoreNew

    Remove-AgentTask
    Register-ScheduledTask -TaskName $TaskName -Trigger $trigger -Action $action `
        -Settings $settings -RunLevel Limited -User $env:USERNAME `
        -Description "Generates an AI-curated Spotify playlist." | Out-Null

    Write-Ok "registered the $Interval scheduled task (current user, no admin)"
    Write-Info "inspect: Get-ScheduledTask -TaskName '$TaskName'"
    Write-Info "run now: Start-ScheduledTask -TaskName '$TaskName'"
    Write-Warn "ANTHROPIC_API_KEY must be a *user* environment variable for the task to see it."
    Write-Info "set it with: [Environment]::SetEnvironmentVariable('ANTHROPIC_API_KEY','sk-ant-...','User')"
}

# ---------------------------------------------------------------------------
# PATH (user scope only)
# ---------------------------------------------------------------------------
function Add-ToUserPath([string]$Dir) {
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $current) { $current = '' }
    $entries = $current -split ';' | Where-Object { $_ -ne '' }
    if ($entries -contains $Dir) {
        Write-Info "$Dir is already on your user PATH"
        return
    }
    $updated = (@($entries) + $Dir) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $updated, 'User')
    # Update this session too, so the verification below works immediately.
    $env:Path = "$env:Path;$Dir"
    Write-Ok "added $Dir to your user PATH"
    Write-Info "open a new terminal for other sessions to pick it up"
}

function Remove-FromUserPath([string]$Dir) {
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $current) { return }
    $entries = $current -split ';' | Where-Object { $_ -ne '' -and $_ -ne $Dir }
    [Environment]::SetEnvironmentVariable('Path', ($entries -join ';'), 'User')
    Write-Ok "removed $Dir from your user PATH"
}

# ---------------------------------------------------------------------------
# Uninstall
# ---------------------------------------------------------------------------
if ($Uninstall) {
    Write-Host "Uninstalling $App..."
    Remove-AgentTask
    if (Test-Path $Target) {
        Remove-Item $Target -Force
        Write-Ok "removed $Target"
    } else {
        Write-Info "no binary at $Target"
    }
    Remove-FromUserPath $Prefix
    if ((Test-Path $Prefix) -and -not (Get-ChildItem $Prefix -Force)) {
        Remove-Item $Prefix -Force
    }
    Write-Host ''
    Write-Info 'Your config and cache were left in place:'
    Write-Info "  $ConfigDir"
    Write-Info "  $DataDir"
    exit 0
}

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------
Write-Host "Installing $App for $env:USERNAME on windows/$arch"
Write-Info "prefix  $Prefix"
Write-Info "config  $ConfigDir"
Write-Info "data    $DataDir"
Write-Host ''

New-Item -ItemType Directory -Force -Path $Prefix | Out-Null

function Get-ReleaseBase {
    if ($AssetBase) { return $AssetBase }
    if ($Version -eq 'latest') { return "https://github.com/$Repo/releases/latest/download" }
    return "https://github.com/$Repo/releases/download/$Version"
}

# Verify the archive against the release's SHA256SUMS.
#
# This is the security boundary for `irm ... | iex`: without it, anything that
# can intercept the download chooses what you run. A missing checksum is
# reported, never skipped silently.
function Test-Checksum([string]$Dir, [string]$Archive) {
    if ($NoVerify) { Write-Warn 'checksum verification disabled'; return $true }

    $sumsPath = Join-Path $Dir 'SHA256SUMS'
    try {
        Invoke-WebRequest -Uri "$(Get-ReleaseBase)/SHA256SUMS" -OutFile $sumsPath -UseBasicParsing
    } catch {
        Write-Warn 'no SHA256SUMS published for this release; cannot verify the download'
        return $false
    }

    $expected = $null
    foreach ($line in Get-Content $sumsPath) {
        $parts = $line -split '\s+', 2
        if ($parts.Count -eq 2 -and $parts[1].TrimStart('*') -eq $Archive) {
            $expected = $parts[0].ToLower()
            break
        }
    }
    if (-not $expected) {
        Write-Warn "$Archive is not listed in SHA256SUMS"
        return $false
    }

    $actual = (Get-FileHash (Join-Path $Dir $Archive) -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected) {
        Die @"
checksum mismatch for $Archive
       expected $expected
       actual   $actual
       Refusing to install. This is either a corrupted download or tampering.
"@
    }
    Write-Ok 'checksum verified'
    return $true
}

function Install-FromRelease {
    $archive = "$App-$Triple.zip"
    $url     = "$(Get-ReleaseBase)/$archive"
    $tmp     = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString())
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    try {
        Write-Host "Downloading $archive..."
        # TLS 1.2 is not the default on older Windows PowerShell hosts.
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        Invoke-WebRequest -Uri $url -OutFile (Join-Path $tmp $archive) -UseBasicParsing

        if (-not (Test-Checksum $tmp $archive)) { return $false }

        Expand-Archive -Path (Join-Path $tmp $archive) -DestinationPath $tmp -Force
        $exe = Get-ChildItem $tmp -Recurse -Filter "$App.exe" | Select-Object -First 1
        if (-not $exe) { return $false }
        Copy-Item $exe.FullName $Target -Force
        return $true
    } catch {
        Write-Info "release download failed: $($_.Exception.Message)"
        return $false
    } finally {
        Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Install-FromSource {
    if (-not $RepoDir -or -not (Test-Path (Join-Path $RepoDir 'Cargo.toml'))) {
        # Piped through `iex`, there is no checkout to build from.
        Write-Info 'no source checkout here; to build from source:'
        $repoDir = ($Repo -split '/')[-1]
        Write-Info "  git clone https://github.com/$Repo; cd $repoDir; .\packaging\install.ps1 -FromSource"
        return $false
    }

    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Write-Warn 'cargo was not found.'
        Write-Host 'Install the Rust toolchain (per-user, no admin) from https://rustup.rs'
        Write-Host '  winget install Rustlang.Rustup'
        return $false
    }

    # rusqlite compiles the bundled SQLite amalgamation, so the MSVC build
    # tools are required. Without them the link step fails with a confusing
    # "link.exe not found".
    if (-not (Get-Command link.exe -ErrorAction SilentlyContinue) -and
        -not (Get-Command cl.exe  -ErrorAction SilentlyContinue)) {
        Write-Warn 'MSVC build tools were not detected; SQLite is compiled from source.'
        Write-Host '  winget install Microsoft.VisualStudio.2022.BuildTools --override "--quiet --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"'
        Write-Host 'Then run this installer from a "Developer PowerShell for VS" prompt.'
        Write-Info 'Continuing anyway -- cargo may still find the toolchain.'
    }

    Write-Host 'Building from source (this takes a few minutes the first time)...'
    Push-Location $RepoDir
    try {
        & cargo build --release --locked
        if ($LASTEXITCODE -ne 0) {
            & cargo build --release
            if ($LASTEXITCODE -ne 0) { return $false }
        }
    } finally {
        Pop-Location
    }

    $built = Join-Path $RepoDir "target\release\$App.exe"
    if (-not (Test-Path $built)) { return $false }
    Copy-Item $built $Target -Force
    return $true
}

$installed = $false
if (-not $FromSource) { $installed = Install-FromRelease }
if (-not $installed)  { $installed = Install-FromSource }
if (-not $installed)  { Die "could not install $App -- see the messages above" }

Write-Ok "installed $Target"
& $Target --version | ForEach-Object { Write-Info $_ }

# ---------------------------------------------------------------------------
# Config scaffold
# ---------------------------------------------------------------------------
New-Item -ItemType Directory -Force -Path $ConfigDir, $DataDir | Out-Null
if (-not (Test-Path (Join-Path $ConfigDir 'config.toml'))) {
    & $Target config init 2>&1 | Out-Null
    if (Test-Path (Join-Path $ConfigDir 'config.toml')) {
        Write-Ok "wrote $ConfigDir\config.toml"
    } else {
        Write-Warn "could not write a starter config; run '$App config init' yourself"
    }
}

Add-ToUserPath $Prefix

if ($Schedule) {
    Write-Host ''
    Register-AgentTask $Schedule
}

Write-Host ''
Write-Host 'Next steps:'
Write-Host '  1. Create a Spotify app at https://developer.spotify.com/dashboard'
Write-Host '     and add this redirect URI:  http://127.0.0.1:8888/callback'
Write-Host "  2. Put the client id in $ConfigDir\config.toml (or set SPOTIFY_CLIENT_ID)"
Write-Host "  3. [Environment]::SetEnvironmentVariable('ANTHROPIC_API_KEY','sk-ant-...','User')"
Write-Host "  4. $App login"
Write-Host "  5. $App            # interactive TUI"

# NOTE: keep this file strictly ASCII. Windows PowerShell 5.1 decodes a
# BOM-less .ps1 with the system ANSI codepage, so a stray em-dash or ellipsis
# becomes mojibake and takes the *parser* down with it -- the error you get
# points at a random later line about a missing brace, not at the character.
# `tests/packaging.rs` enforces this.
