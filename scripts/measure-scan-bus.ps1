# scripts/measure-scan-bus.ps1
param(
  [int]$ProcessId = 0,
  [ValidateRange(1, 86400)][int]$Seconds = 60,
  [string]$LogPath = "$env:USERPROFILE\.tuvis\logs\mam.log"
)
$ErrorActionPreference = 'Stop'
if ($ProcessId -eq 0) {
  $ProcessId = (Get-Process tuvis -ErrorAction Stop | Select-Object -First 1).Id
}
if (-not (Test-Path -LiteralPath $LogPath)) {
  throw "日志文件不存在: $LogPath"
}
$p = Get-Process -Id $ProcessId -ErrorAction Stop
$c1 = $p.TotalProcessorTime.TotalSeconds
$w1 = Get-Date
Start-Sleep -Seconds $Seconds
# 进程可能在采样期间退出：Get-Process 默认只报非终止错误，这里必须显式判空，
# 否则 $p 变成 $null、cpu 归一为 0，会在 ≤3% 门禁下静默假通过
$p = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
if ($null -eq $p) {
  throw "进程 $ProcessId 在测量期间退出，样本无效"
}
$c2 = $p.TotalProcessorTime.TotalSeconds
$w2 = Get-Date
$wall = ($w2 - $w1).TotalSeconds
$cpu = $c2 - $c1
# 扫描次数：日志中 adapter] OpenCode 行数（= 全量扫描轮数）
$since = $w1.ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ss")
$lines = Get-Content -LiteralPath $LogPath -Tail 20000 -Encoding UTF8 | Where-Object {
  $_ -match "adapter\] OpenCode:" -and $_ -ge "[$since"
}
$scans = ($lines | Measure-Object).Count
Write-Output ("pid={0} wall={1:N1}s cpu={2:N2}s avgCPU={3:N2}% scans={4} scansPerMin={5:N1}" -f `
  $ProcessId, $wall, $cpu, (100 * $cpu / $wall), $scans, ($scans / ($wall / 60.0)))
