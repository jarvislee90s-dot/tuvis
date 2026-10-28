# M7–M8 · macOS 实机验收清单（Windows 执行机回传）

> 执行环境说明：M7 三通道注入与 M8 按键生效的实机验证只能在 macOS 完成（tmux/iTerm2/Terminal.app + AppleScript）。
> Windows 侧已完成：全部代码 + 单测 + 六门禁（见台账 progress.md 门禁数字）。本清单供 Mac 侧逐条实测，
> **实测结论不回写设计文档**，问题按批次设计降级路径处理并回报用户裁决。
> 基线：分支 `feat/phase2-injection`（Windows 侧本地提交链，未 push——合并评审后自然并入）。

## M7 · 终端注入全链（8 项）

| # | 场景 | 前置 | 步骤 | 预期观察 |
|---|---|---|---|---|
| 1a | tmux 会话注入 | Mac 上 tmux 会话内跑真实 `claude`（等待输入态） | 手机（或 Mac 本机浏览器过 PIN）进详情页发「你好」 | 终端出现 `[mobile <设备名>] 你好` 输入行 + 回车；agent 响应；回执「已送达终端」 |
| 1b | iTerm2 会话注入 | iTerm2 tab 内跑 `claude` | 同上 | 同上（两步 write：文本 newline NO + 补回车） |
| 1c | Terminal.app 会话注入 | Terminal.app 窗口跑 `claude` | 同上 | 同上（`do script`） |
| 2 | 黄排队 → 可输入态 flush | 黄态（运行中）会话 | 运行中发一条消息 → 得「排队中 第1位」；等任务完成（转绿/红） | ≤1s 内自动送达（flush 循环订阅跃迁事件），chip 不变（排队→终端出现） |
| 3 | 立即发送插队 | 运行中会话 + 排队项 | 点「立即发送」 | 观察各 TUI 对 busy 态输入的缓冲/忽略/打断行为并记录（裁决 12 语义；行为不明家后续禁用插队） |
| 4 | 撤回 | 排队中条目 | 点「撤回」 | 条目消失；终端不出现该内容 |
| 5 | 锁屏注入 | 锁定 Mac | 锁屏前发消息（黄态排队或可输入态直发） | 解锁后确认终端收到（应用级脚本理论不依赖解锁；不可用则登记边界文案） |
| 6 | 审计页 | 上述操作后 | 桌面设置 → 注入审计 | 每条写动作逐条可查（设备/会话/动作/摘要），只增不改 |
| 7 | 不可注入卡禁用 | WorkBuddy 会话（或 zcode）详情页 | 打开输入区 | 输入禁用 + 显示原因（黑盒/无头通道后续版本） |
| 8 | 一期零回归抽查 | — | 看板/会话内容/文件预览/SSE/配对抽查 | 全部照旧（自动化回归面：cargo 862+/5 环境败 + vitest 538 全绿已过） |

## M8 · 审批应答（3 项，Task 13 收口后追加实测）

| # | 场景 | 前置 | 步骤 | 预期观察 |
|---|---|---|---|---|
| M8-1 | 红卡一键批准/拒绝 | claude 会话触发权限请求（如让它跑一条需批准命令） | 红卡出现 → 点「允许」/「拒绝」 | 终端收到对应按键（1/esc 等——以 Task 13 取证键位为准），agent 继续或中止；审计 action=approve/reject。**Mac 实测（2026-09-19，55f37e7）：部分——经降级达效**。红卡在 macOS 暂不可达（状态判定+提示文本均依赖 Windows 屏读路径，见 Mac 报告 §四-A）；降级链（普通发送键位字符→排队→插队投递）实测等效可用：键位「1」投递后提示被应答、命令执行，409 not_waiting 守卫正确拦截不发错误键位 |
| M8-2 | 计划模式「批准执行」 | claude 计划模式会话出计划 | 红卡点批准执行 | 计划被批准开始执行（裁决 13 会话内应答范畴）。**Mac 实测：PASS（经降级）**——plan mode 出计划→"Would you like to proceed?"→降级投递「1」→批准执行（auto mode on、开始写码） |
| M8-3 | 降级路径 | 把映射 KV 清掉（或用无映射工具） | 红卡不出现 → 普通发消息 | 不发错误键位；降级文案出现 |
| M8-4 | codex 键位取证补测（**已于 M6R–M9R 批 T10 Windows 侧取证：codex y/esc，0.154.0 回填 DEFAULT_MAPPINGS_JSON**——本项仅保留 Mac 版本漂移复核，见 D-4） | codex `/approvals` 设为需审批模式（如 Ask always） | 触发原生审批框，抄录提示原文与键位 | 与 T10 取证口径核对一致；Mac 版本若有漂移，据实修订 DEFAULT_MAPPINGS_JSON 的 codex markers/keys/verified_with |
| M8-5 | codex hooks 触发复验（**F3 PascalCase 修复后**，M1A 前置，2026-09-20 批新增） | codex 0.155.x 在场 | 跑 `cargo test --lib monitor::hooks::codex_pascal -- --ignored`（注册写入真实 ~/.codex/hooks.json）→ 跑一次真实 codex 交互会话 | hooks.json 出现 PascalCase 六键且旧 camelCase 键被迁移清除；会话期间 `~/.mam/events/<session_id>.json` 出现（hook 真触发）。**M1A 调研锚点**：`docs/research/refs/phase2-消息注入/2026-09-19-审批事件钩子通道调研.md`（claude Notification permission_prompt 判定①②一步到位；codex 0.155.1 PascalCase hooks 源码证据 §3.2/§3.3；红卡=钩子信号+固定键位，marker 文本检测在 macOS 红卡路径可放弃） |

## 已知执行形态差异（实测时留意）

- **iTerm2/Terminal.app 脚本带运行态守卫**：App 未运行时返回 not found 哨兵降级下一通道，不会冷启动终端。
- **write 脚本无 activate（不抢焦点）；按键脚本有 activate（System Events keystroke 需前台）**——审批应答会抢一次焦点，属预期。
- **tmux 半成功（文本入 pane 而回车失败）立即返回失败且文案注明「勿重试」**——实测验证该文案出现时的终端实际状态。
- Terminal.app 通道的单键形态是「activate + keystroke」变体（无 do-script 单键等价）——M8-1/M8-2 时留意其真实效果。

## Windows 侧已实测（本机，非本清单范围）

详见 `%USERPROFILE%\mam-probe\windows-终端注入-探测报告.md`（M6）+ 台账 progress.md 的 M9 实机记录（Task 16 后补）。

---

## M6R–M9R 批次 · Mac 回传清单追加（2026-09-19）

> 硬化批次（引擎重写/确认层/队列生命周期/严格档/resume/macOS 匹配修复）后新增的 macOS 实机验证项。
> 项 D-1～D-9 与 `docs/release-notes/m6r-m9r-acceptance-checklist.md` D 段同源，彼处含完整前置与判定说明；
> **判定与实测结论以 m6r-m9r-acceptance-checklist.md D 段为 SSOT，本节为执行镜像勿单边修订**。

| # | 场景 | 前置 | 步骤 | 预期观察 |
|---|---|---|---|---|
| D-1 | 三通道长消息 + 确认机制 | tmux/iTerm2/Terminal 各跑真实 CLI | 直发长消息；再测插队排空+草稿 | 三通道均送达；直发以会话文件命中回执；插队 busy 入草稿、转闲命中；macOS 无屏读时确认层降级语义（Failed 文案）可用性确认 |
| D-2 | 相等匹配：ttys 前缀相近双会话互不串扰 | iTerm2 开 tty 前缀相近的双会话 | 对其一发消息 | 仅目标会话收到（iTerm2 脚本 contains→is 全路径匹配的实机复核） |
| D-3 | Terminal.app 后台标签页注入 | 目标会话在非前台标签 | 对后台标签会话发消息 | `do script in t` 后台执行语义成立，不切换前台标签 |
| D-4 | 审批 codex/claude 复验 | codex Read Only 档审批 / claude 权限请求+计划模式 | 红卡批准/拒绝各一次 | 键位与 Windows 取证口径一致（codex y/esc、claude 1/esc）；codex Mac 版本键位漂移复核 |
| D-5 | resume：iTerm2/Terminal 双通道 | 有 cwd 的会话（≥2 家工具） | 点「在电脑上打开」 | 开窗 + `cd <cwd> && <resume>` + 置前聚焦均成立 |
| D-6 | **activate→keystroke 时序竞争** | Terminal.app 审批红卡 | 点批准 | 若偶发丢键/落错窗：activate 后加 `delay 0.2` 复验（降级预案备案） |
| D-7 | **Terminal raise 语义裁决** | Terminal.app 多窗口 | 观察按键/审批应答的升窗行为 | 「升窗不升标签 vs 纯后台」二选一，实机观察后回报用户定案 |
| D-8 | **iTerm2/Terminal `tty of s/t` 全路径口径确认** | iTerm2 与 Terminal.app 各一 | 打印 `tty of …` 取值 | 确认返回 /dev/ttysNNN 全路径（相等匹配前提；裸后缀则 normalize_dev_tty 兜底） |
| D-9 | **normalize_dev_tty 实机验证** | macOS 终端 `ps -o tty` | 观察输出口径 | ps 返裸后缀时归一 `/dev/ttysNNN` 后匹配成功（构造已单测，实机口径待验证） |
