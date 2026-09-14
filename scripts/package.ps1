. "$PSScriptRoot\env.ps1"
$monHopWorkspace = Split-Path -Parent $PSScriptRoot
Push-Location $monHopWorkspace
try {
    $monHopExecutable = Join-Path $monHopWorkspace 'target\release\monhop.exe'
    if (-not (Test-Path -LiteralPath $monHopExecutable)) { throw 'Run scripts/verify.ps1 before packaging.' }
    & "$PSScriptRoot\dependencies.ps1" -Check
    $monHopNativeReceipt = Join-Path $monHopWorkspace 'docs\dependencies\runtime-system-windows.txt'
    $monHopBinaryHash = (Get-FileHash -LiteralPath $monHopExecutable -Algorithm SHA256).Hash
    if (-not ((Get-Content -LiteralPath $monHopNativeReceipt) -contains "Binary SHA256: $monHopBinaryHash")) {
        throw 'Native DLL receipt is stale. Run scripts/dependencies.ps1 for this release executable before packaging.'
    }
    $monHopArtifacts = Join-Path $monHopWorkspace 'artifacts'
    New-Item -ItemType Directory -Path $monHopArtifacts -Force | Out-Null
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    function Write-MonHopArchive([string]$Path, [hashtable]$Entries) {
        $stream = [IO.File]::Open($Path, [IO.FileMode]::Create, [IO.FileAccess]::Write, [IO.FileShare]::None)
        $archive = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Create)
        try {
            foreach ($entry in ($Entries.Keys | Sort-Object)) {
                [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, $Entries[$entry], $entry, [IO.Compression.CompressionLevel]::Optimal) | Out-Null
            }
        } finally { $archive.Dispose(); $stream.Dispose() }
    }
    $monHopBundle = @{ 'MonHop/monhop.exe'=$monHopExecutable }
    foreach ($relative in @('LICENSE','README.md','SECURITY.md','TESTING.md','THIRD_PARTY_LICENSES.md','docs\MAC_HANDOFF.md','docs\dependencies\THIRD_PARTY_NOTICES.txt','docs\dependencies\sbom.cdx.json','docs\dependencies\license-report.json','docs\dependencies\runtime-x86_64-pc-windows-msvc.txt','docs\dependencies\runtime-system-windows.txt')) {
        $monHopBundle['MonHop/' + $relative.Replace('\','/')] = Join-Path $monHopWorkspace $relative
    }
    Write-MonHopArchive (Join-Path $monHopArtifacts 'MonHop-windows-x64-checkpoint.zip') $monHopBundle
    $monHopSource = @{}
    $monHopFiles = @(rg --files --hidden apps crates docs scripts vendor) + @('Cargo.toml','Cargo.lock','rust-toolchain.toml','.gitignore','.gitattributes','CLAUDE.md','AGENTS.md','LICENSE','README.md','ARCHITECTURE.md','SECURITY.md','TESTING.md','THIRD_PARTY_LICENSES.md','deny.toml')
    foreach ($relative in $monHopFiles) { $monHopSource['MonHop/' + $relative.Replace('\','/')] = Join-Path $monHopWorkspace $relative }
    Write-MonHopArchive (Join-Path $monHopArtifacts 'MonHop-source.zip') $monHopSource
    Get-FileHash -Algorithm SHA256 -LiteralPath $monHopExecutable,(Join-Path $monHopArtifacts 'MonHop-windows-x64-checkpoint.zip'),(Join-Path $monHopArtifacts 'MonHop-source.zip') |
        ForEach-Object { "$($_.Hash)  $([IO.Path]::GetFileName($_.Path))" } |
        Set-Content -LiteralPath (Join-Path $monHopArtifacts 'SHA256SUMS.txt') -Encoding utf8
    Write-Output 'Created Windows diagnostic checkpoint and portable source archives.'
} finally { Pop-Location }
