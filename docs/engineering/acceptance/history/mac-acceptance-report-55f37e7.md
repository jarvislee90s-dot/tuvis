# macOS 实机验收报告（基准 55f37e7 · 2026-09-19 · jarvisdeMac-mini）

<!-- 历史标注（2026-10-14 · #131）：本文档成文于「命名隧道 → 自有域名」改名前；文中旧卡名「命名隧道」即现行「自有域名」卡，属历史记录，按裁决不改原文。 -->

> 执行人：MAM macOS 实机验收 agent（ZCode）。基准：独立 worktree `/tmp/mam-mac-accept/repo`，`git rev-parse --short HEAD` = **55f37e7**（已校验）。
> 测试会话全部位于 `/tmp/mam-mac-accept/`（proj-tmux / proj-iterm2 / proj-terminal / proj-codex / proj-kimi / proj-opencode），未触碰用户本人会话。
> 证据（截图/日志/终端留痕）统一存 `/tmp/mam-mac-accept/evidence/`，报告内以 `evidence/…` 相对路径引用。
> 全程未 commit / 未 push / 未动分支；发现 FAIL 未修任何代码（含 macOS cfg 区）。

## 一、结论先行

两份清单（m7-m8 八项 + M8 四项、m6r-m9r D-1～D-9 与 C 段 Mac 相关项）逐项实测完毕：**M7 八项全 PASS；M8 四项 = 3 PASS + 1 部分（M8-1）；D 九项 = 6 PASS + 1 FAIL（D-5）+ 1 部分（D-7）+ 1 不适用（D-6）；C 段 Mac 相关项全部 PASS 或与镜像一致（C-10/C-13 记部分，C-12/C-15 不适用）**。合计 **FAIL 1 项（D-5 resume 实机链路）、部分/降级记录 4 项、不适用 3 项，其余全部 PASS**。核心结论：**M7 三通道注入全链、队列生命周期（排队/插队/撤回/冻结/重启补投）、macOS 相等匹配与 tty 口径、确认层降级语义、锁屏注入全部实机 PASS**；**审批红卡在 macOS 整体不可达**（等待态判定与提示文本获取两处均依赖 Windows 屏读路径），但键位（claude 1、codex y）经降级投递实测全部有效；**resume 是唯一实机 FAIL**，分层归因清晰（MAM dev 进程缺 TCC 静默吞错 + iTerm2 3.7.2 建窗异常）。版本漂移已逐项记录（claude 2.1.278 / codex 0.155.1 / kimi 2.0.1）。

## 二、环境基线

| 项 | 本机（macOS 实测） | Windows 取证口径 | 漂移 |
|---|---|---|---|
| OS | macOS 26.6.2 (25G83) arm64，Mac-mini | Windows 实机 | — |
| Terminal.app | 2.15 | — | — |
| iTerm2 | **3.7.2**（验收中经 brew 新装） | — | 新装 |
| tmux | **3.7c**（验收中经 brew 新装） | — | 新装 |
| claude | **2.1.278** | 2.1.251 | **漂移（minor）** |
| codex | **0.155.1**（npm 包口径 0.154.0；TUI 自更新后 0.155.1） | 0.154.0 | **漂移（minor）** |
| kimi | **2.0.1** | 2.0.0 | **漂移（patch）** |
| opencode | 1.18.31 | 1.18.31 | 一致 |
| node/pnpm/rustc | v26.8.1 / 11.23.0 / 1.97.1 | — | — |

环境备注：tmux 与 iTerm2 原未安装，为执行清单经 brew 安装（可逆、仅测试前置）；报告目录 `.superpowers/sdd/2026-09-18-phase2-injection-mainline/` 由验收创建。

## 三、逐项结果表

判定 SSOT = `m6r-m9r-acceptance-checklist.md` D 段/C 段；M7/M8 以 `m7-m8-macos-live-checklist.md` 为镜像。

### M7 · 终端注入全链

| # | 场景 | 结果 | 证据 | 备注 |
|---|---|---|---|---|
| 1a | tmux 会话注入 | **PASS** | evidence/m7-1a-tmux-inject.txt（31/39 行） | `[mobile 新设备] 你好` 落盘+agent 回复+「已送达终端」回执；stop hook 正常 |
| 1b | iTerm2 会话注入 | **PASS** | evidence/m7-1b-iterm2-inject.png | 两步 write（newline NO+补回车）生效；**环境注记**：iTerm2 脚本引擎卡死需重启一次（见六-4） |
| 1c | Terminal.app 注入 | **PASS** | evidence/m7-1c-terminal-inject.txt（16 行）/ .png | `do script` 落盘+回执 |
| 2 | 黄排队→可输入 flush | **PASS** | 审计 18:47:52 queue→18:49:14 flush；pane 67 行 | running 态发送→`queued itemId=11 position=1`；转绿自动送达+回复 |
| 3 | 立即发送插队 | **PASS** | 审计 18:55:26 queue→:28 jump ok；pane 48 行 | macOS 语义=**busy 态直写 tty 即时送达**（回执 delivered），非 Windows 的排空/防重形态 |
| 4 | 撤回 | **PASS** | 审计 18:54:21 queue→:52 retract ok | 队列行删除、终端零到达、chip 收敛 |
| 5 | 锁屏注入 | **PASS** | evidence/m7-5-lockscreen-inject.txt（46/50 行） | 锁屏（⌘⌃Q）期间 pair+send 均成功：delivered（stamp 命中）+tmux pane 落盘+agent 锁屏期间回复。tmux 通道=应用级；iTerm/Terminal 通道锁屏态未单独复测 |
| 6 | 审计页 | **PASS** | evidence/m7-6-audit-small.png | 时间/设备/会话/动作/摘要五列齐全，全测试动作可追、只增不改 |
| 7 | 不可注入卡禁用 | **PASS** | API 实测 | ZCode 会话 `injectable:false / reasonCode:headless_only / "ZCode 走无头通道（M11）"` |
| 8 | 一期零回归抽查 | **PASS** | 看板/详情/文件面板截图 | 看板 10 会话、卡片实时更新（SSE/轮询）、正文渲染、文件面板（全部/文档/图片+范围切换）、桌面完成通知正常 |

### M8 · 审批应答

| # | 场景 | 结果 | 证据 | 备注 |
|---|---|---|---|---|
| M8-1 | 红卡一键批准/拒绝 | **部分（降级达效）** | iTerm claude 权限提示实测 | 红卡不可达（见四-A）；`approve-options` available=false；409 not_waiting 守卫按设计工作；**键位 1 经降级投递实测有效**（提示被应答、命令执行） |
| M8-2 | 计划模式「批准执行」 | **PASS（经降级）** | evidence/m8-2-plan-approve.txt | plan mode 出计划→"Would you like to proceed?"→降级投递「1」→**批准执行**（auto mode on、开始写码） |
| M8-3 | 降级路径 | **PASS** | codex/kimi 直发实测 | 红卡不出现→普通发送可用、不发错误键位；降级文案「已注入未确认（未见会话记录），请检查终端后重试」如实可用、重试不双投 |
| M8-4 | codex 键位漂移复核 | **PASS（有漂移记录）** | 审批框原文抄录（见六） | 键位 **y/esc 不变**、marker "would you like to run the following" 不变；0.155.1 新增选项 (p) 与 /permissions 更名；y 经降级投递实测生效（Desktop 文件落盘） |

### D 段（m6r-m9r）

| # | 项 | 结果 | 证据 | 备注 |
|---|---|---|---|---|
| D-1 | 三通道长消息+确认 | **PASS** | 审计 57-64 行 | tmux/iTerm 800 字 delivered（0.8s stamp 命中）；Terminal 形态 busy 缓冲→Fail 降级文案如实；2000 字 claude（queued→flush ok）+ opencode（3.6s delivered）；无屏读降级语义确认可用、重试不双投 |
| D-2 | 相等匹配互不串扰 | **PASS** | 审计 + iTerm/Terminal 内容比对 | 实测最近编号对 **ttys002(iTerm claude) vs ttys003(Terminal claude)**：消息只达 ttys002，ttys003 零扰动；构造性撞号（ttys100/ttys1000）实机无法凑出（当前 tty 空间 ttys000-007），已由单测 `tmux_pane_match_is_full_path` 覆盖 |
| D-3 | Terminal 后台标签注入 | **PASS（等价形态）** | evidence/d3-background-inject.txt | `do script in t` 按 tty 命中**后台窗口内 tab**，前台窗口身份不变、无焦点抢占；同窗多 tab 形态因 macOS 26 Terminal ⌘T 自动化下表现为新窗未复现，留人工 |
| D-4 | 审批 codex/claude 复验 | **PASS（有漂移）** | 同 M8-4/M8-1 | codex y/esc、claude 1/esc 均实测有效；漂移见六 |
| D-5 | resume 双通道 | **FAIL** | evidence/d5-resume-terminal.png、d5-resume-manual-payload.png | 详见四-B（分层归因） |
| D-6 | activate→keystroke 竞争 | **不适用（不可达）** | — | 红卡不可达（四-A 同根因），approve 的 keystroke 路径无实机样本；未观察到丢键/落错窗（无触发机会） |
| D-7 | Terminal raise 语义裁决 | **部分（观察受限）** | 见五 | 写路径从不 raise（多次后台注入实证）；按键路径不可达；resume 应 raise 但受 D-5 失败未观察——**二选一裁决素材见五，未定案** |
| D-8 | `tty of s/t` 全路径 | **PASS** | AppleScript 实机打印 | iTerm2 返 `/dev/ttys002`、`/dev/ttys006`；Terminal 返 `/dev/ttys003/004/005`——全路径口径成立 |
| D-9 | normalize_dev_tty 实机 | **PASS** | `ps -o tty` 输出 | ps 返裸后缀（ttys001-003）；归一后与 AppleScript 全路径匹配成功（注入全链即实证） |

### C 段 Mac 相关项

| # | 场景 | 结果 | 备注 |
|---|---|---|---|
| C-1 | 四家各发短消息 | **PASS** | claude×3 / codex / kimi / opencode 各一条送达；**codex/kimi 注入成功但自动回车被 TUI 吞**（见四-C），补回车后完成、确认层降级文案如实 |
| C-2 | 长消息 2000 字 | **PASS** | claude（快）即时；opencode 3.6s delivered，慢消费者预算内 |
| C-3/4/5 | = M7-2/3/4 | **PASS** | 见上 |
| C-6 | 关远程→冻结→重开续跑 | **PASS** | 停服期间 1 分钟+不投递（会话已转绿）；重开 **8 秒内**对账补投+终端确认 |
| C-7 | MAM 重启→自动补投 | **PASS** | pkill 杀进程→pnpm tauri:dev 重启→队列项跨重启存活→启动对账补投（audit 19:52:46 queue→19:54:47 flush）+终端回复 |
| C-8/9 | 审批 codex/claude 计划 | **PASS（经降级）** | = M8-4 / M8-2 |
| C-10 | resume 双端 | **部分** | 移动端触发已测（=D-5 FAIL，audit 三条 action=open 已落账）；桌面端 Tauri 路径未测（其不写审计，清单已注明） |
| C-11 | 审计页逐条可查 | **PASS** | = M7-6 |
| C-12 | WT/conhost 判冻 | 不适用 | Windows 专属 |
| C-13 | 滞留→屏读→补按恢复 | **部分（等价实证）** | macOS 无屏读，自动补按不存在；实测「人工按回车即提交、确认回执与落盘一致、不双投」与该条意图一致 |
| C-14 | 锁屏可达性 | **PASS** | = M7-5（macOS 侧） |
| C-15 | 蜂窝复验 | 不适用 | 需真机手机（用户项）；注：本机临时隧道（trycloudflare）已由用户开启，可作为蜂窝替代入口 |

## 四、FAIL/部分项详情

### A.（Important）macOS 审批红卡整体不可达（M8-1/D-4/D-6 根因）

- **复现**：claude 出现权限提示（"Do you want to proceed? 1.Yes/2.Yes-auto/3.No"）或 codex 审批框（"Would you like to run the following command? … (y)/(p)/(esc)"）时，GET `/m/api/v1/session-approve-options` 恒 `available:false, options:[]`。
- **初步归因（两处叠加）**：① `available = status==Waiting && 映射 && detect(last_message)`——两 CLI 在审批等待期会话状态均判 **processing** 而非 Waiting；② `detect` 吃 `last_message`，而审批提示是 TUI 层覆盖物**不落会话文件**。两边独立成立，任一即可卡死红卡。
  > **【2026-09-20 更正】** 本条原文称「Windows 靠 CONOUT$ 屏读拿到（提示文本）」——经 Windows 侧代码复核（2026-09-20 C-8 调查），**审批判定链路中不存在任何屏读**（屏读仅用于注入后确认层），Windows 红卡与 macOS 同样被上述两判据卡死。红卡跨平台统一解 = M1A（issue #74：钩子等待信号 + 固定键位）。
- **降级链实测（全部达效）**：普通发送键位字符（1/y）→ 排队 → jump 投递 → 提示被正确应答（claude 命令执行、codex 文件落盘 `~/Desktop/mam-accept-codex-test.txt`=hello）；`POST /session-approve` 被 409 not_waiting 守卫正确拦截（不发错误键位）。
- **截图**：审批框原文在 evidence/m7-1c-codex-terminal.png 会话流与对话记录中。

### B.（Important·唯一 FAIL）D-5 resume 实机链路未达成

- **复现**：移动端「在电脑上打开」→ HTTP 200 `{"status":"opening"}` + 审计 `action=open ok`；但 **Terminal 通道无任何窗口出现**（1 秒粒度连拍 8 窗不变）；**iTerm2 通道**出现延迟物化窗口且 workspace 显示 `/Users/jarvis`（cd 未生效嫌疑）。
- **判别实验（关键）**：用 **完全相同 payload** 手工执行 `osascript: do script "cd '<cwd>' && codex resume <id>"` → 窗口正常开启、cd 正确（proj-codex）、codex resume TUI 正常运行（evidence/d5-resume-manual-payload.png）。⇒ payload 与 codex 0.155.1 的 `codex resume <id>` 语法**均正确**。
- **归因猜测（分层）**：① MAM dev 二进制（无 bundle id）对 Terminal 的 Apple Events **TCC 授权缺失**，spawn 的 osascript 以 -1743 失败但 resume.rs 是 fire-and-forget，**stderr 被吞、audit 照记 open ok**——SSOT 与现实背离（主线应修：spawn 回执捕获 stderr）；② iTerm2 3.7.2 在 macOS 26 上 `create window with command` 存在**延迟物化/闪退/偶发 cd 失效**现象（本轮多次复现），属环境兼容性脆弱点。
- 附带：正装（非 dev）应用签名后 TCC 授权体验正常与否未验。

### C.（Medium）codex/kimi TUI 吞掉注入的自动回车

- **现象**：向 codex（Terminal 通道 do script 自带回车）与 kimi（iTerm2 两步写：newline NO+空文本补回车）注入后，文本**滞留 composer 未提交**（codex 连续 3 条堆叠、kimi 单条滞留），确认层在 3s 窗内找不到落盘记录→回执「已注入未确认」。**人工补一次回车即提交，无双投**。
- **归因猜测**：两 TUI（crossterm 系）疑似启用 bracketed-paste，尾随 `\n` 被视为 paste 内容不触发提交；claude TUI 不受影响（同法全通过）。
- **建议**：macOS 引擎按 TUI 家族区分——codex/kimi 在文本注入后补发独立 keystroke Enter（类似 Windows 按键通道的形态），或文档降级口径固定「注入后请到终端按一次回车」。

## 五、D-7 raise 语义观察（供裁决，未定案）

| 形态 | 实机事实 |
|---|---|
| 写路径（文本注入，`do script in t` / tmux send-keys / iTerm write） | 脚本**无 activate**——多次向后台窗口/后台 tab 注入，前台窗口身份与聚焦从未被改变（D-3 实证）；即「**纯后台**」是写路径的既成形态 |
| 按键路径（审批应答 `terminal_send_key_script`） | 脚本含「先选 tab → set index 置前 → activate → keystroke」，即「**升窗且连带切到目标标签**」的编码意图；但 macOS 红卡不可达（四-A），**无实机触发样本**，无法观察真实升窗效果与丢键率 |
| resume 开窗 | 脚本设计为开窗+聚焦（应 raise）；本轮因 D-5 失败未观察到有效样本 |

**倾向建议（仅供参考）**：既然写路径已是纯后台且体验良好，按键路径若也做成纯后台可避免「正在打字被抢焦点」，但 keystroke 需要前台（System Events keystroke 依赖）——纯后台与 keystroke 在 Terminal.app 上天然冲突，除非改用 `do script`（对单键不适用）或 iTerm2 的 `send text`。**如实陈述：两项形态在 macOS 上各有取舍（升窗切标签=确定性高但抢焦点；纯后台=不打扰但 keystroke 需前台前提），建议主线先解决红卡不可达（四-A），红卡可达后补一次 D-6/D-7 实测再裁决。**

## 六、版本漂移记录

1. **claude 2.1.251 → 2.1.278**：权限提示新增选项（"1. Yes / 2. Yes, and switch to auto mode / 3. No"）；计划批准提示变为 "1. Yes, and use auto mode / 2. Yes, manually approve edits / 3. Tell Claude what to change"；键位 **1/esc 语义未变**；提示文案 "Would you like to proceed?" 与 marker `would you like to proceed` 仍匹配。
2. **codex 0.154.0 → 0.155.1**：审批框 marker "would you like to run the following" **保留**；选项扩为 `1.Yes (y) / 2.Yes-and-don't-ask-again (p) / 3.No (esc)`（新增 p）；**`/approvals` 更名 `/permissions`**；shift+tab 循环出 Plan mode；首次运行触发 npm 自更新（uninstall→restart）。键位 **y/esc 实测有效**。DEFAULT_MAPPINGS_JSON 的 codex verified_with=0.154.0 → 0.155.1 触发 minor 漂移告警（红卡若可达会显示降级提示，符合 drift 规则）。
3. **kimi 2.0.0 → 2.0.1**：微漂移；composer 回车行为与 codex 同类（见四-C）。
4. **opencode 1.18.31**：与 Windows 取证一致，无漂移。
5. 终端本体：iTerm2 3.7.2 / tmux 3.7c 为本机新装（Windows 侧无对应物）；Terminal.app 2.15。

## 七、遗留与建议（给主线的下一动作）

1. **修 macOS 审批等待态判定**（四-A，Important）：建议走 hook 事件（claude PreToolUse/Notification 已有 status-hook 管道可扩）或会话文件尾部启发式，替代对屏读的依赖；否则红卡在 macOS 永久不可达，M8 价值只剩降级。
2. **resume spawn 结果回执化**（四-B，Important）：捕获 osascript stderr（-1743 TCC 等）并以 failed 回执，杜绝 audit「open ok 但无窗」的 SSOT 背离；dev 场景在 README 提示手动 TCC 授权。
3. **codex/kimi 自动回车补发**（四-C，Medium）：按 TUI 家族在注入后补独立 keystroke Enter，或固化「补回车」降级口径。
4. **iTerm2 3.7.2 + macOS 26 兼容性**（Medium）：create window+command 延迟物化/闪退/偶发 cd 失效；AppleScript 引擎在冷启动 TCC 打断后会卡死（-1712）直到应用重启——建议把「iTerm2 脚本健康探测」纳入注入前置（not found 哨兵已有，卡死形态可加超时计数）。
5. **失败注入行队列残留**（Low）：`inject_queue` 中 fail 行 failed_reason 事后被清空且未标 sent、仍留 pending，与 A10「失败行已退出 pending」口径不符，建议主线复核是否为周期兜底重投语义。
6. **设备计数展示口径**（Low）：远程关闭期间 UI 显示 0/10 而 DB remote_devices 有 9 行（含 7 条历史 E2E 设备）、重开后恢复——疑内存注册表 vs DB 双口径，建议统一或加说明。
7. **未覆盖面如实留白**：tmux「半成功（文本入 pane 回车失败）」文案未触发过；锁屏态 iTerm/Terminal 通道注入未单独复测；D-3 同窗多 tab 形态、D-6/D-7 按键路径待红卡可达后补测；C-12/C-15 为 Windows/真机项。
8. **测试痕迹**：本机 codex 会 `codex --resume` 测试窗、`~/Desktop/mam-accept-codex-test.txt`、`~/.mam` 内测试审计行与队列行、设备表 2 条「新设备」为本轮产物，可按需清理；iTerm2/tmux 为新装软件，可 `brew uninstall --cask iterm2 && brew uninstall tmux` 移除。
9. **外网链路**：命名隧道 `mac-mam.bondtoolbox.asia` 仍缺 DNS CNAME（指向隧道 ID `14ad5920-0fc6-42e5-8daf-f19a59c28b93.cfargotunnel.com`，或 Zero Trust→Public Hostname 添加，Service=`http://localhost:9420`）；用户已开的临时隧道地址可用于蜂窝复验（C-15）。
