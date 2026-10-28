# 二期收尾 · 手工验收问题修复批（批次甲+乙）· 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development 或 executing-plans。步骤用 checkbox 跟踪。**批次甲完成并自验后必须停下等主线评审，评审通过才开批次乙。**
> **Goal:** 修复用户 C 段手工验收发现的全部入计划问题：审批信号链（红卡可达，三家工具）、codex 状态/钩子/文件提取、计划消息可见性、队列交互（修改重发/确认语义/多列队）、plan 文件预览豁免。
> **执行环境：** Windows 实机（分支 feat/phase2-injection）。
> **依据档案（执行前读）：** GitHub issue #74 及其 2026-09-20 评论（审批信号调研全量证据：claude 双信号/codex PermissionRequest 全语义/信任门/kimi hooks）；docs/research/refs/phase2-消息注入/2026-09-19-审批事件钩子通道调研.md；本计划 §1 根因摘要（源码级证据已锚定，勿重新调研）。
> **红线：** 不 push、不动 main、六门禁全绿（cargo fmt/clippy/test + pnpm check/test/build:mobile；preset_v2 5 败=360 环境基线）、实机测试一律 #[ignore]、设计级口径变化只上台账。

## §1 问题清单与根因摘要（13 项对照）

| # | 问题 | 根因（已源码级锚定） | 处置 |
|---|---|---|---|
| 1 | codex 提前绿灯 | 钩子死→纯文件推导→审批等待=rollout 冻结≥300s→停更降级 Waiting→被"不落红"规则强转 Idle | 甲 T2+T3 |
| 2 | codex Hook failed exit 1 | 钩子命令裸 `bash` 在 codex 原生 shell（cmd 包装+清环境）不可解析；脚本从未运行（claude 侥幸因 Git Bash 环境启动） | 甲 T1 |
| 3 | codex md 不进文件预览 | `custom_tool_call`（apply_patch）在 map_codex_lines 落入 `_ => {}` 整条丢弃；图片靠 view_image 结构化引用侥幸命中 | 甲 T5 |
| 4 | 修改后再发送变自动插队 | 修改=完全出队+本地还原；重发走无标志直发，落"快照可输入（文件说闲、TUI 实忙）"窗口即直发 | 乙 T2 |
| 5 | "已注入未确认"假失败+重试双发风险 | 直发确认 5s 窗内 agent 未落盘（消息在 TUI 内部队列）+ 屏读无滞留草稿→误判 Failed；TUI 那份无法撤回 | 乙 T3 |
| 6 | 第二条顶掉第一条/期望多列队 | 后端本就多列队；前端单回执槽覆盖 + 自动 flush 无声消费 | 乙 T4 |
| 7 | C-8 codex 审批远程无感知 | 双判据死（状态永非 Waiting；提示文本不落文件）+ 信任门（钩子未受信不运行） | 甲 T1-T4 |
| 8 | C-9 计划消息远程不可见 | 计划在 ExitPlanMode 工具调用参数里，被折叠成"▸ 调用 ExitPlanMode"+总结模式折叠非末条 | 乙 T1 |
| 9 | 重复打印计划 | claude harness 闰锁重试（实测 7 次 ExitPlanMode 中 4 次同计划）+同 id 多段记录 | **用户裁决不修（2026-09-20）** |
| 10 | plan 文件"受安全策略保护" | SENSITIVE_DIRS 黑名单整目录拦 .claude，plans/ 连带 | 甲 T0+T6 |
| 11 | 红灯判断不准（屎山体感） | 8 工具 5 种红灯语义、9 个防抖常数、尾部形态对审批不可分辨（git 史 5 轮治理 2 次反转） | 甲 T3（两个污染层退役）；**全面重构另立批次** |
| 12 | C-5 撤回 / C-11 审计 / C-15 蜂窝 | 测试通过 | 无动作 |

**实现红线（调研实证，违反即返工）**：
1. helper 监听模式统一 **exit 0 + 空 stdout**——codex 的 exit 2+stderr=Deny 劫持审批、JSON stdout=劫持审批框（与 claude 语义相反，claude 的 exit 2 不生效）；
2. codex 命令钩子默认 600s 超时，Interrupt/SessionEnd 仅 1s（事件文件写入必须瞬时完成）；
3. `async:true` 钩子被 codex 跳过，注册不得带 async；
4. claude PermissionRequest payload 无 message 文本——文案从 Notification.message 或 tool_name+tool_input 拼装；codex PermissionRequest 的 `tool_input.description` 是人类可读审批理由。

## §2 批次甲 · 后端信号链（T0–T7，顺序执行）

### T0 · 八工具产物目录盘点 + opencode server 可达性探测（半天，只读产出）
- [ ] 盘点 ~/.claude ~/.codex ~/.kimi-code ~/.zcode ~/.dsh 等（含 opencode 配置目录）各工具"agent 写给用户看的产物目录"（计划/交接/报告类）：路径、内容形态、敏感性评估——只收"纯 markdown 产物"类；凭据/会话原始数据不收。产出豁免目录表（工具×路径×理由），落 `docs/research/refs/phase2-消息注入/2026-09-20-工具产物目录盘点.md`。
- [ ] opencode 本地 server 探测：用户所开 opencode 进程是否监听本地端口（netstat+进程关联；查其 server 配置项）；可达则只读验证一次 SSE permission.asked 订阅——结论写进盘点文档（二期接入可行性判定，不实现）。

### T1 · 原生 helper 替换 bash 脚本（D1）
- [ ] 新建 MAM 自带原生 helper（Rust bin，复用 mam-marker 分发管道先例 `hooks.rs::install_marker_helper`）：读 stdin JSON（snake_case，claude/codex/kimi 三家同形态）→ 解析 session_id/hook_event_name → 写 `~/.mam/events/<session_id>.json`（沿用现有事件文件格式与白名单）→ **exit 0 空输出，不打印任何 stdout**。
- [ ] 注册形态：codex 用官方 `commandWindows` 字段（hook_config.rs 实证支持）指向 helper 绝对路径；claude/kimi 用各自配置形态注册同款；三家既有 bash 注册条目迁移（沿用 register_hooks_in_file 迁移骨架——旧命令判据含脚本路径即视为我方条目改写）。
- [ ] 退出码防线写成 helper 内建注释与测试（红线 1）；stdin→事件文件逻辑拆纯内核 + tempdir 测试（零接触真实 ~/.mam）。

### T2 · 审批事件注册扩展（三家）+ 信任门引导
- [ ] `adapter/claude.rs` hook_events 增 `PermissionRequest` 与 `Notification`（Notification 注册需带 matcher=permission_prompt 配置——注册 JSON 的 matcher 字段，参照官方 hooks 文档形态）。
- [ ] `adapter/codex.rs` hook_events 增 `PermissionRequest`、`Interrupt`（注释标注 1s 超时→事件写入瞬时性要求，红线 2）。
- [ ] kimi 新接入：adapter 增 kimi hooks 注册（`~/.kimi-code/config.toml` 的 `[[hooks]]`，PascalCase 事件名，注册 `PermissionRequest`/`PermissionResult`）——kimi 此前无 hook 通道，hook_supported/config 解析/注册路径按 claude/codex 先例新建。
- [ ] 信任门（codex）：注册成功后 log::warn 中文引导（"codex 需在 TUI 内 /hooks 审阅并信任 MAM 钩子一次，事件才会触发"）+ 验收清单项；有条件则加注册后事件真触发自检（#[ignore]）。
- [ ] 验收：三家注册文件形态快照测试；#[ignore] 实机各跑一次会话确认事件文件落盘。

### T3 · 持久等待标记 + 状态链接入 + 两污染层退役
- [ ] 持久标记：审批进入事件（claude PermissionRequest / Notification(permission_prompt)、codex PermissionRequest、kimi PermissionRequest）→ DB 写"会话等待审批"标记（新表或 KV：tool+session_id+ts+事件摘要）；清除信号（PostToolUse/PostToolUseFailure/Stop/UserPromptSubmit/kimi PermissionResult）→ 删标记；会话消失→清。**不用 30s TTL 事件文件承载审批等待。**
- [ ] 状态链接入：`adapter/mod.rs` 叠加层消费标记——有标记 → 会话状态强制 Waiting（红=等待审批，看板/详情/SSE 一致）；标记清除后回落文件推导。
- [ ] 污染层退役（含 2026-09-10"codex 全线不落红"裁决的收窄再修订——本计划落档即视为已裁定，台账记录）：①claude Stop 宽限过期→Waiting 段（adapter/mod.rs:390-393）改映射 Idle（Stop=回合结束≠等审批；等待红由标记承担）；②codex 停更 300s 降级产出的 Waiting→Idle 强转（codex_parser.rs:463-467）保持，**有标记时标记优先**；zcode/workbuddy 停更红不动（无信号工具兜底）。
- [ ] 验收：单测（标记写入/清除/强制 Waiting/清除回落）+ 状态链回归（任务完成后不再假红 25 秒）。

### T4 · 红卡接铃铛（M1A 核心）
- [ ] `approve_options_scan`：available = **等待标记存在** ∨ (status==Waiting && detect(last_message))；标记存在时 options 直接取映射表（跳过 marker detect）；marker detect 降级为无标记时的旧路径。409 守卫（非 Waiting 且无标记→409 not_waiting）与严格档（probe-pending 不出键）语义保留。
- [ ] 可选最小实现：等待标记的 reason 摘要（工具名 + codex 的 tool_input.description 审批理由）透出到卡片副标题；不做也可（卡片显示"该会话等待审批"）。
- [ ] 验收：端点测试（标记→available=true；键位零泄漏断言沿用；409 守卫回归）+ #[ignore] 实机全链（见 T7）。

### T5 · codex custom_tool_call 文件提取（D5）
- [ ] `content.rs::map_codex_lines` 增 `custom_tool_call` 分支：解析补丁头行（`*** Add File: <path>` / `*** Update File: <path>` / `*** Delete File: <path>`）为结构化 args（如 `{"changes":[{"op","path"}]}`，对齐 APP 线路 fileChange 形态）→ SessionMessage::tool_call；旧 `function_call+command 串` 的"不收串内路径"防误收规则保持（只解析头行、不碰补丁正文）。
- [ ] files.rs PATH_KEYS 吸收（origin=tool_write）；正文流同时可见（apply_patch 条目不再消失）。
- [ ] 验收：用 2026-09-20 真实会话 rollout 片段做夹具测试（md×3/svg×1 上板、PNG 照旧、串内路径不误收）。

### T6 · 黑名单子路径豁免（D9）
- [ ] `files.rs` SENSITIVE_DIRS 机制增子路径豁免表（EXEMPT_SUBPATHS，首例 `.claude/plans`，其余按 T0 盘点结果）；豁免只放"产物目录"，凭据/会话数据照拦。
- [ ] 测试锁边界：豁免路径可读、同工具其余路径照拦、路径段精确匹配不误伤。

### T7 · 收口：门禁 + Windows 实机验证 → **停下等评审**
- [ ] 六门禁全绿；#[ignore] 实机矩阵：claude（权限弹窗）/codex（/permissions 切 Read Only 档触发补丁审批）/kimi（默认审批）各一次 → 红卡出现 → 一键批准/拒绝生效 → 审计 approve/reject 落账 → 标记清除、状态回落绿灯。
- [ ] 文件预览实机：plan md 可读、.claude 其余路径照拦；codex 会话 md 出现在文件面板。
- [ ] 台账/handover 记批（含裁决收窄记录）。**停下等主线评审，通过后才开批次乙。**

## §3 批次乙 · 前端体验（T1–T5，甲评审后执行）

### T1 · 计划消息一等卡片（D4）
- [ ] 解析层：ExitPlanMode 形态（tool_use 且 input.plan 非空——**按形态不按工具名**）升格 `kind="plan"` 一等消息（content=计划 markdown，role=assistant）；核对 files/confirm 的 role 归一化（plan 消息不进 user 侧确认比对）。
- [ ] 前端：plan 消息默认展开渲染为常驻计划卡片（复用 e85d109 的 extractPlanBody/markdown 渲染），**豁免总结模式折叠**；**不做**消息合并/重复折叠（用户裁决砍除）。
- [ ] 可选（同批）：详情页活状态流——selected 会话状态随既有 3s 轮询/SSE 更新（红卡与总结模式随状态自动切换，不再要求重进页面）。
### T2 · 修改重发只入队（D6）
- [ ] session-send 增可选 `queueOnly`：跳过直发分支直接入队回 queued；前端"修改"重发路径带该标志；队列自动放行对 queueOnly 项照常（注释写明该语义）。
### T3 · 确认判据收紧 + 中间态回执（D7）
- [ ] 直发确认失败分诊：注入 Ok+戳未中+屏读**无滞留草稿**（=已被 TUI 收进队列）→ 新中性回执「已投递至终端输入，agent 空闲后处理（未确认落盘）」，不提供一键重试；屏读见滞留+补回车后 3s 仍不落盘 → 真失败（可重试文案保留防重警示）。
### T4 · 多列队 UI（D8）
- [ ] MessageComposer 排队态渲染完整 /session-queue 列表（复用轮询）：逐条前十余字符预览（截用户正文、去 [mobile] 前缀）+ 各自 立即发送/撤回/修改 按钮；自动逐条放行表现清晰化（如底部说明"空闲时将按序自动发送"）；单回执槽仅保留最近发送条的送达/失败态。
### T5 · 收口
- [ ] 六门禁；更新验收清单 C-3/C-4/C-9 重测口径；台账记批。

## §4 总验收（八条）

①三家审批红卡 Windows 实机全链（甲 T7）；②codex 状态不再提前绿、钩子零 exit 1；③codex 会话 md 进文件预览；④plan 文件可预览（其余 .claude 照拦）；⑤计划消息远程可见为常驻卡片；⑥修改→排队→手动插队链路符合预期；⑦忙时投递回执不再假失败、无双发诱导；⑧多列队逐条操作可用。六门禁全绿（preset_v2 5 败=360 环境基线）。

## §5 不入本计划

- 重复打印计划 / 同消息多段合并：用户裁决不修（2026-09-20）。
- 全面状态语义重构（八工具统一红灯语义 + 9 个防抖常数收敛）：另立批次独立设计评审。
- 三期/M11 协议路线（opencode HTTP 代答 / ACP / zcode app-server interaction/requestPermission）：issue #74 与 research 库已存档。
- 用户侧事项：Mac Q3 在场验证、清理残留拍板、C 段剩余验收+蜂窝+360 白名单终跑。
