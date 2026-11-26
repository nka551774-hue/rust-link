# One-click Rustlink node + Discord music bot launcher (Windows).
# Supports interactive prompts, saved settings in quickstart\data, or non-interactive
# env vars (DISCORD_BOT_TOKEN, RUSTLINK_PASSWORD, RUSTLINK_PORT, RUSTLINK_YOUTUBE_PROXY).

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$Root = (Get-Item -LiteralPath $PSScriptRoot).Parent.FullName
$Data = Join-Path $PSScriptRoot 'data'
$ConfigFile = Join-Path $Data 'config.yaml'
$EnvFile = Join-Path $Data '.env'
$PoTokenFile = Join-Path $Root 'potoken.yaml'
$Utf8 = New-Object Text.UTF8Encoding $false

function Write-PoTokenConfig([string]$Pot, [string]$Vd) {
    $p = $Pot.Replace("'", "''")
    $v = $Vd.Replace("'", "''")
    $body = "# YouTube Proof-of-Origin Token configuration (auto-reloaded on save without restart).`n# Supports YTDLnis (gvsPoToken / playerPoToken / subsPoToken / visitorData / playerClient) or raw YTDLnis paste.`ngvsPoToken: '$p'`nplayerPoToken: '$p'`nsubsPoToken: ''`nvisitorData: '$v'`nplayerClient: 'mweb'`n"
    [IO.File]::WriteAllText($PoTokenFile, $body, $Utf8)
}

function Fail([string]$Message) {
    Write-Host "error: $Message" -ForegroundColor Red
    exit 1
}

function Invoke-Checked([string]$Program, [string[]]$Arguments) {
    & $Program @Arguments
    if ($LASTEXITCODE -ne 0) { Fail "$Program failed with exit code $LASTEXITCODE" }
}

function Read-Secret([string]$Prompt) {
    $secure = Read-Host -Prompt $Prompt -AsSecureString
    $ptr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
    try { return [Runtime.InteropServices.Marshal]::PtrToStringBSTR($ptr) }
    finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($ptr) }
}

function Get-Saved([string]$Key) {
    if (!(Test-Path -LiteralPath $EnvFile)) { return '' }
    $escaped = [regex]::Escape($Key)
    foreach ($line in Get-Content -LiteralPath $EnvFile -Encoding UTF8) {
        if ($line -match "^\s*(?:export\s+)?$escaped\s*=\s*(.*)$") {
            $v = $Matches[1].Trim()
            if ($v.Length -ge 2 -and (($v.StartsWith('"') -and $v.EndsWith('"')) -or ($v.StartsWith("'") -and $v.EndsWith("'")))) {
                return $v.Substring(1, $v.Length - 2)
            }
            return $v
        }
    }
    return ''
}

function New-RandomPassword {
    $bytes = New-Object byte[] 16
    [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
    $pw = -join ($bytes | ForEach-Object { $_.ToString('x2') })
    Write-Host "Generated password: $pw"
    return $pw
}

$envToken = if ($env:DISCORD_BOT_TOKEN) { $env:DISCORD_BOT_TOKEN.Trim() } else { '' }
$hasSaved = (Test-Path -LiteralPath $ConfigFile) -and (Test-Path -LiteralPath $EnvFile)
$reuse = $false
if (-not $envToken -and $hasSaved) {
    $reuse = (Read-Host 'Saved settings found. Use them? [Y/n]') -notmatch '^(n|no)$'
}

if ($reuse) {
    $Password = Get-Saved 'RUSTLINK_PASSWORD'
    $Token = Get-Saved 'DISCORD_BOT_TOKEN'
    $savedUrl = Get-Saved 'RUSTLINK_URL'
    $uri = $null
    $Port = if ([uri]::TryCreate($savedUrl, [UriKind]::Absolute, [ref]$uri) -and $uri.Scheme -in @('http', 'https') -and $uri.Port -ge 1 -and $uri.Port -le 65535 -and $savedUrl -match ':(\d+)(?:[/\?#]|$)') { [string]$uri.Port } else { '' }
    if (-not $Password -or -not $Token -or $Port -notmatch '^\d+$') {
        Fail "saved settings in $Data are incomplete; delete that folder and run again"
    }
} else {
    if ($envToken) {
        $Token = $envToken
        $Password = if ($env:RUSTLINK_PASSWORD) { $env:RUSTLINK_PASSWORD } elseif ($hasSaved) { Get-Saved 'RUSTLINK_PASSWORD' } else { '' }
        if (-not $Password) { $Password = New-RandomPassword }
        $Port = if ($env:RUSTLINK_PORT) { $env:RUSTLINK_PORT.Trim() } else { '2333' }
        $Proxy = if ($null -ne $env:RUSTLINK_YOUTUBE_PROXY) { $env:RUSTLINK_YOUTUBE_PROXY.Trim() } else { '' }
        $PoToken = if ($null -ne $env:RUSTLINK_YOUTUBE_PO_TOKEN) { $env:RUSTLINK_YOUTUBE_PO_TOKEN.Trim() } else { '' }
        $VisitorData = if ($null -ne $env:RUSTLINK_YOUTUBE_VISITOR_DATA) { $env:RUSTLINK_YOUTUBE_VISITOR_DATA.Trim() } else { '' }
    } else {
        $Password = if ($env:RUSTLINK_PASSWORD) { $env:RUSTLINK_PASSWORD } else { Read-Secret 'Node password (empty = generate one)' }
        if (-not $Password) { $Password = New-RandomPassword }
        $Token = (Read-Secret 'Discord bot token').Trim()
        $Port = if ($env:RUSTLINK_PORT) { $env:RUSTLINK_PORT.Trim() } else { Read-Host 'Port for the node [2333]' }
        if (-not $Port) { $Port = '2333' }
        if ($null -ne $env:RUSTLINK_YOUTUBE_PROXY) {
            $Proxy = $env:RUSTLINK_YOUTUBE_PROXY.Trim()
        } else {
            $warp = ''
            $probe = New-Object Net.Sockets.TcpClient
            try { if ($probe.ConnectAsync('127.0.0.1', 40000).Wait(500)) { $warp = 'socks5://127.0.0.1:40000' } } catch {} finally { $probe.Close() }
            $Proxy = (Read-Host "YouTube proxy, e.g. socks5://127.0.0.1:40000 (empty = none) [$warp]").Trim()
            if (-not $Proxy) { $Proxy = $warp }
        }
        $PoToken = if ($null -ne $env:RUSTLINK_YOUTUBE_PO_TOKEN) { $env:RUSTLINK_YOUTUBE_PO_TOKEN.Trim() } elseif (!(Test-Path -LiteralPath $PoTokenFile)) { (Read-Host 'YouTube PO token for potoken.yaml (empty = fill potoken.yaml later)').Trim() } else { '' }
        $VisitorData = if ($null -ne $env:RUSTLINK_YOUTUBE_VISITOR_DATA) {
            $env:RUSTLINK_YOUTUBE_VISITOR_DATA.Trim()
        } elseif ($PoToken -and $PoToken -notmatch ':' -and -not $PoToken.StartsWith('{')) {
            (Read-Host 'YouTube visitorData for potoken.yaml (empty = auto)').Trim()
        } else { '' }
    }

    if ($Password -match '["\\\s]' -or $Password.Length -lt 8) { Fail 'password must be >= 8 characters without quotes, backslashes or spaces' }
    if (-not $Token -or $Token -match '\s') { Fail 'a valid Discord bot token without spaces is required' }
    if ($Port -notmatch '^\d+$' -or [int]$Port -lt 1 -or [int]$Port -gt 65535) { Fail 'port must be between 1 and 65535' }
    if ($Proxy -match '["\\\s]') { Fail 'proxy cannot contain quotes, backslashes or spaces' }
    if ($Proxy -and $Proxy -notmatch '^(https?|socks4a?|socks5h?)://') { Fail 'proxy must start with http://, https://, socks4://, socks4a://, socks5:// or socks5h://' }

    if ($PoToken -or $VisitorData -or !(Test-Path -LiteralPath $PoTokenFile)) {
        Write-PoTokenConfig $PoToken $VisitorData
    }
    $escapedPotFile = $PoTokenFile.Replace("'", "''")
    $extraYaml = @("youtubePoTokenFile: '$escapedPotFile'", 'youtubeRequirePoToken: true')
    if ($Proxy) { $extraYaml += "youtubeProxy: `"$Proxy`"" }
    $extraBlock = ($extraYaml -join "`n") + "`n"
    [IO.Directory]::CreateDirectory($Data) | Out-Null
    [IO.File]::WriteAllText($ConfigFile, "address: 127.0.0.1:$Port`npassword: `"$Password`"`nlogLevel: info`nlogDirectory: logs`ntoolsDirectory: .tools`nautoDownload: true`n$extraBlock", $Utf8)
    [IO.File]::WriteAllText($EnvFile, "DISCORD_BOT_TOKEN=$Token`nRUSTLINK_PASSWORD=$Password`nRUSTLINK_URL=http://127.0.0.1:$Port`n", $Utf8)
    Write-Host "Settings saved to $Data (YouTube PO token config: $PoTokenFile)"
}

if (!(Test-Path -LiteralPath $PoTokenFile)) { Write-PoTokenConfig '' '' }
$env:RUSTLINK_PASSWORD = $Password
$env:RUSTLINK_ADDRESS = "127.0.0.1:$Port"
$env:DISCORD_BOT_TOKEN = $Token
$env:RUSTLINK_URL = "http://127.0.0.1:$Port"
$env:RUSTLINK_YOUTUBE_PO_TOKEN_FILE = $PoTokenFile
$env:RUSTLINK_YOUTUBE_REQUIRE_PO_TOKEN = 'true'
$env:RUSTLINK_NO_PAUSE = '1'
Remove-Item Env:RUSTLINK_YOUTUBE_PO_TOKEN, Env:RUSTLINK_YOUTUBE_VISITOR_DATA, Env:RUSTLINK_YOUTUBE_PROXY -ErrorAction SilentlyContinue

$msysRoot = if ($env:RUSTLINK_MSYS2_ROOT) { $env:RUSTLINK_MSYS2_ROOT } else { 'C:\msys64' }
$bash = Join-Path $msysRoot 'usr\bin\bash.exe'
$ucrt = Join-Path $msysRoot 'ucrt64'
$cargoBin = if ($env:USERPROFILE) { Join-Path $env:USERPROFILE '.cargo\bin' } else { '' }
$env:PATH = "$ucrt\bin;$cargoBin;$env:PATH"

$TargetDir = Join-Path $Root 'target'
$hostTarget = 'x86_64-pc-windows-gnu'
$Node = Join-Path $TargetDir "$hostTarget\release\rustlink.exe"
$Bot = Join-Path $TargetDir "$hostTarget\release\examples\music.exe"

if (Test-Path -LiteralPath (Join-Path $Root 'Cargo.toml')) {
    if (!(Test-Path -LiteralPath $bash)) {
        if ($env:RUSTLINK_SKIP_BOOTSTRAP -eq '1') { Fail 'Install MSYS2 from https://www.msys2.org/ or set RUSTLINK_MSYS2_ROOT.' }
        if (!(Get-Command winget.exe -ErrorAction SilentlyContinue)) { Fail 'Install MSYS2 from https://www.msys2.org/ first; winget is unavailable.' }
        Write-Host 'Installing MSYS2 for the Rust GNU toolchain and libopus...'
        Invoke-Checked 'winget.exe' @('install','--id','MSYS2.MSYS2','--exact','--silent','--accept-package-agreements','--accept-source-agreements','--location',$msysRoot)
        if (!(Test-Path -LiteralPath $bash)) { Fail "MSYS2 was not found at $msysRoot." }
    }
    if ((Test-Path -LiteralPath "$ucrt\bin\rustc.exe") -and (Test-Path -LiteralPath "$ucrt\bin\cargo.exe")) {
        Remove-Item Env:RUSTUP_TOOLCHAIN -ErrorAction SilentlyContinue
    } elseif ((Get-Command rustup.exe -ErrorAction SilentlyContinue) -and (& rustup.exe which --toolchain "stable-$hostTarget" rustc 2>$null)) {
        $env:RUSTUP_TOOLCHAIN = "stable-$hostTarget"
    } else {
        Remove-Item Env:RUSTUP_TOOLCHAIN -ErrorAction SilentlyContinue
    }

    $rustc = Get-Command rustc.exe -ErrorAction SilentlyContinue
    $detectedTarget = if ($rustc) { (& $rustc.Source -vV | Select-String '^host: (.+)$').Matches.Groups[1].Value } else { '' }
    if ($detectedTarget -ne $hostTarget -or !(Get-Command cargo.exe -ErrorAction SilentlyContinue) -or !(Test-Path -LiteralPath "$ucrt\lib\libopus.dll.a") -or !(Test-Path -LiteralPath "$ucrt\bin\gcc.exe")) {
        if ($env:RUSTLINK_SKIP_BOOTSTRAP -eq '1') { Fail 'Install MSYS2 UCRT64 Rust, GCC and Opus packages, or unset RUSTLINK_SKIP_BOOTSTRAP.' }
        Write-Host 'Installing Rust, GCC and Opus in MSYS2 (first run only)...'
        Invoke-Checked $bash @('-lc','pacman -Syu --noconfirm')
        Invoke-Checked $bash @('-lc','pacman -Syu --noconfirm')
        Invoke-Checked $bash @('-lc','pacman -S --noconfirm --needed mingw-w64-ucrt-x86_64-rust mingw-w64-ucrt-x86_64-gcc mingw-w64-ucrt-x86_64-opus mingw-w64-ucrt-x86_64-pkgconf')
        Remove-Item Env:RUSTUP_TOOLCHAIN -ErrorAction SilentlyContinue
        $env:PATH = "$ucrt\bin;$env:PATH"
    }
    $env:OPUS_LIB_DIR = "$ucrt\lib"
    Write-Host 'Building node and music bot (first build takes a few minutes)...'
    Push-Location -LiteralPath $Root
    try {
        Invoke-Checked 'cargo.exe' @('build','--release','--locked','--target',$hostTarget,'--target-dir',$TargetDir,'-p','rustlink','--bin','rustlink','-p','rustlink-discord-bot','--example','music')
    } finally { Pop-Location }
} elseif (!(Test-Path -LiteralPath $Node) -or !(Test-Path -LiteralPath $Bot)) {
    Fail 'Cargo.toml or prebuilt binaries are required'
}

$NodeLog = Join-Path $Data 'node.out.log'
$nodeProcess = $null
try {
    Write-Host "Starting node on 127.0.0.1:$Port (log: $NodeLog)..."
    $nodeProcess = Start-Process -FilePath $Node -ArgumentList '--config', 'config.yaml' -WorkingDirectory $Data -NoNewWindow -PassThru -RedirectStandardOutput $NodeLog -RedirectStandardError "$NodeLog.err"

    $deadline = (Get-Date).AddMinutes(5)
    $ready = $false
    while (-not $ready -and (Get-Date) -lt $deadline) {
        if ($nodeProcess.HasExited) {
            Get-Content -LiteralPath "$NodeLog.err", $NodeLog -Encoding UTF8 -ErrorAction SilentlyContinue | Select-Object -Last 15
            Fail "node stopped; is port $Port already in use?"
        }
        try {
            $req = [System.Net.WebRequest]::CreateHttp("http://127.0.0.1:$Port/version")
            $req.Proxy = $null; $req.Timeout = 2000; $req.ReadWriteTimeout = 2000
            $resp = [System.Net.HttpWebResponse]$req.GetResponse()
            try { $ready = [int]$resp.StatusCode -ge 200 -and [int]$resp.StatusCode -lt 300 } finally { $resp.Close() }
        } catch { Start-Sleep -Seconds 1 }
    }
    if (-not $ready) { Fail 'node did not answer within 5 minutes' }
    Write-Host 'Node ready. Starting music bot (Ctrl+C stops both)...'

    $quickFailures = 0
    while ($true) {
        $started = Get-Date
        Push-Location -LiteralPath $Data
        try { & $Bot '.env' } finally { Pop-Location }
        if ($LASTEXITCODE -eq 0) { break }
        if ($nodeProcess.HasExited) { Fail 'the node stopped' }
        if (((Get-Date) - $started).TotalSeconds -lt 30) { $quickFailures++ } else { $quickFailures = 0 }
        if ($quickFailures -ge 3) { Fail 'music bot failed repeatedly at startup; check DISCORD_BOT_TOKEN' }
        Write-Host 'Music bot exited; restarting in 5s...'
        Start-Sleep -Seconds 5
    }
} finally {
    if ($nodeProcess -and -not $nodeProcess.HasExited) {
        Stop-Process -Id $nodeProcess.Id -Force -ErrorAction SilentlyContinue
    }
}
