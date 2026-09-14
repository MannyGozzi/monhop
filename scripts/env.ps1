$ErrorActionPreference = 'Stop'
$monHopRoot = Split-Path -Parent $PSScriptRoot
$env:CARGO_HOME = Join-Path $monHopRoot '.tools\cargo'
$env:RUSTUP_HOME = Join-Path $monHopRoot '.tools\rustup'
$monHopCargoBin = Join-Path $env:CARGO_HOME 'bin'
if (($env:PATH -split ';') -notcontains $monHopCargoBin) { $env:PATH = "$monHopCargoBin;$env:PATH" }
