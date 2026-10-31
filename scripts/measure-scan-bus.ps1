# scripts/measure-scan-bus.ps1
# 扫描计数来自应用写的 ~/.tuvis/logs/scan_bus_stats.json（单行覆盖写，R4）：
# 取 before/after 的 total 差值；不再依赖 mam.log 文本行。
# CPU 同时报告两个口径：avgCPU = 仅 tuvis.exe；-Tree 时追加进程树
# （tuvis.exe + 其 msedgewebview2 子孙进程，F10 2026-10-30 裁决）。
param(
  [int]$ProcessId = 0,
  [ValidateRange(1, 86400)][int]$Seconds = 60,
  [string]$StatsPath = "$env:USERPROFILE\.tuvis\logs\scan_bus_stats.json",
  [switch]$Tree,
  # 基线对比用：0.6.3 没有 stats 文件，只测 CPU（跳过扫描计数与零计数门禁）
  [switch]$CpuOnly
)
$ErrorActionPreference = 'Stop'
if ($ProcessId -eq 0) {
  $ProcessId = (Get-Process tuvis -ErrorAction Stop | Select-Object -First 1).Id
}
if (-not $CpuOnly -and -not (Test-Path -LiteralPath $StatsPath)) {
  throw "stats 文件不存在: $StatsPath（确认应用已运行并完成首扫；可用 -StatsPath 指定影子目录）"
}

function Read-ScanStats {
  $raw = Get-Content -LiteralPath $StatsPath -Raw -Encoding UTF8
  if ([string]::IsNullOrWhiteSpace($raw)) {
    throw "stats 文件为空: $StatsPath"
  }
  return $raw | ConvertFrom-Json
}

# 进程树 CPU（秒）：tuvis.exe + 递归子孙里名字匹配 msedgewebview2* 的进程。
# 每次采样现取进程表（WebView2 可能在窗口间增删；两端口径的绝对差值即窗口内消耗）。
function Get-TreeCpuSeconds([int]$RootId) {
  $all = Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId, Name
  $ids = New-Object System.Collections.Generic.HashSet[int]
  [void]$ids.Add($RootId)
  $changed = $true
  while ($changed) {
    $changed = $false
    foreach ($proc in $all) {
      $childId = [int]$proc.ProcessId
      if ($ids.Contains([int]$proc.ParentProcessId) -and -not $ids.Contains($childId)) {
        if ($proc.Name -eq "tuvis.exe" -or $proc.Name -like "msedgewebview2*") {
          [void]$ids.Add($childId)
          $changed = $true
        }
      }
    }
  }
  $cpu = 0.0
  foreach ($procId in $ids) {
    $g = Get-Process -Id $procId -ErrorAction SilentlyContinue
    if ($null -ne $g) { $cpu += $g.TotalProcessorTime.TotalSeconds }
  }
  return @{ Cpu = $cpu; Count = $ids.Count }
}

if (-not $CpuOnly) { $before = Read-ScanStats }
$p = Get-Process -Id $ProcessId -ErrorAction Stop
$c1 = $p.TotalProcessorTime.TotalSeconds
if ($Tree) { $t1 = Get-TreeCpuSeconds $ProcessId }
$w1 = Get-Date
Start-Sleep -Seconds $Seconds
# 进程可能在采样期间退出：Get-Process 默认只报非终止错误，这里必须显式判空，
# 否则 $p 变成 $null、cpu 归一为 0，会在 ≤3% 门禁下静默假通过
$p = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
if ($null -eq $p) {
  throw "进程 $ProcessId 在测量期间退出，样本无效"
}
$c2 = $p.TotalProcessorTime.TotalSeconds
if ($Tree) { $t2 = Get-TreeCpuSeconds $ProcessId }
$w2 = Get-Date
$wall = ($w2 - $w1).TotalSeconds
$cpu = $c2 - $c1
if ($CpuOnly) {
  $line = ("pid={0} wall={1:N1}s cpu={2:N2}s avgCPU={3:N2}% mode=cpu_only" -f `
    $ProcessId, $wall, $cpu, (100 * $cpu / $wall))
} else {
  $after = Read-ScanStats
  if ([int]$after.total -lt [int]$before.total) {
    throw "stats.total 回退（$($before.total) → $($after.total)）：应用重启或文件被重置，样本无效"
  }
  $scans = [int]$after.total - [int]$before.total
  # 正向断言：长窗口零计数必须报错——路径配错/扫描停摆时不允许静默假通过
  if ($Seconds -ge 60 -and $scans -eq 0) {
    throw "窗口内未计到任何扫描（stats.total 未增长）：确认 -StatsPath 指向运行中应用的 $StatsPath"
  }
  $line = ("pid={0} wall={1:N1}s cpu={2:N2}s avgCPU={3:N2}% scans={4} scansPerMin={5:N1} stats_last_reason={6} stats_last_duration_ms={7} stats_process_refresh_ms={8} stats_updated_at={9}" -f `
    $ProcessId, $wall, $cpu, (100 * $cpu / $wall), $scans, ($scans / ($wall / 60.0)), $after.last_reason, $after.last_duration_ms, $after.last_process_refresh_ms, $after.updated_at)
}
if ($Tree) {
  $cpuTree = $t2.Cpu - $t1.Cpu
  $line += (" cpu_tree={0:N2}s avgCPU_tree={1:N2}% tree_procs={2}" -f `
    $cpuTree, (100 * $cpuTree / $wall), $t2.Count)
}
Write-Output $line
