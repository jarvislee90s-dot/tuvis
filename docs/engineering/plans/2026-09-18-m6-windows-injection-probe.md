# M6 · Windows 终端注入探测 · 执行计划（探测任务书）

> **For agentic workers:** 本计划在 **Windows 电脑**上独立执行（执行者无项目会话上下文，本文完全自包含）。按任务顺序执行，步骤用 checkbox（`- [ ]`）跟踪。**这是探测任务，不是产品开发：只产出脚本、证据与报告，不修改任何 MAM 产品代码。**

**Goal:** 定案「外部进程向 Windows 终端里运行的 agent TUI 会话写入输入」的可行路径——GO / PARTIAL / NO-GO 三态结论 + 实施建议。

**Architecture:** 主路径 = `AttachConsole(pid)` + `WriteConsoleInput`（Win32 控制台输入 API，写入目标控制台输入缓冲区，理论上不需窗口聚焦）；对照宿主 = Windows Terminal（WT）与经典 conhost 两类；地面真值 = 目标 CLI 的**会话文件出现探测消息**（最强证据）；应急参照 = SendInput 全局键击（仅登记，不作为正式路径）。

**Tech Stack:** PowerShell 5.1+（Windows 自带）+ Add-Type 内嵌 C# P/Invoke（kernel32）；目标 CLI = claude / codex / kimi（本机已装、已登录）。

**Spec:** 本计划实现二期批次设计的 T1（M6）。上位文档（Mac 侧仓库，执行机不需要）：
`docs/engineering/specs/2026-09-18-phase2-m6-m9-injection-design.md` §3 T1 与 `docs/engineering/specs/2026-09-18-phase2-message-injection-design.md`（二期总 spec v1.4）W8、裁决 3（Windows 同期入二期，宪法 D18）。

## 前置事实（不需要复验，直接采信）

1. **AttachConsole 附加上位已实证**：PowerShell `AttachConsole(pid)` 对 claude（ConPTY 宿主）附加成功——本团队跳转 marker 三轮实机验收结论（2026-09-11/12，issue #43）。**本探测要补的是「附加后写入输入，TUI 是否真实收到」**。
2. 一个进程同一时刻只能附加一个控制台：附加前必须 `FreeConsole()` 脱离自己的控制台；脱离后本 PowerShell 的 stdout 失效，**所有输出必须走日志文件**。
3. Windows Terminal 没有官方脚本 API（无 macOS iTerm2 AppleScript 等价物），所以 WT 宿主下 ConPTY 输入缓冲区的可达性是本探测最大的不确定点。
4. MAM（Mac 侧设计）对多行消息的最终形态是**字面 `\n`（反斜杠+n 两个字符）拼接的单行文本 + 一次回车**——探测矩阵必须覆盖这个形态。

## Global Constraints（红线，全程有效）

1. **不修改 MAM 产品代码；不 git push；不建 PR；不动 main**——本探测只产出：脚本 + 台账 + 证据 + 报告。
2. **一次性测试会话**：所有探测在 `%TEMP%\mam-probe\proj-*\` 临时项目目录里新开的 claude/codex/kimi 会话上进行，**绝不向真实工作会话发送任何内容**；探测提示词要求 agent 最小回复（只回 `PROBE-OK`），控制 token 消耗。
3. **逐步台账**：每执行一步探测，在 `probe-log.md` 追加一行：`| 时间 | 任务# | 命令/动作 | 观察 | 判定（送达/未送达/报错） | 证据文件 |`。证据文件命名 `T<任务号>-<序号>-<短名>.txt/.png`。
4. **版本登记**（版本门控要求，宪法横切 6）：报告必须登记 OS 版本/内部版本、Windows Terminal 版本、默认终端应用设置、各 CLI 版本（`claude --version` 等）。
5. **双证据原则**：API 层成功（`WriteConsoleInput` 返回 TRUE + 写入计数）≠ 送达成功。**送达判定只认会话文件出现探测消息**（方法 A）；UIA/截图（方法 B）作为辅助。
6. 脚本与日志全部保留在探测目录，随报告一起回传——结论必须可复现。

## 文件结构（探测工作目录）

```
%USERPROFILE%\mam-probe\          # 探测根目录（固定，便于回传）
├── ConIn.ps1                     # 注入助手（Task 1 产出，唯一脚本）
├── probe-log.md                  # 探测台账（逐步追加）
├── evidence\                     # 证据目录（T*-*.txt / .png / 会话文件摘录）
├── proj-claude-conhost\          # 一次性测试项目目录（每宿主×CLI 一个）
├── proj-claude-wt\
├── proj-codex-*\  proj-kimi-*\
└── windows-终端注入-探测报告.md    # 最终报告（Task 7 按模板撰写）
```

---

### Task 0: 环境盘点与目录建立

**Files:** Create: `%USERPROFILE%\mam-probe\probe-log.md`、`evidence\`

- [ ] **Step 1: 建目录与台账**

```powershell
New-Item -ItemType Directory -Force -Path "$env:USERPROFILE\mam-probe\evidence" | Out-Null
Set-Content -Path "$env:USERPROFILE\mam-probe\probe-log.md" -Value "# M6 Windows 注入探测台账`n`n| 时间 | 任务# | 命令/动作 | 观察 | 判定 | 证据文件 |`n|---|---|---|---|---|---|"
```

- [ ] **Step 2: 版本与目标 CLI 盘点，结果存 `evidence\T0-env.txt`**

```powershell
& {
  "OS: $(Get-CimInstance Win32_OperatingSystem | ForEach-Object { $_.Caption + ' build ' + $_.BuildNumber })"
  "PSVersion: $($PSVersionTable.PSVersion)"
  "默认终端: $((Get-ItemProperty 'HKCU:\Console\%%Startup' -ErrorAction SilentlyContinue).DelegationConsole)"
  "WT 版本: $((Get-AppxPackage Microsoft.WindowsTerminal -ErrorAction SilentlyContinue).Version)"
  "claude: $(claude --version 2>&1)"
  "codex: $(codex --version 2>&1)"
  "kimi: $(kimi --version 2>&1)"
} *>&1 | Tee-Object "$env:USERPROFILE\mam-probe\evidence\T0-env.txt"
```

预期：三家 CLI 均有版本输出。任何一家未安装 → 台账登记「未装，跳过该家」，继续其余。

- [ ] **Step 3: 三家 CLI 可用性预检（防「未登录」误判为「注入不可达」）**

对每家 CLI 各开一个普通会话，手动发一句「你好」，确认能正常回复后退出。任何一家无法对话（未登录/无额度/网络）→ 台账登记「该家本次不可测」并继续其余——**绝不把登录问题记成注入失败**。

- [ ] **Step 4: 台账记一行 Task 0 完成与缺失项**

### Task 1: 注入助手脚本 ConIn.ps1

**Files:** Create: `%USERPROFILE%\mam-probe\ConIn.ps1`

**Interfaces（后续任务消费）:** `Send-ToConsole -TargetPid <uint32> -Text "<字符串>" [-Enter]` → 返回 `$true/$false`；日志固定追加至 `$env:USERPROFILE\mam-probe\conin.log`。字符串中 `\n` 是**字面反斜杠+n**（PowerShell 双引号内天然如此），助手不做任何转义/翻译——与 MAM 归一后的最终形态一致。

- [ ] **Step 1: 写入脚本（全文如下，一字不改）**

```powershell
# ConIn.ps1 — M6 探测：向目标进程所在控制台的输入缓冲区写入按键序列
# 用法：. "$env:USERPROFILE\mam-probe\ConIn.ps1"; Send-ToConsole -TargetPid <pid> -Text "..." -Enter
# 作用：FreeConsole → AttachConsole(目标) → GetStdHandle(STD_INPUT) → WriteConsoleInput(文本字符对 + 可选回车)
# 注意：调用后本 PowerShell 已脱离原控制台，stdout 可能失效——一切结果看 conin.log。
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
using System.Collections.Generic;

public static class ConIn
{
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool FreeConsole();

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool AttachConsole(uint dwProcessId);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern IntPtr GetStdHandle(int nStdHandle);

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool WriteConsoleInput(IntPtr hConsoleInput, INPUT_RECORD[] lpBuffer, uint nLength, out uint lpNumberOfEventsWritten);

    [StructLayout(LayoutKind.Sequential)]
    public struct KEY_EVENT_RECORD
    {
        public bool bKeyDown;
        public ushort wRepeatCount;
        public ushort wVirtualKeyCode;
        public ushort wVirtualScanCode;
        public char UnicodeChar;
        public uint dwControlKeyState;
    }

    [StructLayout(LayoutKind.Explicit)]
    public struct INPUT_RECORD
    {
        [FieldOffset(0)] public ushort EventType;
        [FieldOffset(4)] public KEY_EVENT_RECORD KeyEvent;
    }

    public const ushort KEY_EVENT_TYPE = 0x0001;
    public const ushort VK_RETURN = 0x0D;
    public const int STD_INPUT_HANDLE = -10;

    public static INPUT_RECORD[] TextToRecords(string text)
    {
        var records = new List<INPUT_RECORD>();
        foreach (char c in text)
        {
            records.Add(MakeRecord(true, 0, c));
            records.Add(MakeRecord(false, 0, c));
        }
        return records.ToArray();
    }

    public static INPUT_RECORD[] EnterRecords()
    {
        return new INPUT_RECORD[] { MakeRecord(true, VK_RETURN, '\r'), MakeRecord(false, VK_RETURN, '\r') };
    }

    private static INPUT_RECORD MakeRecord(bool down, ushort vk, char c)
    {
        var r = new INPUT_RECORD();
        r.EventType = KEY_EVENT_TYPE;
        r.KeyEvent.bKeyDown = down;
        r.KeyEvent.wRepeatCount = 1;
        r.KeyEvent.wVirtualKeyCode = vk;
        r.KeyEvent.UnicodeChar = c;
        return r;
    }
}
"@

function Write-Log([string]$Message) {
    Add-Content -Path "$env:USERPROFILE\mam-probe\conin.log" -Value ("{0} {1}" -f (Get-Date -Format o), $Message)
}

function Send-ToConsole {
    param(
        [Parameter(Mandatory = $true)][uint32]$TargetPid,
        [Parameter(Mandatory = $true)][string]$Text,
        [switch]$Enter
    )
    Write-Log ("=== target={0} textLen={1} enter={2}" -f $TargetPid, $Text.Length, [bool]$Enter)
    [void][ConIn]::FreeConsole()
    $attached = [ConIn]::AttachConsole($TargetPid)
    if (-not $attached) {
        Write-Log ("AttachConsole FAILED err={0}" -f [System.Runtime.InteropServices.Marshal]::GetLastWin32Error())
        return $false
    }
    Write-Log "AttachConsole OK"
    $handle = [ConIn]::GetStdHandle([ConIn]::STD_INPUT_HANDLE)
    $written = [uint32]0
    $textRecords = [ConIn]::TextToRecords($Text)
    $textOk = [ConIn]::WriteConsoleInput($handle, $textRecords, [uint32]$textRecords.Length, [ref]$written)
    Write-Log ("WriteConsoleInput text ok={0} written={1}/{2} err={3}" -f $textOk, $written, $textRecords.Length, [System.Runtime.InteropServices.Marshal]::GetLastWin32Error())
    $enterOk = $true
    if ($Enter) {
        $written2 = [uint32]0
        $enterRecords = [ConIn]::EnterRecords()
        $enterOk = [ConIn]::WriteConsoleInput($handle, $enterRecords, [uint32]$enterRecords.Length, [ref]$written2)
        Write-Log ("WriteConsoleInput enter ok={0} written={1}/{2}" -f $enterOk, $written2, $enterRecords.Length)
    }
    return ($textOk -and $enterOk)
}
```

- [ ] **Step 2: 语法自检（不发包）**

Run: `powershell -NoProfile -Command ". $env:USERPROFILE\mam-probe\ConIn.ps1; Get-Command Send-ToConsole | Select Name"`
Expected: 输出 `Name ---- Send-ToConsole`（Add-Type 编译无报错）。

- [ ] **Step 3: 台账记录（Task 1 完成）**

### Task 2: conhost 宿主 × claude · 基础送达（最简路径首发）

**Files:** Create: `proj-claude-conhost\`、`evidence\T2-*`
**Interfaces:** 消费 `Send-ToConsole`；产出「conhost×claude 基础结论」供 Task 7 矩阵引用。

- [ ] **Step 1: 建一次性项目目录并开 conhost 窗口跑 claude**

```powershell
New-Item -ItemType Directory -Force -Path "$env:USERPROFILE\mam-probe\proj-claude-conhost" | Out-Null
Start-Process conhost.exe -ArgumentList "cmd.exe" -WorkingDirectory "$env:USERPROFILE\mam-probe\proj-claude-conhost"
```

在弹出的**经典控制台**窗口里手动执行 `claude`（进入 TUI，等待输入状态）。

- [ ] **Step 2: 找目标 PID（记录备选与命中者）**

```powershell
Get-Process | Where-Object { $_.ProcessName -match 'claude|node|cmd' } |
    Select-Object Id, ProcessName, MainWindowTitle, StartTime | Format-Table | Out-File "$env:USERPROFILE\mam-probe\evidence\T2-pids.txt"
```

优先用 claude 主进程 PID；`AttachConsole` 失败时改试同窗口 cmd.exe 的 PID——**哪个命中记哪个**（这是 M9 实现的进程定位输入）。

- [ ] **Step 3: 发送基础探测消息（单行 + 回车）**

开**新的** PowerShell 窗口执行：

```powershell
. "$env:USERPROFILE\mam-probe\ConIn.ps1"
$stamp = Get-Date -Format yyyyMMddHHmmss
$msg = "[mobile PROBE-$stamp] base-test 收到请只回复：PROBE-OK"
Send-ToConsole -TargetPid <Step2 命中的 PID> -Text $msg -Enter
```

Expected（看 `conin.log`）：AttachConsole OK；WriteConsoleInput text ok=True written=完整计数；enter ok=True。

- [ ] **Step 4: 送达验证（方法 A 地面真值）——60 秒内查会话文件**

```powershell
$hit = Get-ChildItem "$env:USERPROFILE\.claude\projects" -Recurse -Filter *.jsonl |
    Sort-Object LastWriteTime -Descending |
    Select-Object -First 5
$hit | Select FullName, LastWriteTime | Out-File "$env:USERPROFILE\mam-probe\evidence\T2-session-files.txt"
Select-String -Path $hit[0].FullName -Pattern "PROBE-" -SimpleMatch |
    Select-Object -Last 5 | Out-File "$env:USERPROFILE\mam-probe\evidence\T2-delivery.txt" -Append
```

判定：`T2-delivery.txt` 出现 `PROBE-<stamp>`（user 消息）= **送达成功**；随后 agent 回复 `PROBE-OK` 再截 `evidence\T2-reply.png`（控制台窗口截图，Win+Shift+S 存文件）作辅助证据。

- [ ] **Step 5: 未送达时的扫描码兜底重试**——若 `conin.log` 全部 ok=True 但会话文件未命中（API 层成功、TUI 没收到）：把 `ConIn.ps1` 里 `MakeRecord` 的 `wVirtualScanCode` 由 0 改为真实扫描码（Enter=0x1C，字符键=0x2B，或用 `MapVirtualKey` 求值），重发一条**新 stamp** 消息再验证；结果无论成败都台账注明「扫描码兜底：生效/无效」。

- [ ] **Step 6: 台账记录（PID 命中情况 / API 返回 / 会话文件命中 / 回复观察 / 兜底结果）**

### Task 3: Windows Terminal 宿主 × claude · 基础送达（本探测最大不确定点）

**Files:** Create: `proj-claude-wt\`、`evidence\T3-*`

- [ ] **Step 1: WT 里开一次性会话**——在 Windows Terminal 新标签页 `cd $env:USERPROFILE\mam-probe\proj-claude-wt` 后运行 `claude`（先建目录：`New-Item -ItemType Directory -Force`）。
- [ ] **Step 2: 重复 Task 2 Step 2-5**（PID 盘点 → 发同样的 `[mobile PROBE-<stamp>] wt-test ...` → 会话文件验证 → 台账）。证据前缀改 `T3-`。

关键观察点（写进台账「观察」栏）：WT 宿主下 AttachConsole 是否成功、错误码、WriteConsoleInput 计数、**会话文件是否命中**——API 成功但 TUI 没收到（ConPTY 输入缓冲不被宿主转发）是本任务最可能的失败形态，要如实区分记录。

### Task 4: 行为矩阵补全（只在 Task 2/3 中**送达成功**的宿主上做；都失败则本任务记 N/A 跳过）

**Files:** `evidence\T4-*`

- [ ] **Step 1: 字面 `\n` 长串**——`$msg = "[mobile PROBE-$stamp] long\ntext 第一行\n第二行\n第三行 收到请只回复：PROBE-OK"`（双引号内 `\n` 即字面反斜杠+n，≥100 字符），发送后按方法 A 验证会话文件里消息**完整单行**到达（无截断、无真实换行）。
- [ ] **Step 2: 中文与 emoji**——`$msg = "[mobile PROBE-$stamp] 中文测试 你好世界 🌏🚀 收到请只回复：PROBE-OK"`，验证会话文件原文一致（UnicodeChar 通道）。
- [ ] **Step 3: 非聚焦写入**——先把目标窗口**最小化**，再发送一条新 PROBE 消息，验证仍送达（WriteConsoleInput 理论不依赖焦点——这是它优于 keystroke 的核心卖点，必须实证）。
- [ ] **Step 4: 连续两条**——背靠背两次 `Send-ToConsole`（不同 stamp），验证两条都到且顺序不乱。
- [ ] **Step 5: 双会话定向（不串台，GO 必要条件）**——在**两个独立窗口/标签**各开一个 claude 会话（不同临时项目目录），用各自 PID 分别发送**不同 stamp** 的消息，验证两个会话文件各自只收到自己的那条。MAM 是多会话看板，注入串台=致命缺陷，此步失败必须在报告置顶标注。
- [ ] **Step 6: 审批单键注入（M8 Windows 前置，登记不阻塞 GO）**——对 claude 发一条会触发权限请求的指令（如让它运行一条需批准的 shell 命令），等批准提示出现后，**只注入按键**（按提示形态选 `y` 或数字或 Enter，`Send-ToConsole -Text "y" -Enter`），验证会话继续执行。成败都登记——失败时 M8 在 Windows 侧走「映射缺失降级普通发消息」兜底。
- [ ] **Step 7: 锁屏注入（可选加分项）**——Win+L 锁屏后从另一 PowerShell 发送新 stamp，解锁后验证会话文件送达（WriteConsoleInput 理论不依赖用户会话解锁——与 keystroke 的本质区别，macOS 侧同口径实测在 M7）。
- [ ] **Step 8: busy 态观察（仅登记，不影响 GO/NO-GO）**——给会话发一句「请从 1 数到 20，每个数一行」，趁运行中（黄态）再注入一条 PROBE 消息，观察 TUI 是缓冲（turn 结束后处理）、忽略还是打断——这是 M7「插队」语义的 Windows 侧证据。
- [ ] **Step 9: 每步台账 + 证据；全部探测完成后在测试会话里输入 `/exit` 退出 claude，关闭窗口**

### Task 5: codex 与 kimi 复测（第二梯队；宿主 = Task 2/3 中成功的宿主，由简到繁）

**Files:** `proj-codex-*\`、`proj-kimi-*\`、`evidence\T5-*`

- [ ] **Step 1: codex**——在一次性目录开 `codex`（TUI），重复**四项基础行为**（单行+回车、字面 `\n` 长串、中文/emoji、非聚焦——与 GO 判定口径一致）。会话文件位置：`%USERPROFILE%\.codex\sessions\` 下最新 `rollout-*.jsonl`，`Select-String "PROBE-"` 验证。
- [ ] **Step 2: kimi**——同法开 `kimi`，重复四项基础行为。会话文件位置以本机实际为准（先 `Get-ChildItem $env:USERPROFILE -Directory -Filter "*kimi*"` 找线索并记录路径到台账——路径本身是 M9 的路由输入）。
- [ ] **Step 3: 任一家 AttachConsole/WriteConsoleInput 行为与 claude 不同（错误码/计数差异）如实台账**
- [ ] **Step 4: 退出测试会话，清理进程（`Get-Process` 确认无残留 claude/codex/kimi）**

### Task 6: SendInput 应急参照（仅登记证据，不是正式路径）

**Files:** `evidence\T6-*`

- [ ] **Step 1: 用 SendKeys 做一次对照**——把目标会话窗口**置前台**后，在新 PowerShell 里 `[System.Windows.Forms.SendKeys]::SendWait("[mobile PROBE-$stamp] sendkeys-ref{ENTER}")`（先 `Add-Type -AssemblyName System.Windows.Forms`），验证可达但记录三个 D7 限制的实际表现：必须前台聚焦、锁屏（Win+L）后再试一次必然失败、中文输入法开启时行为异常。
- [ ] **Step 2: 台账明确标注「应急参照，正式路径仍为 WriteConsoleInput」**

### Task 7: 结论判定与报告撰写

**Files:** Create: `%USERPROFILE%\mam-probe\windows-终端注入-探测报告.md`

- [ ] **Step 1: 按下列模板写报告（判定标准照抄执行）**

```markdown
# Windows 终端注入 · 探测报告（M6）

> 探测日期 / 执行机 / 报告人：
> 结论先行：**GO | PARTIAL（范围：…）| NO-GO**

## 1. 环境清单（版本门控登记）
| 项 | 版本 |
|---|---|
| OS（含 build） | |
| 默认终端应用 / Windows Terminal | |
| claude / codex / kimi | |
| PowerShell | |

## 2. 结论矩阵（核心交付）
| 宿主 | CLI | 单行+回车 | 字面\n长串 | 中文/emoji | 非聚焦 | 连续两条 | busy 态观察 | 证据 |
|---|---|---|---|---|---|---|---|---|
| conhost | claude | ✅/❌/N/A | | | | | | T2-*/T4-* |
| WT | claude | | | | | | | T3-*/T4-* |
| （成功宿主） | codex | | | | | | | T5-* |
| （成功宿主） | kimi | | | | | | | T5-* |

（矩阵只认「会话文件出现探测消息」为 ✅；API 成功但未送达记 ⚠️ 并注明错误码与扫描码兜底结果）

## 2.1 专项记录
| 专项 | 结果 | 证据 | 影响 |
|---|---|---|---|
| 双会话定向（不串台） | ✅/❌ | T4-* | ❌ 时报告置顶标注，GO 不可给出 |
| 审批单键注入（y/Enter） | ✅/❌ | T4-* | M8 Windows 前置；❌ 时 M8 走降级兜底 |
| 锁屏注入（若测） | ✅/❌/未测 | T4-* | 边界文案依据 |
| 连续两条顺序 | ✅/❌ | T4-* | 队列串行依据 |

## 3. 技术要点与坑
（AttachConsole 目标 PID 选择〔CLI 主进程 or 同窗口 cmd〕、FreeConsole 顺序、错误码表、WT 与 conhost 差异、锁屏/聚焦表现）

## 4. 判定与依据
- **GO** = WT 与 conhost 双宿主下，≥2 家 CLI（必须含 claude）基础行为（单行+回车、字面\n长串、中文、非聚焦）全 ✅，**且双会话定向不串台通过** → M9 全量实现
- **PARTIAL** = 仅单宿主通过，或仅 claude 通过，或定向偶发串台 → M9 缩小宿主/工具范围（写明保留范围）
- **NO-GO** = claude 在两类宿主均不可送达 → 降级登记已知限制，SendInput 仅应急（D7）

## 5. M9 实施建议
（建议 API 层〔同 ConIn 的 P/Invoke 移植为 Rust windows-rs〕、进程→控制台定位方式、版本门控点〔WT 版本/CLI 版本升级复查项〕、遗留问题）
```

- [ ] **Step 2: 自检三件**——①矩阵每个格子都有证据文件可指；②报告结论与台账逐条一致；③版本清单与 T0-env.txt 一致。
- [ ] **Step 3: 台账记 Task 7 完成；打包回传清单 = 报告 + probe-log.md + conin.log + evidence\ 全部 + ConIn.ps1**

---

## 回传与后续（给用户）

探测完成后，把 `%USERPROFILE%\mam-probe\` 整个目录（或至少「报告 + 台账 + 日志 + 证据」）发回 Mac 侧评审：报告将入库 `docs/research/refs/phase2-消息注入/windows-终端注入-探测报告.md`，评审结论（GO/PARTIAL/NO-GO 复核）决定 M9（Windows 注入实现）的范围；PARTIAL/NO-GO 均不阻塞 M7/M8。
