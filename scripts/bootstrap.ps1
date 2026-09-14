$ErrorActionPreference = 'Stop'
$monHopWorkspace = Split-Path -Parent $PSScriptRoot
$monHopTools = Join-Path $monHopWorkspace '.tools'
$monHopChannel = [regex]::Match([IO.File]::ReadAllText((Join-Path $monHopWorkspace 'rust-toolchain.toml')), 'channel\s*=\s*"([^"]+)"').Groups[1].Value
if ($monHopChannel -notmatch '^\d+\.\d+\.\d+$') { throw 'A pinned Rust toolchain is required.' }
New-Item -ItemType Directory -Path $monHopTools -Force | Out-Null
. "$PSScriptRoot\env.ps1"
$monHopRustup = Join-Path $env:CARGO_HOME 'bin\rustup.exe'
if (-not (Test-Path -LiteralPath $monHopRustup)) {
    $monHopInstaller = Join-Path $monHopTools 'rustup-init.exe'
    $monHopUrl = 'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe'
    Invoke-WebRequest -Uri $monHopUrl -OutFile $monHopInstaller
    $monHopChecksumResponse = (Invoke-WebRequest -Uri "$monHopUrl.sha256").Content
    $monHopChecksumText = if ($monHopChecksumResponse -is [byte[]]) { [Text.Encoding]::UTF8.GetString($monHopChecksumResponse) } else { [string]$monHopChecksumResponse }
    $monHopExpectedHash = ($monHopChecksumText -split '\s+')[0]
    if ($monHopExpectedHash -notmatch '^[0-9a-fA-F]{64}$' -or (Get-FileHash -LiteralPath $monHopInstaller -Algorithm SHA256).Hash -ne $monHopExpectedHash) { throw 'Rustup checksum verification failed.' }
    & $monHopInstaller -y --no-modify-path --profile minimal --default-toolchain $monHopChannel --component rustfmt --component clippy
    if ($LASTEXITCODE -ne 0) { throw 'Project-local Rust installation failed.' }
} else {
    & $monHopRustup toolchain install $monHopChannel --profile minimal --component rustfmt --component clippy
    if ($LASTEXITCODE -ne 0) { throw 'Project-local toolchain setup failed.' }
}
Write-Output 'Rust is available in .tools. The system PATH and startup settings were not changed.'
