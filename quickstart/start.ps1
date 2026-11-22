# One-click start for a Rustlink node plus the example music bot (Windows).
#
# Asks for the node password, the Discord bot token and the port, writes them
# to quickstart\data (ignored by git), builds both programs, starts the node and
# then the bot. Ctrl+C stops both.
#
#   powershell -ExecutionPolicy Bypass -File quickstart\start.ps1
#   (or double-click quickstart\start.bat)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
$Data = Join-Path $PSScriptRoot 'data'
$ConfigFile = Join-Path $Data 'config.yaml'
$EnvFile = Join-Path $Data '.env'

function Fail([string]$Message) {
    Write-Host "error: $Message" -ForegroundColor Red
    exit 1
}

function Read-Secret([string]$Prompt) {
    $secure = Read-Host -Prompt $Prompt -AsSecureString
    $pointer = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
    try { return [Runtime.InteropServices.Marshal]::PtrToStringBSTR($pointer) }
    finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($pointer) }
}

# Reads `KEY=value` from the saved .env, so a later start can reuse it.
function Get-Saved([string]$Key) {
    foreach ($line in Get-Content -LiteralPath $EnvFile) {
        if ($line -match "^$Key=(.*)$") { return $Matches[1] }
    }
    return ''
}

# --- Settings ---------------------------------------------------------------

$reuse = $false
if ((Test-Path -LiteralPath $ConfigFile) -and (Test-Path -LiteralPath $EnvFile)) {
    $answer = Read-Host 'Saved settings found. Use them? [Y/n]'
    $reuse = $answer -notmatch '^(n|no)$'
}

if ($reuse) {
    $Password = Get-Saved 'RUSTLINK_PASSWORD'
    $Token = Get-Saved 'DISCORD_BOT_TOKEN'
    $Port = (Get-Saved 'RUSTLINK_URL') -replace '^.*:(\d+)$', '$1'
    if (-not $Password -or -not $Token -or $Port -notmatch '^\d+$') {
        Fail "the saved settings in $Data are incomplete; delete that folder and start again"
    }
} else {
    $Password = Read-Secret 'Node password (empty = generate one)'
    if (-not $Password) {
        $bytes = New-Object byte[] 16
        [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
        $Password = -join ($bytes | ForEach-Object { $_.ToString('x2') })
        Write-Host "Generated password: $Password"
    }
    if ($Password -match '["\\\s]') { Fail 'the password cannot contain quotes, backslashes or spaces' }
    if ($Password.Length -lt 8) { Fail 'the password needs at least 8 characters' }

    $Token = (Read-Secret 'Discord bot token').Trim()
    if (-not $Token) { Fail 'a Discord bot token is required (Developer Portal -> Bot -> Reset Token)' }
    if ($Token -match '\s') { Fail 'the token cannot contain spaces' }

    $Port = Read-Host 'Port for the node [2333]'
    if (-not $Port) { $Port = '2333' }
    if ($Port -notmatch '^\d+$' -or [int]$Port -lt 1 -or [int]$Port -gt 65535) {
        Fail 'the port must be a number between 1 and 65535'
    }

    # YouTube refuses many server addresses ("Sign in to confirm you're not a
    # bot"); a proxy such as Cloudflare WARP gives it another one. A WARP proxy
    # already listening on its default port is offered as the default.
    $warp = ''
    try {
        $probe = New-Object Net.Sockets.TcpClient
        if ($probe.ConnectAsync('127.0.0.1', 40000).Wait(500)) { $warp = 'socks5://127.0.0.1:40000' }
        $probe.Close()
    } catch { }
    $Proxy = (Read-Host "YouTube proxy, e.g. socks5://127.0.0.1:40000 (empty = none) [$warp]").Trim()
    if (-not $Proxy) { $Proxy = $warp }
    if ($Proxy -match '["\\\s]') { Fail 'the proxy cannot contain quotes, backslashes or spaces' }
    if ($Proxy -and $Proxy -notmatch '^(https?|socks4|socks5h?)://') { Fail 'the proxy must start with http://, socks4://, socks5:// or socks5h://' }
    $proxyLine = if ($Proxy) { 'youtubeProxy: "' + $Proxy + '"' } else { '' }

    New-Item -ItemType Directory -Force -Path $Data | Out-Null
    $utf8 = New-Object Text.UTF8Encoding $false
    [IO.File]::WriteAllText($ConfigFile, @"
# Written by quickstart\start.ps1. Run it again to change these.
address: 127.0.0.1:$Port
password: "$Password"
logLevel: info
logDirectory: logs
toolsDirectory: .tools
autoDownload: true
$proxyLine
"@, $utf8)
    [IO.File]::WriteAllText($EnvFile, @"
# Written by quickstart\start.ps1. Keep this file private.
DISCORD_BOT_TOKEN=$Token
RUSTLINK_PASSWORD=$Password
RUSTLINK_URL=http://127.0.0.1:$Port
"@, $utf8)
    Write-Host "Settings saved to $Data"
}

# The files are the source of truth: a variable left in this shell would
# otherwise override them.
$env:RUSTLINK_PASSWORD = $Password
$env:RUSTLINK_ADDRESS = "127.0.0.1:$Port"
$env:DISCORD_BOT_TOKEN = $Token
$env:RUSTLINK_URL = "http://127.0.0.1:$Port"

# --- Build ------------------------------------------------------------------

# A windows-gnu toolchain builds and runs against MSYS2; use it when it is
# installed but not on PATH.
$msys = 'C:\msys64\ucrt64\bin'
if (-not (Get-Command gcc -ErrorAction SilentlyContinue) -and (Test-Path $msys)) {
    $env:PATH = "$msys;$env:PATH"
}

$Node = Join-Path $Root 'target\release\rustlink.exe'
$Bot = Join-Path $Root 'target\release\examples\music.exe'
if (Get-Command cargo -ErrorAction SilentlyContinue) {
    Write-Host 'Building (the first build takes a few minutes)...'
    Push-Location $Root
    try {
        cargo build --release -p rustlink --bin rustlink
        if ($LASTEXITCODE -ne 0) { Fail 'building the node failed' }
        cargo build --release -p rustlink-discord-bot --example music
        if ($LASTEXITCODE -ne 0) { Fail 'building the music bot failed' }
    } finally { Pop-Location }
} elseif (-not ((Test-Path $Node) -and (Test-Path $Bot))) {
    Fail 'Rust is not installed; get it from https://rustup.rs and run this again'
}

# --- Run --------------------------------------------------------------------

$NodeLog = Join-Path $Data 'node.out.log'
$nodeProcess = $null
try {
    Write-Host "Starting the node on 127.0.0.1:$Port (output: $NodeLog)..."
    $nodeProcess = Start-Process -FilePath $Node -ArgumentList '--config', 'config.yaml' `
        -WorkingDirectory $Data -NoNewWindow -PassThru `
        -RedirectStandardOutput $NodeLog -RedirectStandardError "$NodeLog.err"

    # The first start downloads FFmpeg before it listens, so allow a while.
    $deadline = (Get-Date).AddMinutes(5)
    $ready = $false
    while (-not $ready -and (Get-Date) -lt $deadline) {
        if ($nodeProcess.HasExited) {
            Get-Content -LiteralPath "$NodeLog.err", $NodeLog -ErrorAction SilentlyContinue | Select-Object -Last 15
            Fail "the node stopped; is port $Port already in use?"
        }
        try {
            Invoke-WebRequest -UseBasicParsing -TimeoutSec 2 "http://127.0.0.1:$Port/version" | Out-Null
            $ready = $true
        } catch { Start-Sleep -Seconds 1 }
    }
    if (-not $ready) { Fail 'the node did not answer within 5 minutes' }
    Write-Host 'Node is ready. Starting the music bot (Ctrl+C stops everything)...'

    # The bot exits when Discord drops its connection, so it is started again;
    # one that keeps failing at once (a wrong token) is not retried forever.
    $quickFailures = 0
    while ($true) {
        $started = Get-Date
        Push-Location $Data
        try { & $Bot '.env' } finally { Pop-Location }
        if ($LASTEXITCODE -eq 0) { break }
        if ($nodeProcess.HasExited) { Fail 'the node stopped' }
        if (((Get-Date) - $started).TotalSeconds -lt 30) { $quickFailures++ } else { $quickFailures = 0 }
        if ($quickFailures -ge 3) { Fail 'the music bot keeps failing at start; check the token and the message above' }
        Write-Host "The music bot stopped; starting it again in 5 seconds..."
        Start-Sleep -Seconds 5
    }
} finally {
    if ($nodeProcess -and -not $nodeProcess.HasExited) {
        Write-Host 'Stopping the node...'
        Stop-Process -Id $nodeProcess.Id -Force
    }
}
