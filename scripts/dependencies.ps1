param([switch]$Check)

. "$PSScriptRoot\env.ps1"
$monHopRoot = Split-Path -Parent $PSScriptRoot
$reportDirectory = Join-Path $monHopRoot 'docs\dependencies'
$python = $null
$pythonArguments = @()
foreach ($name in @('python', 'python3', 'py')) {
    # Store aliases may open an installer instead of running an interpreter.
    $python = Get-Command $name -CommandType Application -ErrorAction SilentlyContinue |
        Where-Object { $_.Source -notmatch '[/\\]Microsoft[/\\]WindowsApps[/\\](python3?|py)\.exe$' } |
        Select-Object -First 1
    if ($python) {
        if ($name -eq 'py') { $pythonArguments = @('-3') }
        break
    }
}
if (-not $python) { throw 'An installed Python 3.9 or newer interpreter is required to generate dependency reports.' }
& $python.Source @pythonArguments -c 'import sys; sys.exit(0 if sys.version_info.major == 3 and sys.version_info >= (3, 9) else 1)'
if ($LASTEXITCODE -ne 0) { throw 'The selected interpreter is not a supported Python 3.9 or newer executable.' }

Push-Location $monHopRoot
try {
    $reportArguments = @("$PSScriptRoot\dependencies.py")
    if ($Check) { $reportArguments += '--check' }
    & $python.Source @pythonArguments @reportArguments
    if ($LASTEXITCODE -ne 0) { throw 'Canonical dependency report generation failed.' }
    if ($Check) {
        Write-Output 'Checked canonical Cargo reports without rewriting tracked reports or the native DLL receipt.'
        return
    }

    $releaseExecutable = Join-Path $monHopRoot 'target\release\monhop.exe'
    if (Test-Path -LiteralPath $releaseExecutable) {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
        if (-not (Test-Path -LiteralPath $vswhere)) { throw 'Visual Studio discovery tool missing for native runtime inventory.' }
        $vsDirectory = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if ($LASTEXITCODE -ne 0 -or -not $vsDirectory) { throw 'Visual Studio C++ installation missing.' }
        $dumpbin = rg --files (Join-Path $vsDirectory 'VC\Tools\MSVC') -g dumpbin.exe | Where-Object { $_ -match 'Hostx64\\x64\\dumpbin.exe$' } | Sort-Object -Descending | Select-Object -First 1
        if (-not $dumpbin) { throw 'dumpbin missing for native runtime inventory.' }
        $imports = & $dumpbin /imports $releaseExecutable
        if ($LASTEXITCODE -ne 0) { throw 'Native runtime inventory failed.' }
        $systemDlls = @($imports | Where-Object { $_ -match '^\s+[^\s]+\.dll\s*$' } | ForEach-Object { $_.Trim().ToLowerInvariant() } | Sort-Object -Unique)
        $nativeHeader = @('Native DLL imports from the Windows MSVC release executable. Windows libraries are supplied by the OS. VCRUNTIME140.dll requires the Microsoft Visual C++ runtime already installed on the development host. MonHop never downloads runtime dependencies.', "Binary SHA256: $((Get-FileHash -LiteralPath $releaseExecutable -Algorithm SHA256).Hash)", '')
        $nativeHeader + $systemDlls | Set-Content -LiteralPath (Join-Path $reportDirectory 'runtime-system-windows.txt') -Encoding utf8
    }
} finally {
    Pop-Location
}
