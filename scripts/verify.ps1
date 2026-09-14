param([switch]$Offline)
. "$PSScriptRoot\env.ps1"
$monHopPreviousOffline = $env:CARGO_NET_OFFLINE
Push-Location (Split-Path -Parent $PSScriptRoot)
try {
    if ($Offline) { $env:CARGO_NET_OFFLINE = 'true' }
    cargo fmt --all -- --check
    if ($LASTEXITCODE -ne 0) { throw 'Formatter check failed.' }
    cargo clippy --workspace --all-targets --locked -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'Clippy failed.' }
    cargo test --workspace --locked
    if ($LASTEXITCODE -ne 0) { throw 'Tests failed.' }
    cargo test -p tauri-runtime-wry --lib --locked navigation_tests
    if ($LASTEXITCODE -ne 0) { throw 'Desktop navigation tests failed.' }
    npm ci --no-fund --no-audit
    if ($LASTEXITCODE -ne 0) { throw 'npm ci failed.' }
    npm run --silent lint
    if ($LASTEXITCODE -ne 0) { throw 'oxlint failed.' }
    npm run --silent fmt:check
    if ($LASTEXITCODE -ne 0) { throw 'oxfmt check failed.' }
    npm run --silent test:ui
    if ($LASTEXITCODE -ne 0) { throw 'Setup UI model tests failed.' }
    python -m unittest discover -s scripts/tests -p test_icons.py
    if ($LASTEXITCODE -ne 0) { throw 'Logo asset tests failed.' }
    python -m unittest discover -s scripts/tests -p test_changelog.py
    if ($LASTEXITCODE -ne 0) { throw 'Changelog tests failed.' }
    python -m unittest discover -s scripts/tests -p test_release.py
    if ($LASTEXITCODE -ne 0) { throw 'Release script tests failed.' }
    cargo build --workspace --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
    if (-not $Offline) {
        cargo audit
        if ($LASTEXITCODE -ne 0) { throw 'Dependency vulnerability audit failed.' }
        cargo deny check licenses bans sources
        if ($LASTEXITCODE -ne 0) { throw 'Dependency license/source check failed.' }
    }
    python -m unittest discover -s scripts -p test_dependencies.py
    if ($LASTEXITCODE -ne 0) { throw 'Dependency report tests failed.' }
    & "$PSScriptRoot\dependencies.ps1" -Check
    if ($LASTEXITCODE -ne 0) { throw 'Dependency inventory failed.' }
} finally {
    if ($Offline) {
        if ($null -eq $monHopPreviousOffline) { Remove-Item Env:CARGO_NET_OFFLINE -ErrorAction SilentlyContinue }
        else { $env:CARGO_NET_OFFLINE = $monHopPreviousOffline }
    }
    Pop-Location
}
