# scripts/measure-scan-bus.ps1
# 计数源：扫描总线每轮一行的 INFO `scan_bus: scan reason=…`（Fix 1 起常驻）。
# 不要改回 DEBUG 级的 adapter 行（默认 info 下会静默得到 scans=0 的假低值）。
param(
  [int]$ProcessId = 0,
  [ValidateRange(1, 86400)][int]$Seconds = 60,
  [string]$LogPath = "$env:USERPROFILE\.tuvis\logs\mam.log"
)
$ErrorActionPreference = 'Stop'
# 日志级别提示：扫描计数依赖 info 级；RUST_LOG 过滤掉 info 时这里能直接看出原因
$rustLog = if ($env:RUST_LOG) { $env:RUST_LOG } else { "<未设置，env_logger 默认 info>" }
Write-Output "rust_log=$rustLog"
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
# 扫描次数：每轮扫描一行 INFO `scan_bus: scan reason=…`（Fix 1 起）
$since = $w1.ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ss")
$lines = Get-Content -LiteralPath $LogPath -Tail 20000 -Encoding UTF8 | Where-Object {
  $_ -match "scan_bus: scan reason=" -and $_ -ge "[$since"
}
$scans = ($lines | Measure-Object).Count
# 正向断言：长窗口零计数必须报错——日志级别过滤/路径配错时不允许静默假通过
if ($Seconds -ge 60 -and $scans -eq 0) {
  throw "窗口内未计到任何扫描（scans=0）：确认 RUST_LOG 放行 info（当前 $rustLog）、日志路径 $LogPath 正确且应用在运行"
}
Write-Output ("pid={0} wall={1:N1}s cpu={2:N2}s avgCPU={3:N2}% scans={4} scansPerMin={5:N1}" -f `
  $ProcessId, $wall, $cpu, (100 * $cpu / $wall), $scans, ($scans / ($wall / 60.0)))
