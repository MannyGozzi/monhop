# Posts a message from the Windows agent to the Mac agent's mailbox (see README.md next to this file).
param(
    [string]$Text,
    [string]$File
)
$ErrorActionPreference = 'Stop'
if ($File) { $Text = Get-Content -Raw -LiteralPath $File }
if (-not $Text) { throw 'Pass -Text or -File.' }
$checkout = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$mail = Join-Path $checkout '.claude\state\mail'
New-Item -ItemType Directory -Force -Path $mail | Out-Null
$box = Join-Path $mail 'to-mac.jsonl'
$seq = if (Test-Path $box) { @(Get-Content $box).Count + 1 } else { 1 }
$commit = git -C $checkout rev-parse --short=9 HEAD
$line = [ordered]@{
    seq    = $seq
    at     = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    from   = 'win'
    commit = $commit
    text   = $Text.Trim()
} | ConvertTo-Json -Compress
[IO.File]::AppendAllText($box, $line + "`n", [Text.UTF8Encoding]::new($false))
"posted message $seq at $commit"
