# 远程新建会话（session-create）· C12 验收清单

> 依据：`docs/engineering/specs/2026-09-27-remote-session-create-design.md` §7；
> 计划：`docs/engineering/plans/2026-10-02-remote-session-create-phase-c.md` Task C12。
> 状态标记：【自动】= 实机 E2E / 单测已覆盖并实测通过；【待真机】= 需用户手机走查（Phase C 停点后的真机验收段）。
> E2E 实跑数字与证据：C8 四家矩阵 + 冒烟 2026-10-02 13:35–13:37（2 passed）、C9 HTTP 全链 13:53–13:54（1 passed）；
> 证据目录 `~/.mam/create-evidence/`（每 leg 屏读尾 40 行 + keys/dialog_log 台账）。

## 一、四家全链（新建 → 物化 → 上板）

| # | 项 | 预期 | 实测 | 证据 |
|---|---|---|---|---|
| 1 | claude 全链【自动】 | 信任框（危险默认 ❯ No, exit）→ ↓+Enter 处置 → idle → 首句注入 → 会话文件物化 → 卡上板 | E2E PASS：keys_sent=["down","enter"]（红线实机复核）；sid 取得；首条 user 逐字节="hi [mobile C8E2E]"；模型已回复（assistant 2 条） | `~/.mam/create-evidence/20261002-133*claude.log`；台账入 tests/create_e2e.rs 文件头 |
| 2 | codex 全链【自动】 | 0.160.0 实机态：无更新框/信任框（信任模型机器态漂移）→ 直 idle → 注入 → 物化 | E2E PASS：keys_sent=[]（三态域内）；sid 取得；首条 user（带 codex 自有 instructions 包装）contains composed ✓；模型回合因 CC Switch 代理 400 未回复（环境态，如实记录，不属 create 链） | `~/.mam/create-evidence/20261002-133*codex.log` |
| 3 | kimi 全链【自动】 | 信任框（per-folder）→ Enter 处置 → idle → 注入 → 物化 | E2E PASS：keys_sent=["enter"]；sid 取得；首条 user 逐字节 ✓；模型已回复（assistant 2 条） | `~/.mam/create-evidence/20261002-133*kimi.log` |
| 4 | opencode 全链【自动】 | 无信任框首帧 idle → 注入 → 物化（拷 db+wal+shm 三件套查副本——P4 红线） | E2E PASS：keys_sent=[]；sid 取得；首条 user 逐字节 ✓；模型已回复（assistant 2 条） | `~/.mam/create-evidence/20261002-133*opencode.log` |
| 5 | 更新提示路径【自动】 | codex 更新框（实机构造，探测期 0.156.1→0.157.1）：'2'=Skip（esc 无效、Enter 禁用）；claude 非阻塞横幅不挡 idle | 处置键序由 C5 单测锁定（disposal_keys + codex_update_then_trust_with_char2 等 8 例）；实机 0.160.0 已无更新框（版本差消失）——三态断言域覆盖 | C5 pipeline_tests；探测定案 §5 |
| 6 | 未识别界面报错【自动】 | 连续 15 轮无锚 → failed:unrecognized_screen，零按键（不盲打红线） | 单测 unrecognized_screen_fails_without_any_key + codex hooks 框实测两轮（13:02/13:03 leg）均零按键止血（dialog_stuck/unrecognized） | C5/C8 测试与 evidence |
| 7 | MAM hooks 审查框【自动】（spec §4.8 阻塞语义实机修正，用户在场裁决） | 核验式自动信任：codex hooks.json 全条目命中 MAM 指纹 → '2'+enter（Trust all）；混杂/失败 → esc 跳过 | 单测 codex_hooks_dialog_verified_trusts_and_unverified_skips（'2'+enter / esc 两臂）+ codex_hooks_all_ours_verifies_entries（全我方/混杂/缺失三态） | monitor/hooks.rs 测试；C5 pipeline_tests |

## 二、路径边界（v1 约束）

| # | 项 | 预期 | 实测 | 证据 |
|---|---|---|---|---|
| 8 | 黑名单命中（凭据表全局段）【自动】 | `D:\keys\.ssh\k` / `/Users/u/.claude/x` → 400 blacklisted | C2 四测试 + C6 端点契约测试 PASS；42 例边界探针 0 偏差（.ssh2 不误伤、小写盘符、大小写折叠等） | C2 create_path 测试；C6 create_tool_gate 系列 |
| 9 | 黑名单命中（系统目录表）【自动】 | `C:\Windows\System32\x`、`C:\Program Files\app`、`/System/Volumes`、`/usr/bin` → blacklisted | 同上 PASS；create-projects 列表同规过滤（不给不能用的候选） | 同上 + create_projects 测试 |
| 10 | 盘不存在【自动+待真机】 | create_dir_all 失败 → 400 mkdir_failed | mkdir 失败分支由端点校验链覆盖（C2 契约）；真实不存在的盘符手填场景留真机走查复核 | api.rs 校验链；【待真机】手填 `Q:\demo` |
| 11 | 深层创建【自动】 | 缺失目录递归创建（create_dir_all）成功 | C6 契约测试（合法路径 200 且目录落盘）+ C8/C9 E2E 真实递归创建 | C6 create_rejects_non_ascii_path_and_honors_trim 等 |
| 12 | 非 ASCII 拒绝【自动】 | `E:\项目\demo` → 400 non_ascii_path；候选列表同规过滤 | C2 测试 + C6 契约（create_rejects_non_ascii…）+ create-projects 过滤断言 PASS | C2/C6 测试 |
| 13 | UNC 拒绝【自动】 | `\\server\share\p`、`//server/share` → 400 not_local_volume | C2 测试 PASS | C2 rejects_empty_relative_nonascii_unc |
| 14 | trim 契约【自动】 | 首尾空白路径：校验按 trim 形态过、create_dir_all 落 trim 后路径（盘上无带空格目录） | C6 create_rejects_non_ascii_path_and_honors_trim PASS（评审 I1 钉死） | C6 测试 |

## 三、多实例与配对信号

| # | 项 | 预期 | 实测 | 证据 |
|---|---|---|---|---|
| 15 | 表单黄字（≥1 活跃）【自动】 | 选中 tool ∈ 项目 activeTools → 黄字（与 pairingAmbiguous 分层） | C11 vitest 黄字三态（在→显示/不在→消失/手填→不显示） | CreateSessionSheet.test.tsx |
| 16 | 配对不确定标记（≥2 进程）【自动】 | /sessions 卡 pairingAmbiguous；session-send 回执 pairingHint（不拦截） | C7 两断言 + C9 全链断言新卡 pairingAmbiguous=false | C7 测试；C9 E2E |
| 17 | 陈旧实例防误锚【自动】 | 同目录陈旧 TUI/MCP 子进程/蹦床壳不被锚定 | C8 实机三轮定案（取根 + 屏读内容终判）+ freshest/candidate_roots 单测 | C8 五项实机定案（create_e2e.rs 文件头） |

## 四、审计

| # | 项 | 预期 | 实测 | 证据 |
|---|---|---|---|---|
| 18 | create 行【自动】 | 终态一行 result=ok/failed:<code>，sid=物化者/""，content 不含首句原文 | C6 create_audit_rows_success_and_failure PASS；C9 E2E 实测恰 1 行 ok+sid | C6 测试；C9 E2E |
| 19 | dialog 行【自动】 | 每次弹窗处置逐条留痕（场景+键序，如 `create_trust: down,enter`），sid="" | C6/C9 PASS（claude 信任框一行实查） | 同上 |
| 20 | send 行【自动】 | 首句 composed 原文（设备=发起设备），sid=""；桌面审计页 action 原样渲染零适配 | C6/C9 PASS（send 恰 2 行：首句+第二条，device_name 一致） | 同上 |

## 五、移动端 UI

| # | 项 | 预期 | 实测 | 证据 |
|---|---|---|---|---|
| 21 | 工具四选置灰【自动】 | enabledTools 外 disabled+标「未启用」 | C11 vitest | CreateSessionSheet.test.tsx |
| 22 | phase 中文映射【自动】 | 六值逐值中文 | C11 it.each 逐值经 UI 渲染断言 | 同上 |
| 23 | done→跳转【自动】 | 复用既有导航（App.setSelected），不新造路由；15s 未上板如实提示不伪造跳转 | C11 测试（跳转+晚到提示两臂） | 同上 |
| 24 | 409/404/400 文案【自动】 | 已有任务进行中 / 任务已失效 / reasonCode 中文分診 | C11 测试（409、blacklisted、non_ascii_path、no_task） | 同上 |
| 25 | 手机真机走查【待真机】 | 手机浏览器：新建→进度→跳转→收发全流程 | **待用户真机走查**（Phase C 停点后） | —— |

## 六、自动化 E2E（spec §7 自动化项）

| # | 项 | 预期 | 实测 | 证据 |
|---|---|---|---|---|
| 26 | 四家矩阵 E2E【自动】 | `cargo test --test create_e2e -- --ignored --nocapture --test-threads=1` 全过 | 2026-10-02 13:35 首跑 **2 passed**（四家 27.6/22.4/27.2/24.9s，全过）；17:24 六门禁终跑 **claude/codex/kimi 三家过**、opencode **锚定失败（spawn Ok、find_tui_pid 30s 超时）**——**上游大版本漂移**：opencode 批内自升 1.18.32→2.0.22（服务架构，进程无可附加控制台 err=5/6，启动形态全变），触发 AGENTS.md「工具大版本升级后注入规格复验」条款，归后续探测批（1.18.32 线上全链已两次实机验证）；17:28 复跑同签名再现（非瞬态），同归 opencode2 复验批。**2026-10-02 D2 翻案**：opencode 2.x 适配批修复后复跑 **3 passed 0 failed**（matrix 四家 28.0/22.6/26.0/**22.6s** + http 全链 + claude 冒烟；opencode 腿 22.6s 与三家同量级）；修因二处——① 起窗加 `--standalone`（端口竞争陷阱）；② 消息读取 v2 派发（`session_v2`/`session_message`，stamp 命中路径）。探测定案见 `docs/research/refs/phase2-消息注入/2026-10-02-opencode2-复验定案.md` 与本批 D0 定案 | tests/create_e2e.rs 文件头台账；~/.mam/create-evidence/ |
| 27 | HTTP 全链 E2E【自动】 | POST create → status done → /sessions 新卡 → session-send 第二条 delivered → 审计三类行 | 13:53 首跑 **1 passed**（46.2s）；17:23 终跑复跑 **1 passed**（52.0s，第二条消息落地后 assistant 4 条） | 同上 |

## 七、用户真机验收（停点后执行）

1. 手机浏览器连 MAM → 看板头部「＋ 新建」；
2. 四家各建一次（含黄字场景：选一个已有活跃会话的项目+工具）；
3. 进度态观察（开终端→处置弹窗→注入首句→等待上板）→ 新卡自动跳转；
4. 会话详情收发各一条 → 桌面终端可见 `[mobile <设备名>]` 签名；
5. 手填路径边界抽查：非 ASCII / UNC / 黑名单路径的中文错误文案；
6. 桌面审计页核对 create/dialog/send 行（含失败场景的 failed:<code> 行）。

---

**六门禁终跑记录（2026-10-02，见交付简报）**：cargo fmt --check / clippy --all-targets -D warnings /
cargo test（lib+main+dao+linker 全绿；preset_v2 既有 5 败基线沿用既有口径）/ pnpm check /
pnpm test（create 系隔离全绿；无关 DOM 套件并发负载 flaky 已隔离复跑取证）/ pnpm build:mobile —— 全绿。
