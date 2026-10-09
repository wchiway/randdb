#Requires -Version 5.1
<#
.SYNOPSIS
    Installs the RandDB MCP server from GitHub releases on Windows.

.DESCRIPTION
    Downloads the release archive for this machine, verifies it against the
    release's SHA256SUMS, installs randdb.exe, adds the install directory to the
    user PATH, and creates the configuration template unless -NoInit is given.

    Run it as a file, or piped into Invoke-Expression:

        powershell -ExecutionPolicy Bypass -File install.ps1
        irm https://raw.githubusercontent.com/wchiway/randdb/main/install.ps1 | iex

    Parameters cannot be passed through the iex form; use the environment
    variables RANDDB_VERSION, RANDDB_INSTALL_DIR, RANDDB_RELEASE_BASE_URL, and
    RANDDB_API_BASE_URL instead.

.PARAMETER Version
    Release tag to install, for example v0.1.0-alpha.1. Default: the newest
    release that provides a Windows x64 build.

.PARAMETER InstallDir
    Directory for randdb.exe. Default: %USERPROFILE%\.local\bin.

.PARAMETER ReleaseBaseUrl
    Release host, for mirrors. Default: https://github.com.

.PARAMETER ApiBaseUrl
    API host, for mirrors. Default: https://api.github.com.

.PARAMETER NoInit
    Do not create the configuration template (%USERPROFILE%\.randdb\.env).

.PARAMETER NoPathUpdate
    Do not add the install directory to the user PATH.
#>
[CmdletBinding()]
param(
    [string]$Version = $env:RANDDB_VERSION,
    [string]$InstallDir = $env:RANDDB_INSTALL_DIR,
    [string]$ReleaseBaseUrl = $env:RANDDB_RELEASE_BASE_URL,
    [string]$ApiBaseUrl = $env:RANDDB_API_BASE_URL,
    [switch]$NoInit,
    [switch]$NoPathUpdate
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Repo = 'wchiway/randdb'
$Bin = 'randdb'

if (-not $ReleaseBaseUrl) { $ReleaseBaseUrl = 'https://github.com' }
$ReleaseBaseUrl = $ReleaseBaseUrl.TrimEnd('/')
if (-not $ApiBaseUrl) { $ApiBaseUrl = 'https://api.github.com' }
$ApiBaseUrl = $ApiBaseUrl.TrimEnd('/')
$ApiBase = "$ApiBaseUrl/repos/$Repo"

function Write-Step {
    param([string]$Message)
    Write-Host $Message
}

function Write-Note {
    param([string]$Message)
    Write-Host "warning: $Message" -ForegroundColor Yellow
}

function Fail {
    param([string]$Message)
    Write-Host "error: $Message" -ForegroundColor Red
    throw $Message
}

function Get-AssetName {
    param([string]$Tag)
    "randdb-$Tag-$Target.tar.gz"
}

function Get-ReleaseTags {
    $releases = Invoke-RestMethod -Uri "$ApiBase/releases?per_page=30" `
        -Headers @{ Accept = 'application/vnd.github+json' }
    @($releases | ForEach-Object { $_.tag_name })
}

function Test-AssetExists {
    param([string]$Tag)
    $url = "$ReleaseBaseUrl/$Repo/releases/download/$Tag/$(Get-AssetName $Tag)"
    try {
        $response = Invoke-WebRequest -Uri $url -Method Head -UseBasicParsing
        return ($response.StatusCode -eq 200)
    } catch {
        $status = 0
        if ($_.Exception.Response) { $status = [int]$_.Exception.Response.StatusCode }
        if ($status -eq 404 -or $status -eq 403) { return $false }
        # Unknown failure, for example a mirror without HEAD support. Let the
        # download report the real problem.
        return $true
    }
}

function Add-UserPath {
    param([string]$Directory)
    $key = $null
    try {
        $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
    } catch {
        $key = $null
    }
    if ($key) {
        try {
            $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
            try { $kind = $key.GetValueKind('Path') } catch { }
            $current = $key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            if (-not $current) { $current = '' }
            $entries = @($current -split ';' | Where-Object { $_ })
            if ($entries -contains $Directory) { return $false }
            $key.SetValue('Path', ((@($entries) + $Directory) -join ';'), $kind)
            return $true
        } finally {
            $key.Close()
        }
    }
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $current) { $current = '' }
    $entries = @($current -split ';' | Where-Object { $_ })
    if ($entries -contains $Directory) { return $false }
    [Environment]::SetEnvironmentVariable('Path', ((@($entries) + $Directory) -join ';'), 'User')
    return $true
}

function Send-EnvironmentChanged {
    # Tell running processes that the user environment changed. Failure is not fatal.
    try {
        if (-not ('RandDB.NativeMethods' -as [type])) {
            Add-Type -Namespace RandDB -Name NativeMethods -MemberDefinition @'
[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
'@
        }
        $result = [UIntPtr]::Zero
        [RandDB.NativeMethods]::SendMessageTimeout([IntPtr]0xffff, 0x1A, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$result) | Out-Null
    } catch {
    }
}

# Only the target published by the release workflow is accepted; everything
# else has to build from source.
$arch = $env:PROCESSOR_ARCHITECTURE
if ($env:PROCESSOR_ARCHITEW6432) { $arch = $env:PROCESSOR_ARCHITEW6432 }
if ($arch -eq 'AMD64') {
    $Target = 'x86_64-pc-windows-msvc'
} else {
    Fail "no prebuilt binary for Windows/$arch
Prebuilt binaries exist for:
  Windows x86_64 (x86_64-pc-windows-msvc)
  Linux x86_64   (x86_64-unknown-linux-gnu)
  macOS arm64    (aarch64-apple-darwin)
Build from source instead:
  cargo install --git https://github.com/$Repo --locked"
}

if (-not $InstallDir) { $InstallDir = Join-Path $env:USERPROFILE '.local\bin' }
if ($InstallDir.Length -gt 3) { $InstallDir = $InstallDir.TrimEnd('\') }

if (-not $Version) {
    Write-Step "Looking up the newest release with a $Target build..."
    try {
        $tags = Get-ReleaseTags
    } catch {
        Fail "could not list releases from $ApiBase
Check your network connection, or pass -Version to install a specific tag."
    }
    if (-not $tags -or $tags.Count -eq 0) { Fail "no releases found in $Repo" }
    foreach ($tag in $tags) {
        if (Test-AssetExists $tag) {
            $Version = $tag
            break
        }
    }
    if (-not $Version) {
        Fail "no release provides a $Target build
Pass -Version to install a specific tag, or build from source."
    }
}

$asset = Get-AssetName $Version
$archiveUrl = "$ReleaseBaseUrl/$Repo/releases/download/$Version/$asset"
$sumsUrl = "$ReleaseBaseUrl/$Repo/releases/download/$Version/SHA256SUMS"

Write-Step "Installing RandDB $Version ($Target)"

$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("randdb-install-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tempDir -Force | Out-Null

try {
    $archivePath = Join-Path $tempDir $asset
    $sumsPath = Join-Path $tempDir 'SHA256SUMS'

    Write-Step "Downloading $asset..."
    try {
        Invoke-WebRequest -Uri $archiveUrl -OutFile $archivePath -UseBasicParsing
    } catch {
        Fail "could not download $archiveUrl
If the repository is private, the release assets are not publicly reachable."
    }
    try {
        Invoke-WebRequest -Uri $sumsUrl -OutFile $sumsPath -UseBasicParsing
    } catch {
        Fail "could not download $sumsUrl"
    }

    $expected = $null
    foreach ($line in Get-Content -LiteralPath $sumsPath) {
        $parts = $line -split '\s+'
        if ($parts.Count -ge 2 -and $parts[1].TrimStart('*') -eq $asset) {
            $expected = $parts[0]
            break
        }
    }
    if (-not $expected) { Fail "SHA256SUMS does not list $asset" }

    $actual = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash
    if ($actual -ne $expected) {
        Fail "checksum mismatch for $asset
expected $expected
got      $actual"
    }

    $tar = Get-Command tar.exe -ErrorAction SilentlyContinue
    if (-not $tar) {
        Fail "tar.exe is required to unpack the release archive; it ships with Windows 10 1803 and newer"
    }
    # Git Bash can put GNU tar ahead of Windows bsdtar on PATH. GNU tar treats
    # the colon in C:\... archive paths as a remote host, so use a relative name.
    Push-Location -LiteralPath $tempDir
    try {
        & $tar.Source -xzf "./$asset"
        if ($LASTEXITCODE -ne 0) { Fail "could not unpack $asset" }
    } finally {
        Pop-Location
    }

    $extracted = Join-Path $tempDir "$Bin.exe"
    if (-not (Test-Path -LiteralPath $extracted)) { Fail "$asset does not contain $Bin.exe" }

    if (-not (Test-Path -LiteralPath $InstallDir)) {
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    }
    $dest = Join-Path $InstallDir "$Bin.exe"

    # Stage the new binary next to the target so the swap is a single rename.
    $staged = Join-Path $InstallDir ".$Bin.new.$PID.exe"
    Copy-Item -LiteralPath $extracted -Destination $staged -Force
    try {
        Move-Item -LiteralPath $staged -Destination $dest -Force
    } catch {
        Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue
        Fail "could not replace $dest; stop any running randdb process and try again"
    }

    $runnable = $false
    try {
        $versionOutput = & $dest --version 2>&1
        $runnable = ($LASTEXITCODE -eq 0) -and $versionOutput
    } catch {
        $runnable = $false
    }
    if (-not $runnable) { Fail "$dest was installed but could not be executed" }

    if (-not $NoInit) {
        & $dest init
        if ($LASTEXITCODE -ne 0) { Write-Note "randdb init failed; run '$Bin init' manually" }
    }
} finally {
    Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
}

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not $userPath) { $userPath = '' }
$onPath = (@($userPath -split ';' | Where-Object { $_ }) -contains $InstallDir)

if (-not $onPath -and -not $NoPathUpdate) {
    if (Add-UserPath -Directory $InstallDir) {
        $onPath = $true
        $env:Path = "$env:Path;$InstallDir"
        Send-EnvironmentChanged
    }
}

Write-Host ''
Write-Host "RandDB $Version installed to $dest"

if (-not $onPath) {
    Write-Host ''
    Write-Host "$InstallDir is not on your PATH. Add it to your user PATH, then open a new terminal."
}

if ($onPath) { $mcpCommand = $Bin } else { $mcpCommand = $dest }
$jsonCommand = $mcpCommand -replace '\\', '\\'
$configDir = $env:RANDDB_HOME
if (-not $configDir) { $configDir = Join-Path $env:USERPROFILE '.randdb' }

Write-Host ''
Write-Host 'Next steps:'
if ($NoInit) {
    Write-Host "  1. Run '$Bin init' and add your API keys to $configDir\.env"
} else {
    Write-Host "  1. Add your API keys to $configDir\.env"
}
Write-Host '  2. Add the server to your MCP client:'
Write-Host ''
Write-Host '     {'
Write-Host '       "mcpServers": {'
Write-Host "         `"$Bin`": { `"command`": `"$jsonCommand`", `"args`": [`"mcp`"] }"
Write-Host '       }'
Write-Host '     }'
