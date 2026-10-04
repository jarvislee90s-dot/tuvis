# 二期收尾总设计 · APP 类注入 + 无头通道 + 交接导出 v2（需求说明书）

> 日期：2026-09-27（2026-10-04 落档）· 状态：**定稿落档**——Phase P+ 探测两端完成 + 裁决门六裁（裁决 12–17）+ 补测暴露缺陷裁决（裁决 18）+ Windows 补测收口；实现批次（C0 起）计划另立文档
> 基线：`origin/main` `78bf114`（v0.5.0-beta.1；M6–M9 注入主线 + M6R–M9R 加固已交付）；工作分支 `feat/phase2-closure-app-injection`
> 上位文档：`docs/MASTER-PLAN.md`（宪法）；`docs/superpowers/specs/2026-09-18-phase2-message-injection-design.md`（二期注入总 spec，本批 = 其 W7/M11 提前扩容 + W9/M10 落地 + 收尾编排；功能点 H 系与旧 spec W 系映射见附录 C）
> 文档关系（用户 2026-09-27 裁决）：**本 spec（总设计）→ 探测阶段计划（单独立）→ 探测结果回传 → 修订本 spec → 实现批次计划（单独立）**
> 功能点编号 H1–H13（H1–H11 按执行顺序；**H12（WB 读侧）/ H13（dsh 写侧）为落档前后整理补号**，批次归属见 §8）

## 1. 目标与范围

**目标**：把「手机发消息」从四家终端 TUI 扩展到 **APP 形态四家（ZCode / Codex APP / WorkBuddy / dsh 桌面端——dsh 的主形态即桌面 APP，web 宿主作兼容保留）**，建成无头通道公共底座（含生命周期控制与 zcode 无头新建会话），补 M7 macOS 终验，使二期进入可收官状态。

**范围**：**本期实施计划 = 第一部分 H1–H13（裁决 19；H12/H13 为整理补号）**——C0 前置修复四件 + C1 无头底座与 zcode/codex APP 注入 + C2 WB/dsh 写侧 + H10 无头新建 + C4 三家 CLI 无头；**M10 交接导出（§6）、收尾杂项（§7，M7 终验已完成）移出本期，另立后续计划**。

**非范围（不做清单）**：
- 常驻连接池、zcode/codex app-server attach（三期 F3.3；codex 单写者锁 open issue #47193 跟踪）
- 切换模型选择器 UI、原生 APP 壳（三期 F3.1 / F3.9；本批仅探测取证斜杠命令等效性）
- 用户自定义交接模板（三期后，裁决 10；本期只做预设矩阵）
- OpenClaw gateway 深做 / dsh 插件生态展开（dsh 写通道本体已定案 ACP stdio 集成〔C2〕；生态扩展不做）
- 任意命令/参数拼接暴露（宪法红线；仅固定 CLI 命令形态）
- 四家 CLI 的远程新建会话（独立轨道 `probe/remote-session-create`；本批只做 zcode 无头新建——该轨道明确排除的 APP 形态）
- 推送网关 / APK 壳（三期收尾，裁决 16）

## 2. 裁决记录（2026-09-27 对齐 + 2026-10-04 裁决门/落档补裁，实施不得重议）

| # | 裁决 | 内容 |
|---|---|---|
| 1 | 工具范围 | **核心四家 APP 形态：ZCode / Codex APP / WorkBuddy / dsh（桌面端为主形态，读写两侧与其他 APP 同权重全链）**；OpenClaw 半天级轻探测（只判「有没有写通道」，文档级结论已出：有条件存在） |
| 2 | zcode 验收线 | **判定 F 基线**（两端实测细化：已信任工作区 = 重启级可见；未信任 = 仅 MAM 可见——见 H7/H10 与附录 A）；`--surface` 无影响的实测结论已归档 |
| 3 | codex APP 通道 | **`codex queue` 主通道**（即 app-server 协议 `thread/queue/add` 的 CLI 形态，与桌面 APP 自身排队同机制）+ `exec resume` 兜底；直接 attach 桌面线程不可行（单写者锁，#47193/#25914/#33556 跟踪） |
| 4 | WorkBuddy 路线 | **A（ACP 内嵌端点）**（定案权原在 PW 探测，已裁——见裁决 12） |
| 5 | 三期项 | 切换模型 / 原生 APP 维持三期；本批仅加「斜杠命令经无头通道等效性」探测取证。**宪法零改动** |
| 6 | Mac 段 | 跑 ① 本批 APP 类探测 + ② M7 终验补课；③ session-create Mac 段已在跑不重复，仅交叉验证 1–2 核心格；报告全文回传 |
| 7 | 实现范围 | M11 三家 CLI 无头（H11）并入与否 → **已裁：并入**（裁决 13，C4 正式批次） |
| 8 | 无头架构 | **方案 A · 一次性进程 per turn**：每条消息 spawn 一次无头命令，回执归一后进程退出；不采纳 AionCore 空闲挂起/重生；claude 审批双向（`--permission-prompt-tool stdio`）为「进程存活至 turn 结束」特例；常驻池留三期 |
| 9 | 无头开启/关闭语义 | = **进程生命周期控制**（turn 结束即退、watchdog、移动端可中止）+ **zcode 无头新建会话**；**不做**每工具分开关的设置页多级开关 |
| 10 | 无头通道默认态 | **默认关，显式开启**（设置页远程区单一总开关；理由：无头 = 绕过终端可视确认直接驱动 Agent，风险高于终端注入，zcode `--mode` 审批档需知情选择） |
| 11 | dsh 桌面端（APP 形态） | **读写两侧全链并入本批，与其他 APP 同权重**：读侧 = H1（宿主判定 + v4 代际 + APP 级跳转）；写侧 = **ACP stdio 集成**（C2 交付；D14 出评已过，见裁决 16；源码级可写 + 鉴权不裸奔，两端 401 互证） |
| 12 | WorkBuddy 路线 | **A（ACP 内嵌端点）定案**——`acp/connect` 免鉴权全链；**Win 端点启用条件（远程控制开关）= C2 开工前置跟进**；B/C 降为备选 |
| 13 | H11 三家 CLI 无头 | **并入实现批**（M11 随本批整体关账，C4 转正式批次） |
| 14 | zcode `--mode` 默认档 | **单一 yolo**（用户裁决；安全面由 H3 总开关默认关 + 知情开启兜底；yolo 粘滞疑云入版本门控复核） |
| 15 | watchdog 默认值 | **600s**（可配；对齐 M10-b 自总结口径，实测样本 8–23s 留足余量） |
| 16 | dsh D14 出评 | **出评通过 + 追加 C2 功能点**（集成走 ACP stdio；MASTER-PLAN D14 修订随 C2 落地提请用户） |
| 17 | L13/L14 缺口排期 | **并入新增 C0 前置小批**（靶向 TTY 修法 + 插队诚实化，先于 C1 交付） |
| 18 | 补测暴露的两处读侧缺陷（2026-10-04 晚） | **并入 C0 扩容**：① dsh 桌面端 rc.2 起会话日志代际升 `session.v4.jsonl.zstd`，解析器版本门未放行（正文「无消息」/新会话不上板的根因）→ 放宽正则（H1 扩容）；② WB 5.7.3 交互会话心跳废弃（Test2 会话不上板的根因）→ 发现层改 workbuddy.db 双源（新增 H12） |
| 19 | 实施计划范围（2026-10-04 落档时） | **本期实施计划只覆盖第一部分 H1–H13**（H12/H13 为整理补号，对应裁决 18 的 WB 读侧与裁决 11/16 的 dsh 写侧；C0 前置修复 + C1 底座与 zcode/codex APP 注入 + C2 WB/dsh 写侧 + H10 无头新建 + C4 三家 CLI 无头）；**M10 交接导出（§6 三件）与收尾杂项（§7，M7 终验已完成）移出本期，另立后续计划** |

## 3. 现状与证据基线（2026-09-27 立项时快照；探测后演进以附录 D 为准——如 codex 0.156.1→0.160.0、WB 内嵌 2.115→2.137/5.7.3、dsh 端口归属等）

1. **zcode**：APP 内嵌 CLI（`resources/glm/zcode.cjs`，本机 0.16.9）无头面本机实证——`--prompt` / `--resume sess_xxx` / `--cwd` / `--json` / `--attach` / `--mode build|edit|plan|yolo` / `--surface terminal|desktop`；旧探测（3.11.2）证 zcode:// 深链无会话级路由；用户实机 + 0.16.5 spike = 判定 F（APP 重启后可见）。运行形态：`ELECTRON_RUN_AS_NODE=1 ZCode.exe <安装目录>/resources/glm/zcode.cjs …`（Windows）。
2. **codex**：官方 2026-08 新增 `codex queue --thread <UUID或会话名> --message <文本>`（本机 0.156.1 在场，含 `--image`/`--model`），走共享 daemon `thread/queue/add`——**与桌面 APP 自身 follow-up 排队同机制**，线程发现含桌面（Atlas/ChatGPT）会话（openai/codex PR #39092）；`codex exec resume` 追加 APP 会话 rollout 被 issue #28259 实证（索引刷新不保证）；0.155.1+ 引入会话单写者锁（#46652/#47193）。
3. **WorkBuddy**：会话心跳（`~/.workbuddy/sessions/<PID>.json`）暴露每会话 `url/endpoint`（如 `http://127.0.0.1:35923`），内嵌运行时 `cli/bin/codebuddy`（cwd `workbuddy-host-cli`）；官方调研：CodeBuddy CLI 为独立一等产品（npm `@tencent-ai/codebuddy-code`），带 `-p/--print` 无头、`--resume`、`--serve` HTTP API（`POST /jobs/:id/reply` 向运行中任务追加消息、`/api/openapi.json` 规范）、ACP、SDK；官方文档明示 WorkBuddy = 「使用 CodeBuddy 引擎的应用」且 `CODEBUDDY_CONFIG_DIR` 隔离共存。
4. **dsh 桌面端**（v0.2.0-rc.2 本机运行中）：DeepSeek 桌面 harness = Electron 壳 + 内嵌核心进程（`DeepSeek Harness.exe --expose-internals …\@deepseek-ai\dsh-desktop-host\lib\index.js … ~/.dsh\profiles\desktop`），**数据与网页版同源**（host 显式指向 `~/.dsh`，sessions/ 当天在写）；进程 cmdline 不含 `dsh`+`web` 双令牌 → 现行宿主判定**看不见桌面端**（读侧盲区）；host 进程监听 localhost 动态端口（实测 19387）= 写通道线索；仓库 MIT 开源且本机在库（`packages/` 含 `host`/`api`/`sdk`/`acp`/`webhook`/`jobs`）。

## 4. 写通道总体架构

三族通道 + 一套底座：

```
移动端发消息（/m/api/v1/session-send，PIN+gate 复用）
        │
   路由表（W3 扩展：会话宿主形态 × 工具 → 通道 + 可见性预期）
        │
 ┌──────┼──────────────┬─────────────────┐
 │ 终端注入（在产）    │ spawn 型无头（新）│ HTTP 型（新）
 │ claude/codex TUI/   │ zcode.cjs --prompt│ WB：ACP over HTTP
 │ kimi/opencode TUI   │ codex queue/exec  │ （acp/connect →
 │                     │ claude -p / kimi  │  session/prompt）
 │                     │ -p / opencode run │ dsh：ACP over stdio
 │                     │ dsh ACP stdio     │ （spawn 子进程讲协议）
 └──────┴──────────────┴─────────────────┘
        │
 通道适配层：统一回执结构 {status, sessionId, lastAssistant, tokens, durationMs, stage?, reason?}
        │
 公共底座：无头总开关（默认关）· 按会话串行 · watchdog(600s) · 取消 · 版本门控 · 审计(action=headless)
```

- **spawn 型**：spawn 命令 → 流式读 stdout（`--json`/stream-json）→ 回执 → 进程退出；dsh ACP stdio 同族（spawn 子进程讲 JSON-RPC，turn 结束即退）；
- **HTTP 型**：WorkBuddy ACP 端点调用 → SSE 流事件归一（同一回执结构）；
- **终端注入型**：在产四家不动，与无头互不路由到同一会话（W3 既有裁决）。

## 5. 第一部分 · APP 类注入与无头通道（H1–H13）

> **探测任务书编号**（**✅ 已全部执行完毕，Windows + Mac 两段定案见附录 D**；任务级细节存档于计划书 `2026-10-03-phase2-closure-probe.md` 与 research 本地报告）：
> **PZ** = zcode 八问（resume 全链 / 可见性三态+surface / 并发 / `--json` 回执 / `--mode` / 斜杠 / 版本 / **无头新建会话出现条件**〔H10 依赖〕）· **PC** = codex 五问（queue 触达〔**空闲+运行态各测**〕/ 回执形态 / exec resume 兜底 / id 映射 / daemon 前置）· **PW** = WorkBuddy 五问 · **PL** = OpenClaw + dsh 写通道 · **Mac 段** = ① 三家 APP 在 Mac 的对齐探测（PZ/PW/PC 核心项）② M7 三通道注入终验 ③ session-create Mac 段核心项交叉验证。

### H1 · dsh 桌面端读侧接入（前置件，裁决 11）

- **需求**：DeepSeek 桌面 harness 的会话照常上板——修复宿主判定对桌面端的盲区（现存问题，与注入无关，先行交付）。
- **输入**：dsh 宿主判定的单源核 `monitor/dsh/mod.rs::cmdline_is_dsh_host`（进程发现与 `host::tool_host_alive_in` 共用，零漂移）扩桌面特征：**任一参数含 `dsh-desktop-host`（包路径子串）或以 `\.dsh\profiles\desktop` / `/.dsh/profiles/desktop` 结尾**即桌面宿主（`--expose-internals` 为 Electron 通用旗子，单独过弱不作独立判据——实测桌面 cmdline 必含前两特征）；会话数据源不变（`~/.dsh` 同源，零迁移）。
- **输出与效果**：桌面端会话照常上板（projcache 复用；zstd 解析链**仅放行 v4 代际**——rc.2 起日志代际升级，裁决 18，C0 交付）；三色状态/标题预览口径同网页版；跳转 = 聚焦桌面 APP 窗口（复用 `window/` 聚焦链：Windows win32 / macOS 应用激活；网页版的 `dsh_tab` 浏览器路径保留给网页宿主）；带回归测试（网页版 cmdline 夹具 × 桌面端 cmdline 夹具 × v4 代际夹具三守卫）。
- **边界**：只修读侧；写通道归 H2；平台聚焦能力缺失时降级「仅上板不跳转」如实标注；dsh rc 阶段迭代快，令牌与数据格式漂移由风险 11 兜底。
- **2026-10-04 补测收口**：GUI 核验完成——上板 ✓、APP 级跳转 ✓（会话级无深链，3.11.2 时代已证，预期内）；**数据落点未漂移**（当晚新会话仍写 `~/.dsh/sessions/`），但**桌面端 rc.2 起日志代际升级为 `session.v4.jsonl.zstd`**，解析器版本门只认 v0/v2/v3（`log.rs` 注释原文「未来 v4 只需放宽正则」）→ 正文「无消息」与新会话不上板即此因。**修法（裁决 18，落 C0）**：放宽 `parse_generation` 正则认 v4 + 代际测试补用例（取代际最大逻辑天然兼容）。

### H2 · OpenClaw / dsh 写通道探测（**✅ 已完成**，结论见附录 D；dsh 写侧实现 = H13/C2）

- **需求**：探测「有没有可集成写通道」。OpenClaw 半天级只出一格结论；dsh = **源码通读 + 端点实测**（开源仓库本机在库，不用逆向）。
- **输入**：OpenClaw = sessions CLI / gateway API 面（本地 gateway 在场实测）；dsh = ①直读仓库源码（`packages/host` 桌面 host 的本地 API——动态端口发现机制、路由、鉴权；`api`/`sdk`/`acp`/`webhook`/`jobs` 包的编程面）；②对自建会话相关端点的只读 GET 实测（源码证实存在投递端点且目标为自建会话时，可 POST 验证一次）。
- **输出与效果**：两家各出一格结论「存在写通道（是/否）+ 依据 + 集成形态草图」；dsh 候选形态预填：桌面 host 本地 API（若含会话驱动端点）/ ACP / SDK。「是」→ 追加实现批功能点（届时补需求节），「否」→ 延后表登记复核时机。
- **边界**：本节只探测不实现；OpenClaw gateway 深做 / dsh 插件生态展开仍属非范围；宪法 D14「dsh 写通道另评」以本探测结论出评（流程见 §13）。

### H3 · 无头通道总开关

- **需求**：无头注入（H7–H11）**默认关闭，显式开启**（裁决 10）；用户知情后才暴露该安全面。
- **输入**：设置页远程区「无头注入」开关（**单一总开关**，裁决 9 不做每工具分开关）。
- **输出与效果**：开关状态持久化 settings 表并随 `remote_status` 下发；关闭态下移动端对无头路由会话的发送入口**置灰 + 标因**（「无头通道未开启，请在电脑端 MAM 设置中开启」）；开启动作弹一次性安全说明（无头 = 绕过终端可视确认直接驱动 Agent；zcode 审批档位选择）；开关翻转写审计。
- **边界**：不影响终端注入四家（不经此开关）；远程总开关关闭时无头入口自然一并不可达（gate 在前）。

### H4 · 无头进程生命周期控制（裁决 9 用户点名功能点）

- **需求**：无头进程的开启（spawn）与关闭（回收）全程**受控、可见、可中止**；turn 生命周期 = 进程生命周期（裁决 8）。
- **输出与效果**：
  - **自然路径**：spawn → 流式回执 → 进程自然退出 → 回执终态；
  - **超时**：watchdog（**默认 600s 可配——2026-10-04 裁决 15 定值**；两端实测 turn 仅 8–23s，600s 对齐 M10-b 口径留足余量）到点 kill **进程树**（Windows `taskkill /T`，避免孤儿子进程；**升级候选 = Windows Job Object 整树收编**〔CREATE_SUSPENDED + KILL_ON_JOB_CLOSE + TerminateJobObject 热终止〕——AionCore 生产用法，见附录 E-⑥）+ 分阶段失败回执（stage=timeout）+ 可重试标注；
  - **用户中止**：移动端回执卡「取消」按钮（turn 进行中可见）→ 主动 kill 进程树 + 审计 `action=headless_cancel` + 回执终态「已取消」；与 watchdog 互斥（先到者生效，回执注明由谁终止）；
  - **崩溃**：非零退出 → 回执含退出码 + stderr 尾行；**不自动重试**（重发是用户动作，回执卡一键重发按钮）；连续崩溃 N 次后的通道熔断提示（N 探测批建议）；
  - **MAM 退出/重启**：在飞无头进程优雅关闭（各工具对 kill 的落盘行为 = 探测项——重点 zcode SQLite 半 turn 落盘、codex queue 幂等性）；MAM 重启后无孤儿进程残留自检；
  - **全局并发上限**：同时无头 turn 数上限（默认 2，可配——防手机端连点多会话打爆机器；超额请求即时排队回执含全局队列位置）。
- **边界**：不采纳空闲挂起/崩溃重生（裁决 8）；取消粒度 = turn 级（不提供「暂停」）。
- **配置落点**：watchdog 超时与并发上限随 H3 开关同住设置页远程区「无头」子区（开关 + 超时 + 并发上限三件，不另开页面）。

### H5 · 无头审批与权限档

- **需求**：无头 turn 不因审批静默卡死（W6 既有「三机制必居其一」承诺沿用）；权限档位用户知情可控。
- **输出与效果**：**claude** = `--permission-prompt-tool stdio` 双向（control_request → 移动端审批卡 → control_response；进程存活至 turn 结束——裁决 8 特例）；**codex exec / kimi / opencode** = 策略驱动（spawn 时给定 approval policy，turn 不阻塞；拒绝/需批准事件回流回执，可调策略重试）；**zcode** = `--mode` 档位（**2026-10-04 裁决 14：默认单一 yolo**——Mac 实证 build 档在无审批客户端时阻断一切工具执行（`No permission client configured for Bash`），yolo 保可用性；安全面由 H3 总开关默认关 + 知情开启兜底；plan→yolo 切换粘滞疑云入版本门控复核；后续收紧档经设置扩展）；**watchdog 兜底**覆盖全部 spawn 型通道；queue 通道（H8）的投递命令本身即短命进程，超时保护仅覆盖投递阶段，turn 执行阶段归 codex APP 自身（如实标注，不承诺三机制）。
- **边界**：codex queue 通道无审批面（H8 边界重申）；本批不做权限档的移动端主动切换（三期 F3.1），仅 spawn 档位选择 + 展示。

### H6 · 回执、审计与版本门控（横切）

- **需求**：无头写动作与终端注入同标准的回执诚实性、审计可查性、版本漂移防御。
- **输出与效果**：
  - **回执归一**：`{status: ok|queued|failed|cancelled, sessionId, lastAssistant(截断摘要), tokens?, durationMs, stage?, reason?}`；stage ∈ spawn/version_gate/timeout/crash/channel_error/dialog；移动端按 stage 分診文案；
  - **审计**：`action=headless`（及 `headless_cancel`）逐条入写审计表：设备、会话、通道、命令形态、内容摘要（对齐 W5 口径）、回执终态、耗时；设置页审计视图同口径可查；
  - **版本门控探针**：投递前校验各工具 flag 面（zcode `--resume/--prompt/--json` 在场 / codex `queue` 子命令在场 / codebuddy `--serve` 或 `-p --resume` 在场），结果缓存 + 版本变化提示复核，不盲发。
- **边界**：回执不回传消息全文（摘要口径）；探针失败 = version_gate 拒发回执。

### H7 · zcode 无头发消息（在册会话）

- **需求**：手机对 ZCode APP 的**在册会话**发消息，无头执行一个 turn，手机实时收回执；APP 侧按判定 F 可见（刷新/重启后），不谎报。
- **输入**：session_id（`sess_...`）+ 消息文本 + 设备花名（gate 过闸时记录）；会话所属项目目录（读链路已有）。
- **输出与效果**：版本门控探针通过后 spawn（**命令构造按平台分叉，两端定案**：Windows = `ELECTRON_RUN_AS_NODE=1 <ZCode.exe> <安装目录>/resources/glm/zcode.cjs …`；macOS = 同骨架但**必须设 `ZCODE_BUILTIN_PROVIDER_CONFIG_FILE=<Resources>/config/provider/zcode-builtin.json`**——Mac 打包布局 bug 反编译定案，不设则 `--prompt` 静默无 JSON；版本探针须 `--prompt` 干跑——`--version/--help` 不需要 provider config 会漏判）：
  `--prompt "<文本> [mobile <花名>]" --resume <sess_id> --cwd <项目> --mode yolo --json`
  → stdout 解析（**跳过非 JSON 前缀行**——Mac 实证 `ZCode Built-in missing/skipped (not-due)` 污染）→ 归一回执（末条 assistant 摘要 + token 用量 + 耗时 + sessionId）→ 移动端回执卡；可见性提示**两端定案文案**：「已信任工作区：重启 ZCode 应用后可见；未信任工作区：仅 MAM 可见」。消息落库经读链路自然上板（手机/桌面 MAM 可见）。
- **边界**：**在册工作区限定**（F2.3/D6 不变）；`--mode` 默认 yolo（裁决 14，H3 兜底）；同会话串行 MAM 自建维持（zcode 无头不拒绝并发，PZ 定案）；**APP×无头并发 = 工作区级瞬态争用锁**（Mac 逐变量排除定案：APP 活跃工作区 → `Model creation failed` 1s，APP 空闲即恢复；争用型非排他型）→ **注入前置「APP 工作区活跃探测 + 探活重试」，冲突回执「工作区忙，稍后自动重试或手动再发」**；多行 `\n` 归一与 W4 同口径；斜杠命令字面化（PZ 定案：不等效，MAM 侧拦截或明示）。
- **⚠️ Task 8 实机修订（2026-10-05，Windows + ZCode v0.16.9，硬证据——更正本节字面）**：
  1. **provider config 在 Windows 同样需要**（本节原字面只说 macOS）：真机 CLI 自报查找表为
     `<root>\resources\glm\provider\zcode-builtin.json` 与 `<盘>:\config\provider\zcode-builtin.json`，
     而装包**实际**把文件放在 `<root>\resources\config\provider\zcode-builtin.json`（真机 stat 实证）
     ——**与 Mac 同款的打包布局错位**；不设 `ZCODE_BUILTIN_PROVIDER_CONFIG_FILE` 时（Windows 上）
     `--prompt` **直接失败**：`无法定位 CLI ZCode Built-in Provider Config：…`（exit 1、0.5s 内退出、
     不触模型）。**两端统一按 cjs 反推路径**（`<Resources>/config/provider/zcode-builtin.json`）；
     实现取「macOS 无条件设、Windows 推导路径在场才设」（见 `inject/headless/gate.rs::provider_config_env`）。
  2. **版本探针不是 `--prompt ""` 干跑，而是**一次**真实最小模型回合**：真机 CLI 拒绝空载荷
     （`--prompt requires non-empty text.`）——空串探针在真机**恒失败**，会把通道变成永久拒发。
     现探针载荷 = 最短非空（`hi`），结论按 (exe 在场性 + mtime) **成功长缓存 / 失败 5 分钟 TTL**
     （失败不得钉死通道；见 `gate.rs::PROBE_PROMPT` / `FAILURE_TTL_MS`）。
  3. **回执真源 = 会话库，不是 stdout**：真机 `--resume` 回合进程 exit 0 且会话库确有回复
     （末条 assistant + `tokens.output`），但 **stdout 没有可解析 JSON**——故 stdout 只作**完成信号**
     （退出码 / 看门狗 / 争用串），`lastAssistant`/`tokens` 取自 `~/.zcode/cli/db/db.sqlite` 只读读链路
     （确认判据 = 回合前后「末条 assistant 消息 id 变了」，有界轮询 3×1s）；stdout JSON 路径保留为**第一优先**
     （未来子命令仍可能出 JSON）。

### H8 · codex APP 托管会话发消息

- **需求**：手机对 **Codex 桌面 APP 托管**的会话发消息；CLI TUI 托管的会话维持既有终端注入不动（双形态分派）。
- **输入**：session_id + 文本 + 花名；MAM session id ↔ codex thread id 映射（**两端定案：取 rollout 文件名 UUIDv7；`session_index.jsonl` 滞后 13 天实证不作 id 源；只用 UUID，禁用会话名**——Mac 重名普查 202 thread 中「hi」×5）。
- **输出与效果**（**两端定案，Mac 主证**）：**按 APP 在场与否分派**——APP 开（thread 被打开/索引）→ `codex queue --thread <UUID> --message "<文本> [mobile <花名>]"`（回执 = `Queued message <msg-id>` + exit 0，**只证明入队不证明投递**；消费确认自建：直查 `~/.codex/queue_1.sqlite` 的 `queued_items` 或 rollout 追加侦测）；**queue 前置校验目标 thread 是否被 APP 打开**——未打开 = 永久滞留（Mac 实测 19.2min），提示并自动改道 exec resume 或引导用户在 APP 打开；APP 关 → `codex -C <项目> exec resume <UUID> "<文本>" --skip-git-repo-check`（**`-C` 是全局 flag 必须置于子命令前**，Mac 实测纠正——置于后有 exit 2）。可见性 = APP 原生排队（打开态空闲 ~20s 消费 / 忙态回合结束后投递，FIFO 保序）。
- **边界**：`codex exec` 自建会话不进 session_index → 不能用 queue 投递；不 attach 桌面线程（单写者锁 #47193）；**插队原语 = `turn/steer`（协议面，CLI 无 flag）、审批 = `turn/start approvalPolicy`（协议级）——本批不做**，schema 已全量归档为三期 F3.1/F3.3 素材；queue experimental 入版本门控 + **CLI 版本与 APP 内嵌版本双坐标探测**（APP 改名 `ChatGPT.app`/`com.openai.codex`，按 bundle id 识别）。

### H9 · WorkBuddy 注入（**路线 A 定案**，裁决 12）

- **需求**：手机对 WorkBuddy 会话发消息（活跃会话投递 + 可选新建）。
- **输入**：session_id + 文本 + 花名；**端点发现 = 心跳 `~/.workbuddy/sessions/<pid>.json` 的 `endpoint` 字段**（每会话异端口；**按活跃轮询**——心跳文件按需生成，查空 ≠ 形态不存在，两端定案）。
- **输出与效果**（ACP 全链，Mac 实证）：`POST <endpoint>/api/v1/acp/connect`（**免鉴权**）→ `{connectionId, sessionToken}` → `POST /api/v1/acp`（头 `acp-connection-id` / `acp-session-token` + `Accept: application/json, text/event-stream`）→ `initialize` → **活跃会话 = `session/load` + `session/prompt`；新建 = `session/new` + `session/prompt`** → SSE 流事件归一回执（agentPhase / session_update）。可见性 = APP 内实时（端点即宿主运行时）。
- **边界**：**prompt 仅对新建/活跃会话生效**——对已结束会话（load 后 `endReason=end_turn`）静默挂（接口语义，非鉴权）→ 已结束会话的复活语义（重开回合）C2 实现时定案；ACP 新会话**不进 `workbuddy.db`** 仅落 `~/.workbuddy/projects/<munged-cwd>/` 转写 → 读链路补扫 `projects/`；Permission Mode 协议级四档（default/acceptEdits/plan/auto）只读展示，切换留三期 F3.1；**端点发现的平台差异（补测定案）**：macOS = 心跳 `endpoint` 字段直读；**Windows 5.7.3 = 无交互心跳且无 per-session serve 端点**（3 端口实测均非 CodeBuddy 形态）→ 端点启用条件（APP 内远程控制开关）= **C2 开工前置跟进（风险 16）**，期间路线 B/C 为备选；API 为逆向 bundle 所得（非公开文档）→ 版本门控覆盖 WB 升级漂移；不修改 WorkBuddy 安装本体。

### H10 · zcode 无头新建会话（裁决 9 用户点名功能点）

- **需求**：手机上直接**无头创建 zcode 新会话并注入首句**，APP 可见后接着用——补上「远程新建会话」轨道明确排除的 APP 形态一格。
- **输入**：项目路径（候选列表 = 在册工作区〔zcode recentProjects 口径，F3.8 前置感知〕∪ 看板快照项目 ∪ 手填完整路径——手填校验/黑名单/递归建目录复用 session-create spec §2 同款规则）+ 首句（可选，默认探针 `hi`——会话物化条件 PZ 复核）。
- **输出与效果**：spawn
  `ELECTRON_RUN_AS_NODE=1 <ZCode.exe> <…>/zcode.cjs --prompt "<首句> [mobile <花名>]" --cwd <项目> [--surface desktop] --mode <档> --json`
  （`--surface` 无差异，两端定案）→ 新 `sess_id` 回执 → 会话经读链路自然上板 → **可见性提示两端定案：项目在 APP 已信任 → 「重启 ZCode 应用后可见」；未信任 → 「仅 MAM 可见」**；同项目已有活跃 zcode 会话 → 黄字提示（复用配对不确定门信号，不拦截）。**候选列表口径（两端定案）：以 APP 已信任工作区清单（recentProjects）为主源**——未信任目录的新会话 APP 永不收录，如实标注。**移动端入口**：新建表单的工具选择器新增 zcode 分组（本批独立交付；与四家 CLI 新建入口的 UI 融合留 session-create Phase C）。
- **边界**：仅 zcode；路径黑名单同源文件预览黑名单（session-create 口径）；不做无头「删除/归档」会话；宪法登记动作见 §13。

### H11 · claude / kimi / opencode 无头（**已裁并入**，裁决 13；M11 随本批关账）

- **需求**：未开窗的 CLI 会话也能被驱动（W7 原需求）。
- **输出与效果**：`claude -p --resume <session> --output-format stream-json`（+`--permission-prompt-tool stdio` 审批双向，裁决 8 特例）/ `kimi -p -S <id>` / `opencode run`，全部走 H3–H6 底座；回执与可见性同口径。
- **边界**：无头对已开 TUI 的会话默认不路由（W3）；三家命令面为 B/C 级官方证据，C4 实现首任务逐家实机验证。

### H12 · WorkBuddy 会话发现双源（读侧修复，裁决 18）

- **需求**：WB 5.7.3（Windows 实测）起**交互会话不再写 `sessions/<pid>.json` 心跳**（目录里仅 prewarm 池会话的瞬态残留，会话结束即清理）——MAM 的心跳驱动发现失明，会话不上板（Test2 实证：转写与 db 行都在，看板无卡）。
- **输入**：发现层双源——①心跳路径保留（旧版兼容；macOS 5.4.7 仍写，且**按需生成、转瞬即逝**，轮询判读不能靠单次查空否定形态）；②**`~/.workbuddy/workbuddy.db` 的 `sessions` 表**（新真相源：`id/cwd/title/status/transport/source_mode/permission_mode/updated_at` 等 40 列；Windows 实测 Test2 会话 `3f12ca20` 即在表中）。
- **输出与效果**：db 触发 = `workbuddy.db` 文件 mtime 轮询（WAL 活跃，实测分钟级更新）→ 读副本查询（活库不直查纪律）→ 会话上板；三色状态按 `status` 列（completed/terminated…）映射对齐既有口径；心跳与 db 双源并集去重。
- **边界**：只读 db；`transport`/`source_mode` 等列的完整语义 C0 实现时对齐（实测 transport=local / source_mode=craft/working）；ACP 写通道（H9）不依赖本节——但**端点发现受连带影响**（见风险 16：Windows 无 per-session serve，端点启用条件为 C2 前置跟进）。

### H13 · dsh 无头发消息（ACP stdio，C2；整理补号——原散落于裁决 11/16 与 C2 行的实现需求收拢）

- **需求**：手机对 dsh 桌面端（主形态）/ 网页版宿主的在册会话发消息——补齐四家 APP 形态的写侧最后一块。
- **输入**：session_id + 文本 + 花名；会话 id 映射 = `~/.dsh/storages/session_projcache/sessions/*.json`（identity 强校验沿用 M1 读侧同源口径）。
- **输出与效果**（探测定案形态）：spawn dsh 的 **ACP stdio** 子进程（一次一进程，裁决 8 同构；具体 CLI 入口与 argv 面 = C2 首任务实机定案——源码级已证 `session/prompt` + `session/resume` 完整、冷会话自动 resume、queue=排下轮 / steer=插队）→ `initialize` 能力协商 → 会话定位（new/load）→ **`session/prompt` 投递** → 事件流归一回执（回执结构同 H6）。鉴权按 dsh 既有 token→HMAC cookie 模型（`/?token=` 流程，不裸奔；ACP stdio 路径的鉴权形态 C2 定案）。
- **边界**：写侧与 H1 读侧共用 `~/.dsh` 数据源但互不干扰；`queue`（排下轮）为默认投递语义、`steer`（插队）登记为协议面后续项（对齐 H8 的 turn/steer 同款口径）；webhook 只能新建不能追加（探测定案，不采用）；桌面 host 的 HTTP API（19387 族，401 鉴权）作为备选通道登记不主推；可见性 = dsh 自带 UI 刷新（同判定 F 量级，如实提示）。

## 6. 第二部分 · M10 交接导出与交接配置（W9 落地 + 配置功能点）

### M10-a · 交接导出双档（W9 全量定案落地）

按总 spec W9 已定案内容实现，**不重议**：HANDOFF v1 九段式骨架恒定；分类矩阵 10 格（编码/设计/文档 × 进度环节，封闭枚举）；提示词 = 公共骨架 1 + 类型模块 3 + 进度模块 10 组件拼装；模板三级选择（用户显式 > 自动预判启发式 > 公共兜底）；**双档执行**（注入自总结默认档：模板提示词经注入通道发给会话本体 → Agent 在项目根写 `MAM-HANDOFF-<短id>-<时间戳>.md` → MAM 检测落盘 → **自动收割转移**至 `~/.mam/handoffs/<规范名>.md` + SQLite 索引 + `.json` 附本，超时兜底规则摘要；规则摘要兜底档：零依赖离线，不可注入/黑盒/APP 形态会话适用）；强制保留项（失败路径清单 + 带预期结果的验证步骤）全格恒在。
**本批新增关联**：zcode/codex APP/WorkBuddy 会话注入落地后，其「交接」自动升级为注入自总结档（原仅规则摘要）——注入通道在本批**含无头通道**（模板提示词经 H7–H9 通道投递给会话本体），H7–H9 与 M10 的验收交叉点。

### M10-b · 交接配置方式（用户点名功能点）

- **需求**：交接行为的用户可配置面集中一处，明示默认值。
- **输入**：设置页「交接」区。
- **输出与效果**：
  - `.json` 无损附本开关（**默认开**，W9 定案）；
  - 注入自总结超时时长（默认 10 分钟，可配，超时自动降规则摘要并回执）；
  - **handoffs 保留期策略**（v1.5 裁决落地项）：保留时长档位（默认 90 天 / 180 天 / 永久）+ 手动「清理过期」按钮 + 当前占用体积展示；清理动作写审计；
  - 「打开交接目录」按钮（系统文件管理器）。
- **边界**：不自定义模板/组件（裁决 10）；不动 MAM 之外的目录。

### M10-c · keystroke 应急预案文档（W10）

文档级交付（D7）：适用场景、前置条件、风险（抢焦点/前台/输入法/辅助功能权限）写入 docs；不实现、不暴露 UI。

## 7. 第三部分 · 收尾杂项（**裁决 19：本节整体移出本期计划，转后续批次**）

- **M7 macOS 终验补课**：**✅ 已由 Mac 段完成**（三通道文本注入 ✅ / 黄排队 ✅ / 锁屏 ✅ / 立即发送插队 ❌ 假成功 → L14 → C0 修复；m9r E2E 套件 Windows-gated 登记为 L3 跨平台套件缺口）——遗留仅 L14 修复与 L3 测试债。
- **W13 B 兜底「发」半部补全**：H7–H9/H13 落地后，「不可注入会话经无头驱动」路径自然成立，resume 窗口的边界条款在验收中关账。
- **远程新建会话 Phase C**：引用 `2026-09-27-remote-session-create-design.md`（独立轨道）；本批 H10 与其共享移动端「+ 新建会话」入口 UI 的信息架构，UI 融合在其 Phase C。
- **README / CHANGELOG / 发版收尾**：功能矩阵注入列按实测定案更新（四家 APP 形态从 ❌ 升 🧪 或如实维持）；发版流程沿用 v0.5.0-beta.1 流水线。

## 8. 里程碑与批次编排（C 系，顺序固定；H 系按执行顺序对应）

| # | 交付 | 出口标准 |
|---|---|---|
| C-P+ | H1 dsh 读侧前置修复（一天级先行）+ H2 探测批（PZ/PC/PW/PL + Mac 段） | 报告落盘 `research/refs/phase2-消息注入/`；dsh 桌面端会话上板实机核验；裁决门过用户（**✅ 2026-10-04 六裁，见 §2 裁决 12–17**） |
| C0 | 前置修复批四件（裁决 17+18）：① L13 注入靶向 **TTY 精确匹配**（agent 进程 TTY ↔ tmux pane_tty / Terminal tab tty / iTerm session tty；取不到 TTY 回退 cwd，多候选**拒绝注入并报错**）② L14 macOS 插队**诚实化**（确认面不可达报中性 `submitted` 不报 `delivered`，比照 codex 范式）③ H1 扩容：dsh 日志代际 **v4 放行**（版本门开正则）④ H12：WB 发现层 **workbuddy.db 双源** | 同 cwd 双开不乱窜（Mac 场景复测）；macOS jump 不再假成功；**dsh 卡正文恢复 + 新会话上板（Windows 实测）**；**WB Test2 类会话上板（Windows 实测）**；A1 分层确认闭环实测 |
| C1 | 无头底座（H3/H4/H5/H6）+ zcode 注入（H7）+ codex APP 注入（H8） | 手机→zcode 在册会话全链（回执 + 重启级可见性兑现）；codex APP queue 全链；开关默认关实测；取消/watchdog(600s)/审计逐条可查；`#[ignore]` E2E |
| C2 | WorkBuddy 路线 A（H9；**端点启用条件 = 开工前置跟进**，风险 16）+ dsh 写侧（**H13**，ACP stdio 集成；裁决 11/16） | WB ACP 全链实机（端点就绪后）；dsh ACP stdio 注入全链；两家失败面如实登记 |
| C3 | zcode 无头新建（H10）+ M10 交接导出（M10-a/b/c）——**M10 三件移出本期计划（裁决 19），随 H10 交付后另立批次** | 无头新建全链（可见性两端定案口径）；M10 部分转入后续计划 |
| C4 | H11 三家 CLI 无头（**已裁并入**，裁决 13） | W7 口径：三家无头全绿 + claude 审批双向实机 |
| C5 | 收尾（**整体移出本期，裁决 19 → 后续计划**）：README/CHANGELOG + 发版（M7 终验已完成〔Mac 段〕，L14 修复在 C0） | 后续计划 |

依赖关系：C-P+ → **C0** → C1 → C2/C3（底座先行）；C4 依赖 C1；M7 终验修复项经 C0 提前化解。

## 9. 输入输出总表

| 功能点 | 输入 | 输出 / 效果 |
|---|---|---|
| H1 dsh 桌面端读侧接入 | 宿主判定门扩两强特征（dsh-desktop-host 子串 / profiles\desktop 后缀）+ v4 代际放行 | 桌面端会话上板（projcache 复用 + 代际门开 v4）；跳转聚焦桌面窗口（win32/应用激活）；三夹具回归测试（网页版 cmdline × 桌面端 cmdline × v4） |
| H2 OpenClaw/dsh 写通道探测 | OpenClaw gateway 实测；dsh 源码通读（packages/host 等）+ 端点 GET 实测 | 有/无写通道结论 + 依据 + 集成形态草图；dsh 候选 = host API / ACP / SDK |
| H3 无头总开关 | 设置页远程区单开关 | 默认关；关闭置灰标因；开启一次性安全说明；审计 |
| H4 生命周期控制 | 回执卡取消 / watchdog / 进程退出 | turn=进程；超时 kill 树；取消审计；崩溃不自动重试；MAM 退出优雅关闭；全局并发上限（无头子区三件配置） |
| H5 审批与权限档 | 无头 spawn 档位；审批事件 | claude 双向卡；策略驱动回执；zcode 档位映射（默认收紧）；watchdog 兜底 spawn 型 |
| H6 回执/审计/门控 | 全部无头动作 | 归一回执结构；action=headless 审计；flag 面探针 + 漂移提示 |
| H7 zcode 无头发消息 | session_id + 文本 + 花名；在册校验；平台分叉命令（Mac 补 env var） | zcode.cjs spawn 一次 turn（`--mode yolo`，跳过 stdout 前缀）；回执（末条 assistant/token/耗时）；可见性提示 = 重启级/未信任仅 MAM；工作区争用锁探活；审计 |
| H8 codex APP 发消息 | session_id + 文本；thread id = rollout UUID（禁会话名） | **按 APP 开/关分派**：queue（入队回执 + 消费确认自建 + thread 打开前置校验）/ exec resume（`-C` 前置）；APP 原生排队可见（忙态回合后投递） |
| H9 WorkBuddy 发消息 | session_id + 文本；心跳 endpoint 发现 | **路线 A（定案）**：ACP 免鉴权全链（connect→initialize→load/new→prompt）；SSE 事件归一回执；APP 内实时可见；已结束会话语义 C2 定 |
| H10 zcode 无头新建 | 项目路径（候选/手填）+ 首句 | 无头起会话 + 首句注入 + sess_id 回执 + 上板 + 可见性提示；黄字多实例提示 |
| H11 三家 CLI 无头（已裁并入） | session_id + 文本 | claude/kimi/opencode 一次 turn + 回执（W7 口径） |
| H12 WB 会话发现双源 | 心跳（旧版）∪ workbuddy.db sessions 表（mtime 轮询 + 副本查询） | WB 5.7.3+ 交互会话上板；三色状态按 status 列映射；双源并集去重 |
| H13 dsh 无头发消息 | session_id + 文本；projcache id 映射 | ACP stdio spawn（一次一进程）→ session/prompt 投递 → 事件流归一回执；queue 默认/steer 登记；可见性如实提示 |
| M10-a 交接双档 | 会话卡「交接」+ 两级单选 | HANDOFF v1 + 收割转移 + 索引；注入自总结/规则摘要自动降档（含无头通道） |
| M10-b 交接配置 | 设置页交接区 | .json 附本开关（默认开）/ 自总结超时（默认 10min）/ 保留期档位 + 清理 / 打开目录 |
| M10-c 应急预案 | — | 文档级登记（D7） |

## 10. 非功能需求

- **安全**：PIN + gate + Host 双条件豁免复用（M5 在产）；无头命令固定形态无参数拼接（宪法红线）；敏感目录黑名单对 H10 手填路径生效；审计含开关状态。
- **可靠性**：无头 turn 永不静默卡死（watchdog 必在）；进程树级回收无孤儿；MAM 重启无残留自检。
- **性能**：无头 spawn 不占主线程（spawn_blocking 同款纪律）；版本探针结果缓存；全局并发上限防打爆。
- **诚实性**：可见性预期如实（判定 F 文案不谎报实时）；未投递/超时/取消如实回执；探不通的工具如实标注。
- **测试**：纯核单测（路由/回执归一/串行/门控）+ MSW 前端用例 + `#[ignore]` 实机 E2E（m9r 模式）；测试零网络、零真实用户数据目录。

## 11. 证据台账

| 项 | 等级 | 依据 / 验证点 |
|---|---|---|
| dsh 桌面端同源与读侧盲区（H1） | A | 本机实测（v0.2.0-rc.2 运行中：host cmdline 指向 `~/.dsh`、sessions 当天在写、双令牌不匹配不上板）；令牌扩展 = C-P+ 回归测试交付 |
| dsh 写通道（H2） | **A** | PL 定案（2026-10-03）：源码级 `session/prompt`（queue/steer+冷会话自动 resume）+ 19387 全 401 只读实测；19387 归属与版本漂移登记复核 |
| OpenClaw 写通道（H2） | C | 本机未安装不可实测；官方文档级「有条件存在」（Gateway HTTP 默认关 + operator token + session key 路由） |
| zcode 无头通道（H7/H10） | **A** | 两端实测闭环：resume 全链（8s/10s）、回执 JSON 全集、并发不拒绝、斜杠字面化、**收录规则两端定案**（未信任永不收录 / 已信任重启收录 / surface 无影响）、工作区争用锁（Mac 排除法定案）、Mac 调用形态分叉 |
| codex queue（H8） | **A**（两端实测） | Mac 三态主证（未打开滞留/打开空闲 ~20s/忙态回合后投，FIFO 补投）+ Win 抽验（~55s 消费）；回执 = message-id；`codex exec` 会话不进索引不能用 queue；daemon 消费前提定案 |
| codex exec resume 兜底（H8） | A | issue #28259 实证 + Mac 双态实测（APP 关可用 23s / 开被单写者锁拒 -32600，`-C` 前置） |
| codebuddy CLI/serve（H9） | B+ | 官方文档 + 2.161.1 隔离安装 flag 面实测（`-p/-r/--serve/--output-format json`，内嵌 2.137.1）；**ACP 通道 Mac 实证打通（路线 A 主案）；Win 端点启用条件 = 风险 16 跟进** |
| WorkBuddy ACP 通道（H9/H12） | **A-**（Mac） | `acp/connect` 免鉴权 → `session/new`+`session/prompt` 写入实证（转写 44KB/function_call）；已结束会话语义边界；Win 侧待端点启用（5.7.3 无 per-session serve） |
| claude/kimi/opencode 无头（H11） | B/C | 官方 flag 面（旧 spec W7 台账沿用）；并入否 = 裁决门 |
| 交接导出（M10-a） | B | W9.1 决策表 12 项调研定档（四家 compact 一手核对）；组合模板 = C3 实机验证 |
| AionUi/AionCore 参考实现（H4–H6/H8/C4） | B（强参照） | 双纪要：`research/refs/phase2-消息注入/2026-10-04-aionui-study.md`（web 层）+ `2026-10-04-aioncore-source-study.md`（源码级，文件:行号）——claude wire 实机档案 / agy per-turn 先例 / codex 重放面 / Job Object 进程治理（附录 E） |
| macOS 三通道（C5） | A-（代码） | 引擎 macOS 执行层在产；实机终验 = Mac 段 ② |

## 12. 风险与已知限制（预登记）

| # | 风险 / 已知限制 | 处置 |
|---|---|---|
| 1 | WorkBuddy 端点无写 API 或 token 不可寻 | **已消解**：ACP `connect` 免鉴权打通（Mac 实证）——token 前提不成立；残余风险 = Win 端点不在场（转风险 16） |
| 2 | codex queue 触达桌面会话无官方保证 | **已实证**：Mac ~20s / Win ~55s 消费（两端）；「未打开 thread 永久滞留」为已知语义 → H8 前置校验 + 自动改道 |
| 3 | zcode `--mode` 默认 yolo 风险 | yolo 已裁为默认（裁决 14）；风险面 = H3 总开关默认关 + 知情开启提示兜底；plan→yolo 粘滞疑云入版本门控复核 |
| 4 | codebuddy 内嵌与独立 CLI 版本偏差 | **降级为备选风险**（路线 B 已非主案）；实测口径：Win 内嵌 2.137.1 / 独立 2.161.1；版本门控沿用 |
| 5 | codex daemon 前置条件（APP 关闭场景） | **已实证消解**：daemon 由 `codex agents` 自动安装托管；消费前提是「thread 被 APP 打开」而非 daemon 在场（Mac 三态定案） |
| 6 | 无头与 APP 同会话/同工作区写冲突 | **两端定案（粒度分设守卫）**：zcode 无头不拒绝并发（串行 MAM 自建）+ **工作区级瞬态争用锁**（APP 活跃工作区 → 1s 失败，空闲恢复 → 探活重试，H7）；codex = **会话级单写者锁**（APP 开 → exec resume 拒 -32600，H8 按 APP 开关分派）；WB ACP = 已结束会话静默挂（风险 15） |
| 7 | macOS 行为差异（路径/刷新/端点拓扑） | Mac 段对齐探测；差异如实登记 |
| 8 | codex queue / codebuddy serve 标 experimental/Beta | 版本门控 + 分阶段失败回执 + issue 跟踪清单（#28259/#47193/#25914/#33556） |
| 9 | kill 进程树对半 turn 的落盘损伤 | 探测各工具 kill 行为；取消回执提示「可能未完整落盘」 |
| 10 | zcode 无头新建会话 APP 不显示 | **两端定案（附录 D Mac 块）**：未信任工作区永不收录 / 已信任唯重启收录 / surface 无影响——H10 可见性提示按此分层，降级路径不再候补 |
| 11 | dsh rc 阶段迭代快（桌面端令牌/端口/数据格式漂移） | 两强特征 OR 匹配（`dsh-desktop-host` 子串 / `profiles\desktop` 后缀）；**19387 归属与「版本漂移嫌疑」已双结案**（两端同版 0.2.0-rc.2、同返 401）；后续漂移沿用版本探针 |
| 12 | 同 cwd 多实例注入靶向歧义（写侧无保护，消息可乱窜；读侧上板正确——Mac 实测暴露，平台无关缺口 L13） | TTY 精确匹配修法已端到端验证（tmux pane_tty / Terminal tab tty / iTerm session tty 三家 1:1）；取不到 TTY 回退 cwd 且**多候选拒绝注入并报错**；Windows 等价键待定 |
| 13 | macOS claude「立即发送插队」假成功（确认面不可达仍报 ok、消息滞留 composer；codex 同平台为诚实失败范式，L14） | 确认不可达时报中性 `submitted` 不报 `delivered`（比照 codex `engine.rs:456` 前置门禁范式）；长期以「会话文件新增该消息」替代屏读判据 |
| 14 | codex CLI↔APP 版本劈叉（PATH CLI 0.160.0 vs 旧 APP 内嵌 0.155-alpha；历史异常现象的成因） | 版本门控**双坐标**记录：CLI 版本与 APP 内嵌 codex 版本分开探测（APP 改名 `ChatGPT.app`/`com.openai.codex`，按 bundle id 识别） |
| 15 | WB ACP 对已结束会话 prompt 静默挂（200 + 心跳但回合永不推进——接口语义，非鉴权问题） | H9 语义二分：仅活跃/新建会话投递；已结束会话的复活语义（load 后重启回合）C2 实现时定 |
| 16 | 上游读侧/端点协议漂移（本批已发生三例：dsh 代际 v4；WB 5.7.3 心跳废弃；**WB Windows 无 per-session serve 端点**——3 个监听端口实测均非 CodeBuddy 形态，Mac 可用端点实为「Remote Control」功能服务，Windows 5.7.3 疑未启用） | 读侧版本门 + 双源降级设计（H1/H12）；**WB 端点启用条件（APP 内远程控制开关，USER-ASSIST 30s）= C2 前置复验的第一跟进项**；若 Windows 无此功能 → 路线 A 降级评估（B/C 兜底） |

## 13. 宪法与在产代码对照

**宪法**：本批 = 二期 spec W7（M11）提前扩容 + W9（M10）落地 + W13 边界条款关账；F2.3/F2.4/F2.5/F2.10/D6/D18 全部沿用，无既有条款冲突。两处需宪法动作（均由用户在场裁决，AI 不改 `docs/MASTER-PLAN.md`）：
1. **dsh 写通道 D14 出评**：**2026-10-04 用户裁决出评通过（裁决 16）**——源码级可写 + ACP stdio 集成；MASTER-PLAN D14 修订随 C2 落地提请用户；
2. **H10 zcode 无头新建属宪法未列新能力**：沿用「远程新建会话」先例（用户裁决提前 + F2.x 新条目正文修订随编码批次执行），本批落地时同样办理。
三期项（F3.1/F3.9）维持，宪法零改动（裁决 5）。

**在产代码对照**（衔接点与变更点，非冲突）：

| 在产模块 | 现状 | 本批动作 |
|---|---|---|
| `inject/routing.rs` | APP 形态一律判不可注入（含测试断言） | C1 改为按工具分派新通道；既有 app() 不可注入测试随批翻转语义 |
| `monitor/dsh/mod.rs::cmdline_is_dsh_host`（单源判定门，进程发现与宿主存活共用） | 只认 node cmdline `dsh`+`web` | H1 扩桌面特征 OR 匹配；网页版夹具保留 + 桌面端夹具新增 |
| `window/`（聚焦链） | macOS applescript/iterm/terminal/tmux/dsh_tab + Windows win32 | H1 跳转聚焦桌面窗口复用该链；平台缺口降级「仅上板」如实标注 |
| `inject/queue.rs`（终端队列） | 黄排队/可输入态 flush 语义 | 不动；无头按会话串行为独立层，路由保证同会话单通道族 |
| `database`（写审计/枚举） | action 枚举在产 | 扩 `headless`/`headless_cancel`（migration 随批） |
| 各 adapter（zcode/codex/dsh） | 会话发现与读链在产 | zcode/dsh 读链零改动复用（dsh 仅 H1 代际放行 + 宿主判定扩）；**WB 例外：发现层改 db 双源（H12）**；codex 仅加 thread id 映射 |

## 附录 A · 工具 × 通道矩阵（目标态；「定案」栏探测后回填）

| 工具 | 终端注入（在产） | 无头/APP 通道（本批） | 有头可见性 | 定案状态 |
|---|---|---|---|---|
| Claude Code | ✅ | H11 `-p --resume`（**已裁并入**） | 注入实时 / 无头读链路可见 | ✅ 已裁（C4） |
| Codex CLI（TUI 托管） | ✅ | —（不路由无头） | 实时 | ✅ 在产 |
| Codex APP 托管 | — | H8 queue / exec resume 兜底 | APP 原生排队（忙态回合后投递） | ✅ Mac 主证：queue 三态 / 单写者锁 / UUID 唯一 / `-C` 位置（附录 D Mac 块）；Win 抽验可选 |
| OpenCode | ✅ | H11 `run`（**已裁并入**） | 官方 web 实时 | ✅ 已裁（C4） |
| OpenClaw | — | H2 轻探测 | — | ✅ PL 定案：写通道有条件存在（默认关，文档级） |
| Kimi Code | ✅ | H11 `-p -S`（**已裁并入**） | 刷新后 | ✅ 已裁（C4） |
| WorkBuddy | — | H9 路线 A（ACP） | ACP 写入 APP 内可见 | ✅ ACP 免鉴权打通（Mac 主证）；**Win 端点启用条件 = C2 前置跟进**（风险 16） |
| ZCode | — | H7 在册注入 / H10 无头新建 | **重启级**（已信任工作区；未信任 = 仅 MAM 可见） | ✅ 两端定案：收录规则闭环（未信任不收录〔两端互证〕/ 已信任重启收录〔Mac+用户实证〕/ surface 无影响） |
| dsh | — | 写侧 **ACP stdio**（C2 交付，裁决 11/16） | 自带 UI（读侧上板由 H1 保障） | 读侧 ✅ H1（宿主判定已交付 `4c7b004`/`08ef620`，v4 代际 C0）；写侧 ✅ 源码级定案 |

## 附录 B · 功能点进度表（随批次更新）

| 功能点 | 里程碑 | 状态 |
|---|---|---|
| H1 dsh 桌面端读侧接入 | C-P+ | ✅ 代码交付（`4c7b004`/`08ef620`）+ GUI 核验通过（上板 + APP 级跳转）；v4 代际放行转 C0 |
| H2 OpenClaw/dsh 写通道探测 | C-P+ | ✅ 两端定案（D14 已出评，裁决 16） |
| **C0 · L13 靶向 TTY + L14 诚实化 + dsh v4 + WB 双源（裁决 17+18）** | C0 | ⬜ |
| H3 无头总开关 / H4 生命周期（600s）/ H5 审批档（yolo）/ H6 横切 | C1 | ⬜ |
| H7 zcode 注入 / H8 codex APP 注入 | C1 | ⬜ |
| H9 WorkBuddy 路线 A（端点启用前置，风险 16）/ H13 dsh 写侧（ACP stdio） | C2 | ⬜ |
| H10 zcode 无头新建 / ~~M10-a 双档 / M10-b 配置 / M10-c 文档（移出本期，裁决 19）~~ | C3 | ⬜ / 后续计划 |
| H11 三家 CLI 无头（已裁并入） | C4 | ⬜ |
| H12 WB 发现双源（v4 同批：H1 扩容） | C0 | ⬜ |
| H13 dsh 无头发消息（ACP stdio） | C2 | ⬜ |
| M7 Mac 终验（L14 项已 C0 化）/ README / 发版（移出本期，裁决 19） | C5 | 后续计划 |

## 附录 C · 与旧 spec W 编号映射

| 旧 spec（2026-09-18） | 本 spec | 备注 |
|---|---|---|
| W7 无头通道五家（M11） | H7/H8/H9/H11 + H3–H6 底座 + H10 zcode 新建 + H12 WB 读侧 / H13 dsh 写侧（整理补号） | 提前 + 扩容 APP 形态；空闲挂起/重生被裁决 8 排除 |
| W6 审批应答（无头半部 W6'） | H5 | 三机制承诺沿用（spawn 型）；codex queue 通道例外如实标注 |
| W8 Windows 终端注入 | —（在产不动） | M6R–M9R 已交付 |
| W9 交接导出（M10）+ W9.1 决策表 | M10-a/b | 全量定案沿用；新增配置功能点 M10-b |
| W10 keystroke 应急预案 | M10-c | 文档级不变 |
| W13 一键 resume（B 兜底发半部） | §7 关账条款 | H7–H9 落地即补全 |
| 远程新建会话 spec（zcode 排除条款） | H10 | APP 形态新增量；宪法裁决先例见 §13 |
| W11 推送网关 / W12 APK | —（三期收尾） | 裁决 16 不变 |

## 附录 D · Phase P+ 探测定案与裁决门议程（2026-10-03 Windows 段，随探测批回填）

> **防误读注记**：本节为 2026-10-03 Windows 段当时的快照（其中「待夹具/待用户/待裁决」均已有结果）——最终结论以 **D-2（Mac 段并入 + 补测收口）与 §2 裁决 12–19** 为准，保留作时间线留痕。
> 证据：`~/mam-probe-closure/20261003-022408/`（本地，不入库）；报告：`research/refs/phase2-消息注入/2026-10-03-app-injection-probe-report.md`（本地）。H1 交付：`4c7b004`（宿主判定扩桌面特征）/ `08ef620`（跳转聚焦，web 路径回落）。版本漂移：zcode 0.16.9（无）/ codex 0.160.0（+3.9）/ WB 2.137.1↔独立 2.161.1 / dsh 0.2.0.0。

**定案摘要**（细节见报告与 pz/pc/pw/pl notes）：
- **zcode**：无头新建零阻断（H10 可行）、resume 全链 8s、回执 JSON 全集可映射 H6、`--prompt` **默认 yolo 必须显式覆盖**、并发不拒绝（串行 MAM 自建）、斜杠不等效（字面进模型）、**无头会话不自动入 APP 任务列表**（索引存活下 35min 0 收录实测）；surface 无差异。
- **codex**：thread id 取 rollout 文件名 UUIDv7；`session_index.jsonl` 滞后 13 天实证不作 id 源；queue/exec-resume 实测待 PC 夹具。
- **WorkBuddy**：每会话异端口拓扑；settings.json 无 token；独立 CLI 2.161.1 无头面全绿（路线 B 命令面成立）；端点/投递/映射待 PW 夹具。
- **OpenClaw**：写通道有条件存在（Gateway HTTP 默认关 + operator token；本机未装不可实测）。
- **dsh**：写通道源码级存在（`POST /api/session/prompt`，queue/steer + 冷会话自动 resume；ACP stdio 完整）；19387 全 401 实测、归属未定案（版本漂移嫌疑）；集成建议 ACP stdio；POST 验证不可构造（无合法凭据，不做逆向）。

**裁决门四事（待用户裁决）**：
1. WorkBuddy 路线 A/B/C 定一条（待 PW-2/3 收口；初判：openapi+鉴权可得 → A，否则 B 实测，均不通 → C）；
2. H11 三家 CLI 无头并入实现批与否；
3. 验收线与超时定值（zcode 判定 F 增补降级预案——APP 列表 0 收录实测；watchdog 建议自 600s 起议，定值证据不足）；
4. dsh 写通道结论是否启动 D14 出评（源码级「是」，流程见 §13）。

**USER-ASSIST 待办**（补测后回填本附录与报告）：MAM 桌面 dsh 卡/跳转核验（02:47:59 重启旁证已录）；ZCode APP 三态可见性 + APP 内并发 + 档位佐证；Codex APP `pc-proj` 夹具会话；WorkBuddy `pw-proj` 测试会话。既有失败登记：`tests/preset_v2_test.rs` 5 失败（stash 复现＝先于本批，疑环境依赖）。

### 附录 D-2 · Mac 段回传并入（2026-10-04，证据面齐备）

> 报告：`research/refs/phase2-消息注入/2026-10-04-app-injection-probe-report-mac.md`（归档，run-id 20261004-155820）+ **`2026-10-04-app-injection-probe-cross-platform-synthesis.md`（两端综合，本节索引）**。Mac 段任务书全项完成（多项超额），报告含 4 处就地更正（以末轮结论为准）。

**Mac 段定案增量**（细节以综合报告为准）：
- **zcode**：**收录规则闭环**——未信任工作区三态全不收录（与 Win 35min 互证）；已信任工作区**唯重启收录**（76→84 行实测 + 用户实证）；surface 无影响；成因 = CLI/APP 双库隔离。**Mac 调用形态必须补 `ZCODE_BUILTIN_PROVIDER_CONFIG_FILE`**（打包布局 bug，反编译定案——H7 命令构造按平台分叉）；stdout 有非 JSON 前缀行（解析须跳过）；**APP×无头并发 = 工作区级瞬态争用锁**（APP 活跃 → `Model creation failed` 1s；空闲 → 可用；争用型非排他型）。
- **codex**：queue 三态主证（未打开永久滞留 / 打开+空闲 ~20s 消费 / 打开+忙态回合后投递；FIFO 补投；`codex exec` 会话不进索引不能用 queue）；exec resume 双态（APP 关可用 / 开被单写者锁拒 -32600）；**只用 UUID 禁会话名**（202-thread 重名普查）；`-C` 全局 flag 置子命令前；「0.160.0 疑云」= CLI↔APP 版本劈叉（APP 已改名 `ChatGPT.app`）；**插队原语 = `turn/steer`**（协议面，CLI 无 flag，本批不做）；审批 = `turn/start` 的 `approvalPolicy`（协议级，三期 F3.1 素材）。
- **WorkBuddy**：**路线 A 打通**——ACP 免鉴权全链（`acp/connect` → `initialize` → `session/new|load` → `session/prompt`），写入实证成功；**prompt 仅对新建/活跃会话生效**（已结束会话静默挂）；ACP 会话不进 workbuddy.db（仅落 `projects/` 转写——读链路影响）；心跳按需生成（判读坑更正）；Permission Mode 协议级四档。
- **dsh**：19387 两端同 401、同版 0.2.0-rc.2 → **归属与漂移双结案**；桌面 cmdline 双特征 Mac 命中（H1 判定门跨平台成立）。
- **M7 终验**：三通道文本注入 ✅、黄排队 ✅、锁屏 ✅、**claude 插队假成功 ❌**（L14）；codex jump = 诚实失败；**三个通用产品缺口**：L13 同 cwd 靶向乱窜（TTY 修法已验证）、L14（诚实化修法现成）、L15 Terminal.app 按键需辅助功能授权；m9r E2E 套件 Windows-gated（L3 跨平台套件缺口）。

**裁决门议程（五事，**✅ 2026-10-04 全部已裁，裁决 12–17 入 §2**）**：
1. WorkBuddy 路线：**✅ A 定案**（ACP 免鉴权；Win ACP 复验挂 C2 开工前置）；
2. H11 三家 CLI 无头并入否：**✅ 并入**（C4 转正式）；
3. 验收线与超时：**✅ zcode = 重启级 + 未信任降级（两端定案采纳）；`--mode` 单一 yolo；watchdog 600s**；
4. dsh 写通道 D14 出评：**✅ 出评通过 + 追加 C2（ACP stdio）**；
5. L13/L14 缺口排期：**✅ 并入新增 C0 前置小批**。

**Windows 补测清单（收窄）**：必做 = ① MAM dsh 卡+跳转 GUI 核验、② WB Windows ACP 复验；抽验可选 = zcode 重启态/stdout 前缀/APP 并发、Codex APP queue 抽验（综合报告 §六）。

**补测执行结果（2026-10-04 晚，Windows 段收口 → 落档）**：
- ✅ **dsh 卡核验**：上板 ✓ + APP 级跳转 ✓（会话级无深链，预期内）；暴露两缺陷 → 根因定位 → **裁决 18 入 C0**（v4 代际 + WB 心跳废弃连带发现）；dsh 数据落点未漂移（当晚新会话仍写 `~/.dsh/sessions/`，仅代际升 v4）。
- ✅ **codex queue Windows 抽验**：入队 → **~55s 消费落盘**（Mac 主证 20s，Windows 稍慢但成立）；thread 已索引、`queue_1.sqlite` 消费即清；probe-win-1 消息与回复在 APP 内可见（用户可清理）。
- ✅ **zcode 抽验**：Windows **无 stdout 前缀污染**（Mac 特有差异，防御式解析仍保留）；**工作区争用锁与 Mac 结论一致**（APP 开着但工作区空闲 → 无头正常运行）；重启收录抽验未执行（需重启用户 APP，可选项不阻塞）。
- ⏸ **WB ACP 复验：端点不在场（如实登记）**——WB 5.7.3（Windows，比 Mac 5.4.7 新）无 per-session 服务进程（3 个监听端口实测均非 CodeBuddy serve 形态）、交互会话无心跳（连 prewarm 心跳文件也会话结束即清理）；Mac 可用端点实为「Remote Control」功能服务 → **启用条件（APP 内远程控制开关，USER-ASSIST 30s）= C2 前置复验第一跟进项**（风险 16）；Test2 会话在 workbuddy.db 中（H12 素材已取）。

**落档判定：✅ 设计阶段关账**——探测两端完成 + 裁决门六裁 + 裁决 18 + 补测收口（WB 端点启用条件为 C2 前置活账，不阻塞落档）；本 spec 定稿，下一步 = C0 实施计划（writing-plans 另立文档）。

## 附录 E · 参考实现借鉴登记（AionUi / AionCore，2026-10-04 双调研）

> 纪要：`research/refs/phase2-消息注入/2026-10-04-aionui-study.md`（web 层全景 + 借鉴映射表）＋ `2026-10-04-aioncore-source-study.md`（源码级精读，结论全部带 文件:行号；本地克隆 `E:\LLMproject\Github\AionUi\AionCore` @ `4a707fc`）。
> 定位：**强参照（B 级）**——实现批的规格素材库，不改变本 spec 任何裁决；与宪法无冲突（AionCore 权威源 = wire 回执，用于 MAM 无头管线；MAM 屏读核对面为宪法独有，互不替代）。

**实现批直接采用的素材**（按 H 节索引）：

| # | 素材 | 出处 | 落点 |
|---|---|---|---|
| ① | **claude 无头 spawn argv 全集**：`--print --input-format/output-format stream-json --verbose --include-partial-messages --replay-user-messages --permission-prompt-tool stdio` + **恒带 `--permission-mode`（fail-closed：省略 = bypassPermissions，LIVE-PROBED）**；`--resume` 与 `--session-id` 互斥；fork = `--resume --fork-session`（不可配 id） | claude_conn.rs:154-269 / adapter/claude.rs:1073-1126 | H11/C4 规格 |
| ② | **claude 审批应答构造器**：allow 必带 `updatedInput`（原 input 原样回显，缺 = ZodError → 工具永不执行）；AskUserQuestion 答案按**题面文本**为键注入 `updatedInput.answers`（多选 = JSON 数组）；**弃卡必须 deny**（allow 会静默丢题）；2.1.178–2.1.227 实机标定 | claude_conn.rs:1474-1580 `build_control_response` | H5 审批卡 wire 规格 |
| ③ | **问答卡纪律**：多题一次提交、全答才可提交（claude 静默丢未答题）、Other 自由文本行、decline = 显式 deny | MessageQuestion.tsx + ② 同源 | 移动端问答卡（既有卡面纪律互证） |
| ④ | **codex 首启/续接全量重放**：`thread/resume` params = `thread/start` params 全量 + threadId（裸 resume 丢 MCP、approvalPolicy 重置回 on-request，0.144.1 实测）；固定 `-c shell_environment_policy.inherit=all -c shell_environment_policy.include_only=[]` | codex_conn.rs:593-603 / 47-60 | H8（app-server 路线的后续升级）与 queue/exec 的 env 注入 |
| ⑤ | **agy per-turn 先例**：`agy -p` 一次性进程 + `--conversation <id>` 续接 + `--print-timeout 1h` 退出期墙钟（CLI 自带墙钟与 watchdog 的归一关系）+ 宿主层审批闸 | backend/antigravity/argv.rs:81-135 | **裁决 8 一次性进程架构的同构生产先例**（H4/H11 对照） |
| ⑥ | **Windows 进程治理**：CREATE_SUSPENDED + **Job Object（KILL_ON_JOB_CLOSE）** 整树收编 + TerminateJobObject 热终止（优于事后 taskkill /T：孙子进程不可达问题 I-9）；孤儿回收四闸（锁/machine/epoch/liveness）+ 杀前身份再验 | process_registry / job object 模块（纪要 §6） | H4 生命周期（kill 序列升级候选） |
| ⑦ | **版本门控探测法**：`claude --version` 每二进制一次探测 + 缓存 + verified floor（`--fork-session` 门槛实测 2.1.191）；cli_version 漂移监控（claude 2.1.280 / codex 0.151.0 跳 0.147.0） | claude_flags.rs 等（纪要 §7） | H6 版本门控探针 |
| ⑧ | **事件归一纪律**：SessionEvent 开放枚举（45 变体）+ `AdapterSpecific{tag,payload}` 逃逸舱「未知帧不猜」+ 全量快照广播（非增量） | reducer.rs / orchestrator.rs（纪要 §5） | H6 回执归一（与「快照对位失败→放弃同步」同源互证） |
| ⑨ | **ACP 通用层规格**：initialize 能力协商、`session/new·load·prompt` 全参数、`session/request_permission`（options 真回显）、`set_config_option`/config_options 读取（mode/model 档位元数据） | acp_conn.rs（纪要 §4） | H9 WB/dsh ACP 接入规格 |
| ⑩ | **OnceCell 单 spawn / 进程注册表持久化 / 崩溃恒归 Idle** | task_manager.rs / process_registry.rs | H4 防重复初始化与孤儿自检 |

**边界**：AionCore 的常驻+空闲休眠模型**不采纳**（裁决 8 维持——其 wake=respawn+resume 与 per-turn 同构，无引入必要）；其 ACP 仅 stdio（MAM 的 WB HTTP ACP 为自研探测成果，无对应物可抄）。
