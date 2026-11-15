[CmdletBinding()]
param(
    [ValidateSet('debug', 'release')][string]$Mode = 'release',
    [string]$OutputDirectory = '',
    [string]$Target = ''
)
$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') { throw 'Run this script on Windows with the Rust MSVC toolchain and Windows SDK.' }
$projectRoot = Split-Path -Parent $PSScriptRoot
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $projectRoot 'dist' }
$OutputDirectory = [System.IO.Path]::GetFullPath($OutputDirectory)
if (-not $Target) {
    $rustInfo = & rustc -vV
    if ($LASTEXITCODE -ne 0) { throw 'rustc is unavailable.' }
    $Target = ($rustInfo | Select-String '^host: ').ToString().Substring(6).Trim()
}
if ($Target -notmatch '-pc-windows-msvc$') { throw 'A Windows MSVC target is required for the single executable build.' }
$oldFlags = $env:RUSTFLAGS
$oldEncodedFlags = $env:CARGO_ENCODED_RUSTFLAGS
Push-Location $projectRoot
try {
    # An explicit target keeps these flags off host proc-macro/build dependencies.
    if ($null -ne $oldEncodedFlags) {
        $separator = [char]31
        $extra = '-C' + $separator + 'target-feature=+crt-static'
        $env:CARGO_ENCODED_RUSTFLAGS = if ($oldEncodedFlags) { $oldEncodedFlags + $separator + $extra } else { $extra }
    } else {
        $env:RUSTFLAGS = "$oldFlags -C target-feature=+crt-static".Trim()
    }
    $cargoArgs = @('build', '--locked', '-p', 'sync-ssh', '--target', $Target)
    if ($Mode -eq 'release') { $cargoArgs += '--release' }
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { throw 'SyncSSH build failed.' }
    $targetDirectory = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $projectRoot 'target' }
    $binary = Join-Path $targetDirectory "$Target/$Mode/sync-ssh.exe"
    New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
    $staged = Join-Path $OutputDirectory ('.SyncSSH-' + [guid]::NewGuid().ToString('N') + '.exe')
    try {
        Copy-Item -LiteralPath $binary -Destination $staged
        Move-Item -LiteralPath $staged -Destination (Join-Path $OutputDirectory 'SyncSSH.exe') -Force
    } finally {
        if (Test-Path -LiteralPath $staged) { Remove-Item -LiteralPath $staged }
    }
    Write-Output "Built: $(Join-Path $OutputDirectory 'SyncSSH.exe')"
} finally {
    $env:RUSTFLAGS = $oldFlags
    $env:CARGO_ENCODED_RUSTFLAGS = $oldEncodedFlags
    Pop-Location
}
