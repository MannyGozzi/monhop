# Windows client for the agent relay (PROTOCOL.md). since.txt next to this script remembers the last delivered seq.
[CmdletBinding()]
param(
  [Parameter(Mandatory)][ValidateSet('send', 'wait', 'log', 'health')][string]$Action,
  [string]$File,
  [string]$Text,
  [int]$Since = -1,
  [int]$Timeout = 120
)
$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$tokenPath = Join-Path $here 'token.txt'
if (-not (Test-Path $tokenPath)) { throw "Write the relay token from the coordination Doc to $tokenPath first." }
$token = (Get-Content $tokenPath -Raw).Trim()
$hostPath = Join-Path $here 'host.txt'
if (-not (Test-Path $hostPath)) { throw "Write the Mac relay address to $hostPath first." }
$base = "http://$((Get-Content $hostPath -Raw).Trim()):24880/$token"
$sincePath = Join-Path $here 'since.txt'
$me = if ($IsMacOS -or $IsLinux) { 'mac' } else { 'win' }

function Get-LastSeq { if (Test-Path $sincePath) { [int](Get-Content $sincePath -Raw).Trim() } else { 0 } }
function Set-LastSeq([int]$seq) { Set-Content -Path $sincePath -Value $seq -NoNewline }
# Invoke-RestMethod can hand a JSON array back as one object; piping unrolls it into messages.
function Get-Messages([string]$url, [int]$timeoutSec) { @(Invoke-RestMethod -Uri $url -TimeoutSec $timeoutSec | ForEach-Object { $_ }) }
function Show-Messages([object[]]$items) {
  foreach ($m in $items) {
    if ($m.from -eq $me) { continue }
    Write-Output ("{0} relay #{1} {2}`n{3}`n" -f $m.from.ToUpper(), $m.seq, $m.at, $m.text)
  }
}
function Count-Theirs([object[]]$items) { @($items | Where-Object { $_.from -ne $me }).Count }
function Max-Seq([object[]]$items) { [int](($items | ForEach-Object { [int]$_.seq } | Measure-Object -Maximum).Maximum) }

switch ($Action) {
  'health' { Invoke-RestMethod -Uri "$base/health" -TimeoutSec 10 | ConvertTo-Json -Compress }
  'send' {
    if ($File) { $body = [System.IO.File]::ReadAllText($File) } elseif ($Text) { $body = $Text } else { throw 'send needs -File or -Text' }
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($body)
    $r = Invoke-RestMethod -Method Post -Uri "$base/send?from=$me" -ContentType 'text/plain; charset=utf-8' -Body $bytes -TimeoutSec 30
    Write-Output ("sent seq {0}" -f $r.seq)
  }
  'log' {
    $s = if ($Since -ge 0) { $Since } else { 0 }
    $items = Get-Messages "$base/log?since=$s" 30
    Show-Messages $items
    if ($items.Count) { Set-LastSeq (Max-Seq $items) }
  }
  'wait' {
    $since = if ($Since -ge 0) { $Since } else { Get-LastSeq }
    $deadline = (Get-Date).AddSeconds($Timeout)
    while ($true) {
      $left = [int][Math]::Ceiling(($deadline - (Get-Date)).TotalSeconds)
      if ($left -le 0) { Write-Output "no new message within $Timeout s (since $since)"; break }
      $items = Get-Messages "$base/wait?since=$since&timeout=$left" ($left + 30)
      if ($items.Count -eq 0) { continue }
      $newest = Max-Seq $items
      if ($newest -le $since) { Start-Sleep -Seconds 1; continue }
      Show-Messages $items
      $since = $newest
      Set-LastSeq $since
      if ((Count-Theirs $items) -gt 0) { break }
    }
  }
}
