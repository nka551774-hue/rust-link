# start.bat delegates here so paths containing spaces and native exit codes work.
$ErrorActionPreference = 'Stop'
$serverArguments = $args
Set-Location -LiteralPath $PSScriptRoot

function Invoke-Checked {
    param([string]$Program, [string[]]$Arguments)
    & $Program @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Program failed with exit code $LASTEXITCODE" }
}

try {
    $packaged = Join-Path $PSScriptRoot 'rustlink.exe'
    if (!(Test-Path -LiteralPath 'Cargo.toml') -and (Test-Path -LiteralPath $packaged)) {
        & $packaged @serverArguments
        exit $LASTEXITCODE
    }
    if (!(Test-Path -LiteralPath 'Cargo.toml')) { throw 'Cargo.toml or a packaged rustlink.exe is required.' }

    $msysRoot = if ($env:RUSTLINK_MSYS2_ROOT) { $env:RUSTLINK_MSYS2_ROOT } else { 'C:\msys64' }
    $bash = Join-Path $msysRoot 'usr\bin\bash.exe'
    $ucrt = Join-Path $msysRoot 'ucrt64'
    $rustupBin = Join-Path $env:USERPROFILE '.cargo\bin'
    $env:PATH = "$ucrt\bin;$rustupBin;$env:PATH"

    if (!(Test-Path -LiteralPath $bash)) {
        if ($env:RUSTLINK_SKIP_BOOTSTRAP -eq '1') { throw 'Install MSYS2 from https://www.msys2.org/ or set RUSTLINK_MSYS2_ROOT.' }
        if (!(Get-Command winget.exe -ErrorAction SilentlyContinue)) { throw 'Install MSYS2 from https://www.msys2.org/ first; winget is unavailable.' }
        Write-Host 'Installing MSYS2 for the Rust GNU toolchain and libopus...'
        Invoke-Checked 'winget.exe' @('install','--id','MSYS2.MSYS2','--exact','--silent','--accept-package-agreements','--accept-source-agreements','--location',$msysRoot)
        if (!(Test-Path -LiteralPath $bash)) { throw "MSYS2 was not found at $msysRoot. Set RUSTLINK_MSYS2_ROOT to its location." }
    }

    $rustc = Get-Command rustc.exe -ErrorAction SilentlyContinue
    $hostTarget = if ($rustc) { (& $rustc.Source -vV | Select-String '^host: (.+)$').Matches.Groups[1].Value } else { '' }
    $hasGnuRust = $hostTarget -eq 'x86_64-pc-windows-gnu' -and (Get-Command cargo.exe -ErrorAction SilentlyContinue)
    $hasOpus = Test-Path -LiteralPath (Join-Path $ucrt 'lib\libopus.dll.a')
    $hasCompiler = Test-Path -LiteralPath (Join-Path $ucrt 'bin\gcc.exe')
    if (!$hasGnuRust -or !$hasOpus -or !$hasCompiler) {
        if ($env:RUSTLINK_SKIP_BOOTSTRAP -eq '1') { throw 'Install MSYS2 UCRT64 Rust, GCC and Opus packages, or unset RUSTLINK_SKIP_BOOTSTRAP.' }
        Write-Host 'Installing Rust, GCC and Opus in MSYS2 (first run only)...'
        # MSYS2 core upgrades can require a second invocation before other packages.
        Invoke-Checked $bash @('-lc','pacman -Syu --noconfirm')
        Invoke-Checked $bash @('-lc','pacman -Syu --noconfirm')
        Invoke-Checked $bash @('-lc','pacman -S --noconfirm --needed mingw-w64-ucrt-x86_64-rust mingw-w64-ucrt-x86_64-gcc mingw-w64-ucrt-x86_64-opus mingw-w64-ucrt-x86_64-pkgconf')
        $env:PATH = "$ucrt\bin;$env:PATH"
    }
    $env:OPUS_LIB_DIR = Join-Path $ucrt 'lib'
    $hostTarget = (& rustc.exe -vV | Select-String '^host: (.+)$').Matches.Groups[1].Value
    if ($hostTarget -ne 'x86_64-pc-windows-gnu') { throw "Expected the Windows x64 GNU toolchain; found $hostTarget." }
    Write-Host 'Building Rustlink (the first build can take several minutes)...'
    $targetDir = Join-Path $PSScriptRoot 'target'
    Invoke-Checked 'cargo.exe' @('build','--release','--locked','--bin','rustlink','--target',$hostTarget,'--target-dir',$targetDir)
    $executable = Join-Path $targetDir "$hostTarget\release\rustlink.exe"
    & $executable @serverArguments
    exit $LASTEXITCODE
} catch {
    [Console]::Error.WriteLine("Rustlink: $($_.Exception.Message)")
    exit 1
}
