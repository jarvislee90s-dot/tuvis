# scripts/measure-scan-bus.ps1
# 以稳定的 PID + 创建时刻身份测量 Tuvis 与可选 WebView2 子进程树。
param(
  [int]$ProcessId = 0,
  [ValidateRange(1, 86400)][int]$Seconds = 300,
  [ValidateRange(0, 86400)][int]$WarmupSeconds = 120,
  [ValidateRange(1, 20)][int]$Runs = 3,
  [ValidateRange(0, 10)][int]$MaxRetries = 3,
  [string]$StatsPath = "$env:USERPROFILE\.tuvis\logs\scan_bus_stats.json",
  [switch]$Tree,
  # 旧版本没有 runtime stats 时仅测 CPU，不伪报扫描计数或运行身份对账。
  [switch]$CpuOnly,
  # 只验证合成快照，不接触应用进程、用户 stats 或会话数据。
  [switch]$SelfTest
)
$ErrorActionPreference = 'Stop'

function Test-SnapshotDelta($Before, $After, [int]$RootId) {
  $beforeKeys = @($Before.Keys | Sort-Object)
  $afterKeys = @($After.Keys | Sort-Object)
  if ($beforeKeys.Count -ne $afterKeys.Count -or
      [string]::Join('|', $beforeKeys) -cne [string]::Join('|', $afterKeys)) {
    return @{ Valid = $false; Reason = 'process identity set changed' }
  }

  $root = $null
  $treeDelta = 0.0
  foreach ($key in $beforeKeys) {
    $delta = [double]$After[$key].CpuSeconds - [double]$Before[$key].CpuSeconds
    if ($delta -lt -0.000001) {
      return @{ Valid = $false; Reason = "CPU counter decreased for $key" }
    }
    $treeDelta += [Math]::Max(0.0, $delta)
    if ([int]$Before[$key].Pid -eq $RootId) { $root = [Math]::Max(0.0, $delta) }
  }
  if ($null -eq $root) { return @{ Valid = $false; Reason = 'root identity missing' } }
  return @{ Valid = $true; Reason = ''; RootCpuSeconds = [double]$root; TreeCpuSeconds = [double]$treeDelta }
}

function New-TestSnapshot([object[]]$Rows) {
  $snapshot = @{}
  foreach ($row in $Rows) {
    $key = '{0}|{1}' -f $row.Pid, $row.Creation
    $snapshot[$key] = @{ Pid = [int]$row.Pid; Creation = [string]$row.Creation; CpuSeconds = [double]$row.Cpu }
  }
  return ,$snapshot
}

function Invoke-SelfTest {
  $before = New-TestSnapshot @(
    @{ Pid = 10; Creation = 'root-a'; Cpu = 10.0 },
    @{ Pid = 11; Creation = 'child-a'; Cpu = 3.0 }
  )
  $stable = New-TestSnapshot @(
    @{ Pid = 10; Creation = 'root-a'; Cpu = 11.5 },
    @{ Pid = 11; Creation = 'child-a'; Cpu = 3.25 }
  )
  $delta = Test-SnapshotDelta $before $stable 10
  if (-not $delta.Valid -or [Math]::Abs($delta.RootCpuSeconds - 1.5) -gt 0.000001 -or
      [Math]::Abs($delta.TreeCpuSeconds - 1.75) -gt 0.000001) {
    throw 'SelfTest failed: stable tree did not produce per-process increments'
  }

  $removed = New-TestSnapshot @(@{ Pid = 10; Creation = 'root-a'; Cpu = 11.5 })
  if ((Test-SnapshotDelta $before $removed 10).Valid) { throw 'SelfTest failed: child removal was accepted' }

  $pidReused = New-TestSnapshot @(
    @{ Pid = 10; Creation = 'root-a'; Cpu = 11.5 },
    @{ Pid = 11; Creation = 'child-b'; Cpu = 0.1 }
  )
  if ((Test-SnapshotDelta $before $pidReused 10).Valid) { throw 'SelfTest failed: PID creation change was accepted' }

  $decreased = New-TestSnapshot @(
    @{ Pid = 10; Creation = 'root-a'; Cpu = 11.5 },
    @{ Pid = 11; Creation = 'child-a'; Cpu = 2.9 }
  )
  if ((Test-SnapshotDelta $before $decreased 10).Valid) { throw 'SelfTest failed: CPU counter decrease was accepted' }

  $added = New-TestSnapshot @(
    @{ Pid = 10; Creation = 'root-a'; Cpu = 11.5 },
    @{ Pid = 11; Creation = 'child-a'; Cpu = 3.25 },
    @{ Pid = 12; Creation = 'child-new'; Cpu = 0.01 }
  )
  if ((Test-SnapshotDelta $before $added 10).Valid) { throw 'SelfTest failed: child addition was accepted' }

  $allZero = @(
    [pscustomobject]@{ Value = 0.0 },
    [pscustomobject]@{ Value = 0.0 },
    [pscustomobject]@{ Value = 0.0 }
  )
  $zeroStability = Get-StabilityDeviation $allZero 'Value' 0.0
  if (-not $zeroStability.Undefined -or $zeroStability.Unstable) {
    throw 'SelfTest failed: all-zero measurements must have undefined stability'
  }

  $mixedZero = @(
    [pscustomobject]@{ Value = 0.0 },
    [pscustomobject]@{ Value = 0.0 },
    [pscustomobject]@{ Value = 2.0 }
  )
  $mixedStability = Get-StabilityDeviation $mixedZero 'Value' 0.0
  if (-not $mixedStability.Undefined -or -not $mixedStability.Unstable) {
    throw 'SelfTest failed: mixed zero-median measurements must be unstable'
  }

  $stableValues = @(
    [pscustomobject]@{ Value = 10.0 },
    [pscustomobject]@{ Value = 10.5 },
    [pscustomobject]@{ Value = 9.5 }
  )
  $positiveStability = Get-StabilityDeviation $stableValues 'Value' 10.0
  if ($positiveStability.Undefined -or $positiveStability.Unstable -or
      [Math]::Abs($positiveStability.Percent - 5.0) -gt 0.000001) {
    throw 'SelfTest failed: positive median deviation was calculated incorrectly'
  }

  $singleRun = @([pscustomobject]@{ Value = 10.0 })
  $singleRunStability = Get-StabilityDeviation $singleRun 'Value' 10.0
  if (-not $singleRunStability.Undefined -or $singleRunStability.Unstable) {
    throw 'SelfTest failed: one run cannot establish repeatability'
  }
  Write-Output 'SelfTest passed: process tree identity, CPU rollback, zero-median, and single-run stability cases.'
}

function Get-CanonicalPath([string]$Path) {
  if ([string]::IsNullOrWhiteSpace($Path)) { return '' }
  return [System.IO.Path]::GetFullPath($Path).TrimEnd('\').ToLowerInvariant()
}

function Read-ScanStats {
  $lastError = $null
  for ($attempt = 1; $attempt -le 5; $attempt++) {
    try {
      $raw = Get-Content -LiteralPath $StatsPath -Raw -Encoding UTF8
      if ([string]::IsNullOrWhiteSpace($raw)) { throw 'stats file is empty' }
      $value = $raw | ConvertFrom-Json
      if ($null -eq $value.runtime) { throw 'runtime metrics are missing' }
      return $value
    } catch {
      $lastError = $_.Exception.Message
      if ($attempt -lt 5) { Start-Sleep -Milliseconds 50 }
    }
  }
  throw "无法稳定读取 stats（5 次重试后仍为空/半份/缺 runtime）：$StatsPath；$lastError"
}

function Assert-StatsIdentity($Stats, [int]$ExpectedPid, [string]$ExpectedTarget, [string]$ExpectedRunId = '') {
  $runtime = $Stats.runtime
  if ([int]$runtime.processId -ne $ExpectedPid) {
    throw "stats PID 不匹配：期望 $ExpectedPid，实际 $($runtime.processId)"
  }
  if ([string]::IsNullOrWhiteSpace([string]$runtime.runId)) { throw 'stats runtime.runId 为空' }
  if ($ExpectedRunId -and ([string]$runtime.runId -cne $ExpectedRunId)) {
    throw "应用 runId 在测量期间变化（$ExpectedRunId → $($runtime.runId)）"
  }
  $statsTarget = Get-CanonicalPath ([string]$runtime.target)
  if (-not $statsTarget -or $statsTarget -cne $ExpectedTarget) {
    throw "stats 目标不匹配：期望 $ExpectedTarget，实际 $($runtime.target)"
  }
}

function Get-CurrentProcessRecord([int]$Id) {
  $process = Get-Process -Id $Id -ErrorAction SilentlyContinue
  if ($null -eq $process) { throw "进程 $Id 已退出" }
  try {
    $startTicks = $process.StartTime.ToUniversalTime().Ticks
    $cpu = $process.TotalProcessorTime.TotalSeconds
    $path = $process.Path
  } catch {
    throw "无法读取进程 $Id 的创建时刻/CPU/路径：$($_.Exception.Message)"
  }
  if ([string]::IsNullOrWhiteSpace($path)) { throw "无法读取进程 $Id 的可执行文件路径" }
  return @{ Pid = $Id; Creation = [string]$startTicks; CpuSeconds = [double]$cpu; Path = [string]$path }
}

function Get-ProcessTreeSnapshot([int]$RootId, [string]$ExpectedRootIdentity) {
  $all = @(Get-CimInstance Win32_Process -ErrorAction Stop | Select-Object ProcessId, ParentProcessId, Name)
  $ids = New-Object 'System.Collections.Generic.HashSet[int]'
  [void]$ids.Add($RootId)
  $changed = $true
  while ($changed) {
    $changed = $false
    foreach ($proc in $all) {
      $childId = [int]$proc.ProcessId
      if ($ids.Contains([int]$proc.ParentProcessId) -and -not $ids.Contains($childId) -and
          ($proc.Name -eq 'tuvis.exe' -or $proc.Name -like 'msedgewebview2*')) {
        [void]$ids.Add($childId)
        $changed = $true
      }
    }
  }

  $snapshot = @{}
  foreach ($id in $ids) {
    $record = Get-CurrentProcessRecord ([int]$id)
    if ($id -eq $RootId -and $record.Creation -cne $ExpectedRootIdentity) {
      throw "根进程 PID $RootId 已被复用，当前轮作废"
    }
    $key = '{0}|{1}' -f $record.Pid, $record.Creation
    $snapshot[$key] = @{ Pid = $record.Pid; Creation = $record.Creation; CpuSeconds = $record.CpuSeconds }
  }
  return ,$snapshot
}

function Get-RootSnapshot([int]$RootId, [string]$ExpectedRootIdentity) {
  $record = Get-CurrentProcessRecord $RootId
  if ($record.Creation -cne $ExpectedRootIdentity) { throw "根进程 PID $RootId 已退出或被复用" }
  $key = '{0}|{1}' -f $record.Pid, $record.Creation
  $snapshot = @{}
  $snapshot[$key] = @{ Pid = $record.Pid; Creation = $record.Creation; CpuSeconds = $record.CpuSeconds }
  return ,$snapshot
}

function Start-Measurement([int]$RootId, [string]$RootIdentity, [string]$TargetPath) {
  if (-not $CpuOnly) {
    $beforeStats = Read-ScanStats
    Assert-StatsIdentity $beforeStats $RootId $TargetPath
  } else { $beforeStats = $null }

  $previous = if ($Tree) { Get-ProcessTreeSnapshot $RootId $RootIdentity } else { Get-RootSnapshot $RootId $RootIdentity }
  $rootCpu = 0.0
  $treeCpu = 0.0
  $watch = [System.Diagnostics.Stopwatch]::StartNew()
  while ($watch.Elapsed.TotalSeconds -lt $Seconds) {
    $remainingMs = [Math]::Max(1, [int][Math]::Ceiling(($Seconds - $watch.Elapsed.TotalSeconds) * 1000.0))
    Start-Sleep -Milliseconds ([Math]::Min(1000, $remainingMs))
    $current = if ($Tree) { Get-ProcessTreeSnapshot $RootId $RootIdentity } else { Get-RootSnapshot $RootId $RootIdentity }
    $delta = Test-SnapshotDelta $previous $current $RootId
    if (-not $delta.Valid) { throw "样本无效：$($delta.Reason)" }
    $rootCpu += $delta.RootCpuSeconds
    $treeCpu += $delta.TreeCpuSeconds
    $previous = $current
  }
  $watch.Stop()
  $wall = $watch.Elapsed.TotalSeconds
  if ($wall -le 0) { throw 'Stopwatch wall time 无效' }

  $afterStats = $null
  $scans = $null
  if (-not $CpuOnly) {
    $afterStats = Read-ScanStats
    Assert-StatsIdentity $afterStats $RootId $TargetPath ([string]$beforeStats.runtime.runId)
    if ([int64]$afterStats.total -lt [int64]$beforeStats.total) {
      throw "stats.total 回退（$($beforeStats.total) → $($afterStats.total)），本轮作废"
    }
    $scans = [int64]$afterStats.total - [int64]$beforeStats.total
    if ($Seconds -ge 60 -and $scans -eq 0) { throw '窗口内扫描计数为零，本轮作废' }
  }
  return @{
    WallSeconds = $wall; RootCpuSeconds = $rootCpu; TreeCpuSeconds = $treeCpu
    RootAvgCpu = 100.0 * $rootCpu / $wall; TreeAvgCpu = 100.0 * $treeCpu / $wall
    TreeProcesses = $previous.Count; Scans = $scans; ScansPerMin = if ($null -eq $scans) { $null } else { $scans * 60.0 / $wall }
    BeforeStats = $beforeStats; AfterStats = $afterStats
  }
}

function Get-Median([double[]]$Values) {
  $sorted = @($Values | Sort-Object)
  $middle = [int][Math]::Floor($sorted.Count / 2)
  if (($sorted.Count % 2) -eq 1) { return [double]$sorted[$middle] }
  return ([double]$sorted[$middle - 1] + [double]$sorted[$middle]) / 2.0
}

function Get-StabilityDeviation($Samples, [string]$Property, [double]$Median) {
  $sampleCount = 0
  foreach ($sample in $Samples) { $sampleCount++ }
  if ($sampleCount -lt 2) {
    return @{ Percent = $null; Undefined = $true; Unstable = $false; Label = 'undefined (one run; repeatability unmeasured)' }
  }
  $values = @($Samples | ForEach-Object { [double]($_.$Property) })
  if ($Median -le 0) {
    $hasNonZero = @($values | Where-Object { [Math]::Abs($_) -gt 0 }).Count -gt 0
    $label = if ($hasNonZero) { 'undefined (zero median, mixed samples)' } else { 'undefined (all samples zero)' }
    return @{ Percent = $null; Undefined = $true; Unstable = $hasNonZero; Label = $label }
  }
  $maximum = 0.0
  foreach ($value in $values) {
    $deviation = 100.0 * [Math]::Abs($value - $Median) / $Median
    if ($deviation -gt $maximum) { $maximum = $deviation }
  }
  return @{ Percent = $maximum; Undefined = $false; Unstable = $maximum -gt 10; Label = "{0:N1}%" -f $maximum }
}

if ($SelfTest) { Invoke-SelfTest; exit 0 }

if ($ProcessId -eq 0) { $ProcessId = (Get-Process tuvis -ErrorAction Stop | Select-Object -First 1).Id }
$initial = Get-CurrentProcessRecord $ProcessId
$targetPath = Get-CanonicalPath $initial.Path
$rootIdentity = $initial.Creation
if (-not $CpuOnly -and -not (Test-Path -LiteralPath $StatsPath)) {
  throw "stats 文件不存在：$StatsPath（确认应用已运行并完成首扫；可用 -StatsPath 指定该应用的 stats 文件）"
}
if ($WarmupSeconds -gt 0) {
  Write-Output "warmup=${WarmupSeconds}s pid=$ProcessId target=$targetPath"
  $warmup = [System.Diagnostics.Stopwatch]::StartNew()
  while ($warmup.Elapsed.TotalSeconds -lt $WarmupSeconds) {
    Start-Sleep -Milliseconds ([Math]::Min(1000, [Math]::Max(1, [int](($WarmupSeconds - $warmup.Elapsed.TotalSeconds) * 1000))))
    $current = Get-CurrentProcessRecord $ProcessId
    if ($current.Creation -cne $rootIdentity) { throw "预热期间进程 $ProcessId 退出或被复用" }
  }
}

$results = New-Object System.Collections.Generic.List[object]
for ($run = 1; $run -le $Runs; $run++) {
  $success = $false
  for ($attempt = 0; $attempt -le $MaxRetries; $attempt++) {
    try {
      $result = Start-Measurement $ProcessId $rootIdentity $targetPath
      $results.Add($result)
      $retryLabel = if ($attempt -eq 0) { '' } else { " retries=$attempt" }
      $line = "run=$run/$Runs$retryLabel pid=$ProcessId target=$targetPath wall=$('{0:N1}' -f $result.WallSeconds)s rootCPU=$('{0:N2}' -f $result.RootCpuSeconds)s avgCPU=$('{0:N2}' -f $result.RootAvgCpu)%"
      if ($Tree) { $line += " treeCPU=$('{0:N2}' -f $result.TreeCpuSeconds)s avgCPU_tree=$('{0:N2}' -f $result.TreeAvgCpu)% treeProcs=$($result.TreeProcesses)" }
      if (-not $CpuOnly) {
        $runtime = $result.AfterStats.runtime
        $line += " scans=$($result.Scans) scansPerMin=$('{0:N1}' -f $result.ScansPerMin) runId=$($runtime.runId) action=$($runtime.actionScans) event=$($runtime.eventScans) periodic=$($runtime.periodicScans) coldStart=$($runtime.coldStartScans) lastDurationMs=$($runtime.lastDurationMs) failures=$($runtime.consecutiveFailures) backoffMs=$($runtime.retryBackoffRemainingMs) restarts=$($runtime.workerRestarts) mutationWaitP95Ms=$($runtime.mutationWaitP95Ms) mutationWaitSamples=$($runtime.mutationWaitSamples)"
      } else { $line += ' mode=cpu_only identity_stats=unchecked' }
      Write-Output $line
      $success = $true
      break
    } catch {
      if ($attempt -ge $MaxRetries) { throw "第 $run/$Runs 轮在 $($MaxRetries + 1) 次尝试后失败：$($_.Exception.Message)" }
      Write-Warning "第 $run/$Runs 轮样本作废（尝试 $($attempt + 1)/$($MaxRetries + 1)）：$($_.Exception.Message)；重新测量完整窗口"
      Start-Sleep -Milliseconds 250
    }
  }
  if (-not $success) { throw "第 $run/$Runs 轮没有有效样本" }
}

$medianRootCpu = Get-Median ([double[]]@($results | ForEach-Object { $_.RootAvgCpu }))
$rootStability = Get-StabilityDeviation $results 'RootAvgCpu' $medianRootCpu
$summary = "median_avgCPU=$('{0:N2}' -f $medianRootCpu)% maxDeviation=$($rootStability.Label)"
$stabilities = New-Object System.Collections.Generic.List[object]
$stabilities.Add($rootStability)
if ($Tree) {
  $medianTreeCpu = Get-Median ([double[]]@($results | ForEach-Object { $_.TreeAvgCpu }))
  $treeStability = Get-StabilityDeviation $results 'TreeAvgCpu' $medianTreeCpu
  $stabilities.Add($treeStability)
  $summary += " median_avgCPU_tree=$('{0:N2}' -f $medianTreeCpu)% treeMaxDeviation=$($treeStability.Label)"
}
if (-not $CpuOnly) {
  $medianScans = Get-Median ([double[]]@($results | ForEach-Object { $_.ScansPerMin }))
  $scanStability = Get-StabilityDeviation $results 'ScansPerMin' $medianScans
  $stabilities.Add($scanStability)
  $summary += " medianScansPerMin=$('{0:N1}' -f $medianScans)"
  $summary += " scansMaxDeviation=$($scanStability.Label)"
}
$anyUnstable = @($stabilities | Where-Object { $_.Unstable }).Count -gt 0
$anyUndefined = @($stabilities | Where-Object { $_.Undefined }).Count -gt 0
if ($anyUnstable) {
  $summary += ' stability=unstable (mixed zero-median samples or >10% from median; do not treat as pass)'
} elseif ($anyUndefined) {
  $undefinedReason = ($stabilities | Where-Object { $_.Undefined } | Select-Object -First 1).Label
  $summary += " stability=$undefinedReason; no stability pass"
} else { $summary += ' stability=within-10%-of-median' }
Write-Output $summary
