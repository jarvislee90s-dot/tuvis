# ZCode 官方桥接接入合同（研究决策文件）

> 本文档是工作包 B 的固定产物（见
> `docs/superpowers/plans/2026-10-24-zcode-official-bridge-validation.md`「固定产物与接口」）。
> 当前仅含 **B0 部分**：头部身份与安装指纹、证据索引、官方模块与许可证清单、测试目标等待项。
> auth／route／operations 的实际身份与 wire 值**不在本文档预设**——它们由 B1—B4 以真实取证逐一追加，
> 未取得即 `capability=false` 并写明原因，绝不以 TBD 或伪造 endpoint 占位。

---

## 1. 头部（B0）

### 1.1 run_id 与平台

- **run_id（本轮 B0 取证）**：`macos-B20261024T142609+0800-k0zat1`
  （命名规则沿用探测手册 §4.2：平台＋含时区时间＋随机短码）。
- **platform**：macOS（ProductVersion 27.0.1，BuildVersion 26A434，arch arm64；本轮 `sw_vers`/`uname -m` 直读）。
- **私有证据目录（预留）**：`~/Documents/tuvis-verify-logs/zcode/macos-B20261024T142609+0800-k0zat1/`
  （内含 `raw/`、`sanitized/`、`report.md`；追加轮次不覆盖旧记录；B0 为仓库文档内只读取证，未创建该目录、未落任何 raw 文件）。
- **执行方式声明**：本轮全部为本机文件系统**只读静态检查**与 GitHub API 对固定提交的元数据核对；
  **未启动 ZCode APP／CLI、未运行 Tuvis、未执行任何构建或测试套件、未连接任何官方远端服务**
  （并行代理持有产品构建目录与提交权，见工作包 B 约束）。

### 1.2 本机安装指纹（本轮直取 + 沿用记录）

本轮**不启动任何进程**，因此运行版本沿用既有 PACKAGE 证据并显式标注；静态事实为本轮新测，不覆盖旧记录：

| 项 | 值 | 证据级别 | 来源与本轮状态 |
| --- | --- | --- | --- |
| ZCode.app 版本／build | CFBundleShortVersionString `3.16.2`，CFBundleVersion `3.16.2.8769` | PACKAGE（本轮直读） | `/Applications/ZCode.app/Contents/Info.plist`（PlistBuddy 只读）；与 Mac P0 记录（`macos-20261023T2246+0800-acdb`，APP 3.16.2.8769）**一致，未升级** |
| 随包 CLI 入口文件 | `/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs`，15,243,891 B，mtime 2026-10-19 20:37 | PACKAGE（本轮直读） | 同目录 `.node-bundle-meta.json`：`entry=zcode.cjs`、`runtime=electron-node`、`platform=darwin-arm64`、`source=apps/zcode-cli/packages/cli/dist/zcode.cjs`——随包文件自述其源码路径位于本固定 SHA 仓库内 |
| 随包 CLI SHA256（本轮新增） | `a055234a6ca3da0172616d3987cf3c617b6a26deb1332d69035300c2a0ae0de1` | PACKAGE（本轮直取） | `shasum -a 256`；旧探测文档未记录该哈希，属**追加**新指纹，不改写旧证据 |
| 随包 CLI 静态版本串 | `0.16.9` | PACKAGE（本轮复测命中） | 对 zcode.cjs 的静态 grep 命中；与 P0 复核「随包 CLI 静态串 0.16.9」一致，文件未变 |
| 随包 CLI 运行版本 | **未运行本轮** | UNPROVEN（Mac 侧） | 沿用 `2026-10-24-capability-status.md` §4.1 口径：Windows 运行 help 为 `0.17.0`、DB 迁移版本 `0.16.9`、静态包字符串 `0.16.9`、APP `3.16.3.8992` **四者不可互推**；Mac 的 `0.16.9` 目前只是静态指纹，不能推断 Mac 运行版本 |
| 独立 PATH CLI | 无（`zcode` 不在 PATH） | PACKAGE（本轮直查） | `which zcode` 未命中 |
| Tuvis.app 已安装版本 | `0.6.1`（CFBundleShortVersionString/CFBundleVersion） | PACKAGE（本轮直读） | `/Applications/Tuvis.app/Contents/Info.plist`；与 P0 记录（Tuvis 0.6.1）一致 |
| Tuvis 源码身份 | 分支 `codex/zcode-unified-remote`，HEAD `131346083f0f1112d4b53843d0905d72826ed430`，工作树 `/Users/jarvis/Documents/tuvis-worktree2` | SOURCE（本轮直查） | `git rev-parse HEAD`（只读）；A 包由并行代理续提交，本文件只钉住 B0 取证时点 |
| Windows 安装身份（沿用） | APP `3.16.3.8992`、随包静态串 `0.16.9`、run_id `windows-20261023T2247+0800-1rn60k`、P0-R+E1—E4 包 `zcode-windows-P0R-E1-E4-20261024` | PACKAGE/RELAY | 沿用 P0 复核 §1 与 capability-status §1／§4.1；**本轮未在 Windows 机器重测** |

历史安装指纹（Mac P0：3.16.2.8769／静态串 0.16.9；Windows P0：3.16.3.8992）保持原文不变；
若未来版本升级，只**追加**新 fingerprint 行，不改写本节旧记录。

### 1.3 固定官方源码 SHA 及钉固方式

- **官方源码基线**：`zai-org/ZCode` @ `3c5b227649371507ddc4eb46ac5133afe6db1cab`（提交日期 2026-10-20T06:23:05Z）。
- **钉固方式**：
  1. 由探测手册 §2（`docs/superpowers/plans/2026-10-23-zcode-probe/README.md`）以树链接首次钉固，
     P0 复核 §2.2 与 Mac 首轮／P0-R 提示词全部引用同一 SHA；
  2. 本轮经 GitHub API `GET /repos/zai-org/ZCode/commits/3c5b2276…` 复核该提交存在，
     committer 为「ZCode Source Sync Bot」，commit message 注明外部同步源
     `Source-Commit: 5a082039a1a3b1edfe8364a33c8978689c0d68bf`（即公开仓库为同步镜像，
     固定 SHA 指向同步提交本身；B1 引用官方实现时一律以本 SHA 的 blob 为准）；
  3. 安装版对应性判定沿用 P0 复核结论：**PARTIAL**——`glm/.node-bundle-meta.json` 的
     `source` 字段把随包 zcode.cjs 自述为本仓库 `apps/zcode-cli/packages/cli/dist/zcode.cjs`，
     且 README 标注 2026-10-19 更新到 v3.16.2、本机 APP 即 3.16.2.8769；
     但「同版本号不等于同构建」，无字节级构建对应证据，不升级为 PROVEN。
- **升级规则**（B0 简报第 1 条）：若本机安装版升级或官方发新提交，只记录新 fingerprint／新 SHA 行，
  本节的 3c5b2276 基线及其历史结论**不改写**。

---

## 2. 证据索引（claim → 支撑材料）

本轮**不重跑 P0 探测**；下表把决策文档后续各节将依赖的每一类论断钉到既有条目上。

| # | 论断（桥接依赖） | 支撑材料与路径 | 证据级别 |
| --- | --- | --- | --- |
| E1 | 旧 v1 设计「协议未支持审批／问答」的结论**已被同提交实现反证**，不得再引用 v1 措辞当作当前事实 | `docs/superpowers/plans/2026-10-23-zcode-probe/2026-10-24-p0-review.md` §2.2（撤销定论及修正表） | SOURCE（固定 SHA 行号，见 E2） |
| E2 | 固定 SHA 上的审批／问答／stop 实现入口（桥接模块边界依据） | 同 §2.2 表：`apps/zcode-cli/packages/bootstrap/src/zcode-protocol/server-operations.ts:3513`（注入 `createProtocolInteractionBroker`）、`interaction-broker.ts:105`（requestPermission 反向请求，含 requestId/sessionId/turnId/toolCallId）、`interaction-broker.ts:306`（interactionRequestUserInput）、`zcode-protocol-v4/interaction-registry.ts:1`（broker 与 v4 resolveInteraction 汇合）、`zcode-protocol-v4/commands/handlers/interaction-background.ts:45`（answer 消费；存在 full-access 特权分支，禁止探测误触）、`zcode-protocol/server.ts:677` 与 `server-operations.ts:2702`（session/stop 路由、`requireSession` 后中止 `record.activeAbortController`） | SOURCE |
| E3 | 上述方法在**本机安装版随包 CLI** 中存在（PACKAGE，不升级运行 PASS） | P0 复核 §2.2 末段静态计数：interaction/requestPermission×1、interaction/requestUserInput×1、resolveInteraction×5、session/stop×4；本轮确认 zcode.cjs 哈希（§1.2）使该计数对应当前文件。注意：哈希↔静态计数的对应关系目前依托 P0 以来该文件的 (mtime,size) 同一性支撑（15,243,891 B / 2026-10-19 20:37），下次指纹复核须以内容哈希重算再确认 | PACKAGE |
| E4 | Mac P0-R 已追出「APP host 独占本地 stdio；官方 Web 远控经账号→SID→短期 token→relay→RPC bridge 到 APP」的链路与 broker/server-operations 入口 | `mac-agent-supplement-prompt.md`（R-01 定义）、`mac-agent-prompt.md`（P0-05 定义）；回报本体在私有目录 `~/Documents/tuvis-verify-logs/zcode/macos-P0R20261024T002051+0800-5n1lm/`（留机不入库）；仓库内摘要见 `2026-10-24-capability-status.md` §3「已有 APP 回合审批／问答／打断」行与 §4.5 | RELAY（私有原件）＋SOURCE |
| E5 | 官方远控（网页面）的发送／审批／打断／切模型／链接复用有用户约两个月长期实测；本包**不重复证明**，对象是 Tuvis 独立客户端身份／连接／逐动作映射 | `2026-10-24-capability-status.md` §7；设计 v3 §1「官方页面这些功能已有用户约两个月的真实使用证据」 | 用户直接陈述（登记于 capability-status） |
| E6 | 读取断点根因（新旧 ID 双形态）与修复归属 A 包、不属桥接证据 | P0 复核 §2.1；设计 v3 §6 | OBSERVED（两平台） |
| E7 | 配置 URL 格式合同（A4，输入给 B）：`https`＋主机 `zcode.z.ai`＋`/remote/vN` 版本化路径＋非空唯一 `sid`（≤128 UTF-8 字节）、无 userinfo／非默认端口、≤4096、未知 query 保留；存储 key `remote.zcode_official_url`；`ConfigState=Missing|Saved|Invalid`，**Saved≠Connected**；完整链接不入日志／文档／夹具 | A 包计划 §A4（`docs/superpowers/plans/2026-10-24-remote-settings-and-zcode-read-recovery.md`）＋设计 v3 §4.3；实现 commit `ae3e3353 feat(zcode): persist and validate official remote configuration` | SOURCE（本仓库实现） |
| E8 | #117 最终 head 与其 provider env 处理结论（发送线历史素材的审计边界） | `2026-10-24-p0-review.md` §2.3：head `289b20db4d6bdfba50ecfdeb67f1ef6f47ab583f`，gate.rs:130–147；旧合流点 `d5cc28b1` 结论作废 | SOURCE |
| E9 | 「真实 `hi` 自动探针不进入本路线」 | `2026-10-24-capability-status.md` §4.3；工作包 B 计划全局约束「#117 hi gate 不加入本包」；设计 v3 §2 | 决定（用户裁决） |
| E10 | Windows 运行证据（E2 无头 marker、E4 取消）仅覆盖 Windows 特定构建＋provider 组合，不解锁 Mac、不解锁手机闭环 | capability-status §1／§3 矩阵各行 | OBSERVED（Windows 单机） |
| E11 | 同 SID／role 单 owner、KICKED／冲突须停自动重连的约束 | 设计 v3 §5.2；capability-status §6（mobile allowlist／设备列表／多 SID／每 SID-role 单 owner 的开源证据） | SOURCE＋待 B1 实连核验 |

---

## 3. 官方模块与许可证清单（桥接所需范围）

### 3.1 许可证

- 仓库 `zai-org/ZCode` @ 固定 SHA：根 `LICENSE` 为 **Apache License 2.0**（11,344 B，blob
  `550d8df4cfc74878663a511caa97852f15fd9592`；本轮经 GitHub API 取文核对首 200 行）。
- **无 `NOTICE` 文件**（本轮对该 SHA 查询返回 404）。Apache-2.0 复用义务相应简化为：保留 LICENSE 副本、
  对修改处加注明（state changes）、不得使用其商标背书衍生作品；无额外 NOTICE 传播义务。
- 随包 `ZCode.app/Contents/Resources/licenses/` 仅含第三方 `electron` 许可，与本清单的官方源码模块许可无关；
  `Contents/Resources/THIRD-PARTY-NOTICES.md` 为发行包第三方声明，仅在**再分发安装介质**场景相关，本项目不重新分发该包。
- 结论：**可以按 Apache-2.0 参考实现并选择性复用代码／类型**（引用而非复制整包是默认方式，见 3.3）；
  该许可不授予任何账号、token 或远端服务访问权——认证仍必须走 B1 定案的官方授权流程。

### 3.2 模块边界（相对固定 SHA `3c5b2276…`）

| 模块 | 边界与桥接角色 | 现有证据 |
| --- | --- | --- |
| `apps/zcode-cli/packages/bootstrap/src/zcode-protocol/`（协议服务端面） | `server.ts`、`server-operations.ts`、`interaction-broker.ts`、`transport.ts`（传输层实现，B1 需通读双面）、`session-residency.ts`／`session-resident-pool.ts`（实例持有）、`official-mcp-auth-port.ts`（名称指向外部 MCP 认证端口，**是否涉及远控授权未验证，留给 B1，不预设为登录通路**） | E2、E3 |
| `apps/zcode-cli/packages/bootstrap/src/zcode-protocol-v4/` | `interaction-registry.ts`、`commands/handlers/interaction-background.ts`、`v4-gateway.ts`、`replay.ts`、`cold-session-resume.ts`（v4 应答／事件／恢复面；具体命令 wire 值必须来自实现＋真实脱敏帧） | E2 |
| `apps/zcode-cli/packages/contracts/` | 跨端类型契约目录（`commands/`、`events/`、`interfaces/`、`network/` 等），B1 评估为 wire schema 的**参考源**；引用≠复制（见 3.3） | 本轮目录列表（GitHub API） |
| `apps/zcode-cli/packages/cli/`（`src/resume.ts`、`src/protocol-lifecycle.ts`）、`packages/core/src/permission/service.ts` | P0 已读发送／恢复／权限面文档；CLI prompt 通道**没有**双向审批客户端，不能据此推导协议面能力 | P0 复核 §2.2 结论、手册 §2 表 |
| 官方 Web 远控前端与云 relay 服务 | **不在本仓库**：固定 SHA 的 `apps/` 仅含 `zcode-cli` 与 `dev-docs`（本轮 API 列目录核实）。即 `zcode.z.ai` 远端与 relay 是托管服务，仓库只有其**对端实现**（协议服务端与随包 CLI）；B1 的 relay wire 证据只能来自客户端侧实现、授权流程返回与真实脱敏帧，**不得以“读官方服务端源码”补齐合同** | 本轮目录核实 |

### 3.3 复用方式（登记；逐模块裁决留 B1）

1. **默认方式——协议参照实现最小客户端**：按固定 SHA 的 `zcode-protocol`／`zcode-protocol-v4`／`contracts`
   实现语义，在 Tuvis 侧写最小桥接客户端；不整体嵌入官网、不 clone 全仓进产品树、不引入官方 workspace 依赖。
2. **可选方式——类型契约引用**：仅当 `contracts` 中某 schema 与真实脱敏帧双向核对一致时，
   按 Apache-2.0 摘录最小类型并在文件头注明来源仓库、SHA、修改说明；每个被摘录文件在决策文档登记来源行号。
3. **禁止方式**：复制官方服务端模块进 Tuvis 冒充 host、把 `ZCode.app` 内打包产物（zcode.cjs）作为运行时依赖加载、
   以 DOM 自动点击或新起私有 app-server 附着原 APP（工作包 B 架构约束、D7）。
4. 本轮**下载范围**仅限许可证核验与目录清单（GitHub API 元数据 + LICENSE 文本），未 clone 仓库、
   未取任何实现文件入本地——实现文件与测试的按需下载在 B1 定案依赖时执行并逐文件登记许可证／复用方式。

---

## 4. 测试目标状态与等待项（B0 第 4 条）

- **等待项**：用户尚未为工作包 B 指定专用测试工作区／会话；本执行上下文中**未确认任何有效官方远控链接**
  （A 包 A4/A5 已交付配置入口，但 Saved≠Connected，且链接本体按合同不进入本文档、日志或夹具）。
- **用户需要的最小动作**（按阶段，不必一次做完）：
  1. （B1 只读连之前）在 ZCode APP 内开启移动远控→复制链接，经 Tuvis 设置页 ZCode 卡粘贴保存；
     官方登录／扫码／账号授权**由用户本人完成**，Agent 不读凭据文件、不要求粘贴 JWT／SID 原文。
  2. （任何真实写之前）指定一个专用测试工作区／会话（本机别名登记到本文档 §4 追加小节，
     真实路径与 session ID 只留私有目录映射），并**逐批授权**该会话上的动作（发送／审批／问答／打断／模型切换）。
- **因等待而未执行的 B0 子项**：「测试目标别名」尚无值可登记（不猜测、不占位）；
  「哪些动作已授权」清单为空。B0 其余子项（指纹、SHA 钉固、许可证／模块清单、证据索引）不依赖测试目标，已全部完成。
- 该等待**不阻塞**后续源码／fake 测试面（B1 红测、fake relay 探针可先行），但 B1 的实连步骤与
  B2—B4 的真实写实验在用户动作完成前一律不执行。

## 5. 纪律声明（B 包约束登记）

- 旧 v1 设计文档中「交互（审批／问答）为未来工作」的措辞**已被同固定提交的实现反证**（E1／E2），
  任何后续引用把其当作当前事实都是错误。
- #117 的真实 `hi` 自动探针**不在本路线**（E9）：运行时只做只读预检，真实回合实验仅在获授权的专用测试会话内做。
- 全局约束重申：官方登录／扫码／配对归用户本人；不读其他应用凭据文件、Cookie DB、进程内存或日志取 token；
  不提交真实 SID／JWT／token／会话正文；不修改 `docs/MASTER-PLAN.md`；本包不编辑产品运行链；
  本轮及本文件生成过程中未调用任何官方远端服务（仅 GitHub API 公开仓库元数据）。
- 后续追加区（B1：identity/auth/wire 实况；B2—B4：各自 operations／fixtures）在此线以下按轮次追加，不改写上节记录。

---

# 6. B1 部分（2026-10-24 追加）：官方身份与最小认证合同（source + fake 阶段）

> 本节只做两件事：①从固定官方源码核对「官方允许的终端登录／授权流」，逐条给出行号证据；
> ②用 in-test fake relay 锁死 Tuvis 客户端侧行为（红测→绿测）。**本轮未连接任何真实远端**；
> 实连半场被 §6.7 停等项阻塞。所有凭据/标识均为合成值，不含真实 SID/JWT/token/链接。

## 6.0 执行方式声明

- GitHub 只读取文（`gh api contents` at 固定 SHA `3c5b2276…`）共 15 个实现文件 + 树清单；未 clone 仓库。
- 本机未启动 ZCode APP／CLI／Tuvis；未执行任何真实网络连接（除 GitHub API 公开端点）。
- 测试执行：`pnpm exec vitest run tests/remote/zcodeBridgeRelay.test.ts`（RED 与 GREEN 证据见 §6.5）。
- 新增文件：`tests/remote/zcodeBridgeRelay.test.ts`、`scripts/zcode-bridge-probe/relayAuth.ts`、
  `scripts/zcode-bridge-probe/auth-check.ts`（骨架，拒绝连接）、`tests/fixtures/zcode-official-bridge/contract.json`。
  探针目录不挂应用启动/扫描（工作包 B「固定产物与接口」）。

## 6.1 对 B0 §3.2 模块清单的勘误补充（追加，不改写 B0 原文）

B0 §3.2 末行「官方 Web 远控前端与云 relay 服务**不在本仓库**」的 **apps/ 目录陈述为真**
（固定 SHA 的 `apps/` 确实只有 `zcode-cli` 与 `dev-docs`），但仓库级判断不完整。本轮树清查
（`git trees?recursive=1` @ 同 SHA）证实以下三组**客户端侧实现就在本仓库**：

| 目录 | 角色 | B1 用途 |
| --- | --- | --- |
| `packages/shared/src/web-remote-relay/` | relay V2 wire 契约（zod schema：HTTP 路径表、WS 消息、token 形状、错误码） | 本轮 wire 引用的第一权威 |
| `packages/web/src/web-remote-control/`（含 `auth/`） | 官方网页远控前端（终端角色）+ 其 OAuth 登录模块 | 终端授权流与账号 JWT 来源的证据源 |
| `packages/desktop/src/main/web-remote-relay/` | 官方桌面端（设备角色）relay 客户端 | 设备注册/注销与凭据持久化对照面 |

**仍然不在仓库**：云 relay/hub 服务端本体与 `zcode.z.ai` 站点部署配置。因此 B0 的结论
「relay wire 证据只能来自客户端侧实现 + 授权返回 + 真实脱敏帧」**维持成立**，且本轮起
客户端侧证据已实际在手。

## 6.2 官方允许的终端登录／授权流（逐条行号证据，文件均 @ `3c5b2276…`）

缩写：`shared/contract.ts` = `packages/shared/src/web-remote-relay/contract.ts`；
`web/transport.ts` = `packages/web/src/web-remote-control/adapters/webRemoteControlTransport.ts`；
`web/v2token.ts` = `packages/web/src/web-remote-control/adapters/webRemoteControlV2Token.ts`；
`entry.ts` = `packages/web/src/web-remote-control/app/webRemoteControlEntry.ts`。

**A. 账号身份（OAuth → ZCode JWT）**

1. 授权入口（ZAI 通道）：`{chat.z.ai}/api/oauth/authorize?redirect_uri=<回调>&response_type=code&client_id=<公开client_id>&state=<state>`
   （`packages/web/src/auth/zaiWebOAuthProvider.ts:184-191`；默认 origin `https://chat.z.ai`，
   `webZaiOAuthConfig.ts:30`）。BigModel 通道变体：`{bigmodel}/login?redirect=<回调>&appId=zcode&state=<state>`
   （`zaiWebOAuthProvider.ts:175-181`；`webZaiOAuthConfig.ts:40-43,60-61`，appId 默认 `zcode` 即「桌面端在用的」）。
2. `client_id` 默认 `client_P8X5CMWmlaRO9gyO-KSqtg`，源码注释明示「client_id 会出现在授权 URL 中，
   属于公开配置」（`webZaiOAuthConfig.ts:57-58`）——它是**官方网页 app 的第一方公开客户端**，
   不是第三方客户端发放通道。
3. 回调 redirect_uri **钉死**官方网页自身：`webRemoteCallbackUrl = {zcodeOrigin}/remote/v4`
   （`webZaiOAuthConfig.ts:62` + `packages/shared/src/zcodeEndpoint.ts:378`；callback 判定
   `/remote/v4` + `state` + `code|authCode|error`，`entry.ts:17-26`）。exchange 时回传同一
   redirect_uri（`zaiWebOAuthProvider.ts:225`）。dev 环境另有 `VITE_WEB_REMOTE_ALLOW_DEV_RETURN_TO`
   门控的 devOrigin 回调（`webZaiOAuthConfig.ts:64-65,74-81`）——构建期开关，不是产品第三方通路。
4. token 交换：`POST {zcodeOrigin}/api/v1/oauth/token`，body `{provider, code, redirect_uri, state}`
   → `{code:0, data:{token:<ZCode账号JWT>, zai|bigmodel:{access_token}, expires_in, user}}`
   （`zaiWebOAuthProvider.ts:211-239`；`data.token` 即 `zcodeJwtToken`，121-124,151-156）。
5. JWT 持久化在网页 localStorage，读用时校验过期，**过期即 clearAll 并要求重新 OAuth**；
   logout = `clearAll()`（`packages/web/src/auth/browserOAuthCredentialRepo.ts:145-179`；
   `webAuthService.ts:205-207`；`entry.ts:250-255`）。**没有 refresh token 流**——「刷新」即重新走 OAuth。

**B. 终端角色接入（relay token → WS 握手）**

6. 终端一次性 token：`POST /api/v1/remote/token`，`Authorization: Bearer <ZCode JWT>`，
   body `{role:"terminal"}`（路径表 `shared/contract.ts:96-101`；请求形状 `web/v2token.ts:13-22`）。
   响应 `{code:0, msg, data:{token, expires_in}}`；token = 16 字节无填充 Base64URL 的 22 字符、
   末字符∈`[AQgw]`（schema `shared/contract.ts:119-123`，注释明示 16 字节语义）。
7. **一次性、不缓存**：「一次性 token 不缓存；401 交给入口恢复登录」（`web/v2token.ts:8`）；
   终端传输**每次连接尝试都现取新 token**（`web/transport.ts:104`）；401 → `auth-required`
   终态交上层回登录（`web/v2token.ts:23`；`entry.ts:236`）。→「重连需新 token」由实现结构保证。
8. WS 连接：`{relayWsUrl}` = `wss://<relay>/api/v1/remote/ws2`（`zcodeEndpoint.ts:380`；路径常量
   `shared/contract.ts:102`），URL 带 `?token=<一次性token>`（可选 `mid`）（`web/transport.ts:106-110`）。
9. 握手：open 后发 `auth_init{device_sid, meta:{platform:"web",version,name:"mobile-browser"}, client_ts}`
   （`shared/contract.ts:19-24`；`web/transport.ts:119-143`），服务端回
   `auth_ack{device_sid, terminal_sid?, pair_status: waiting|matched, server_ts?}`
   （`shared/contract.ts:53-59`）；客户端要求 `device_sid` 回显匹配才认账（`web/transport.ts:318`）。
   `waiting` 为合法配对前态（心跳 `pair_status_query` 维持，30s 未 matched → desktop-disconnected 终态，
   `web/transport.ts:460-473`）。
10. 数据面门禁：`data` 帧**仅 authenticated 且 paired** 才处理（`web/transport.ts:329-331`）；
    发送侧未 paired 直接返回 unavailable——不发送、不排队（`web/transport.ts:216-222`）。
    出站/入站 data 形状 `{type:"data", payload, client_ts?|server_ts?}`（`shared/contract.ts:36-44,66-70`）。
11. 错误帧 `error{code: AUTH_FAILED|WRONG_PARAM|KICKED|INTERNAL|DEVICE_OFFLINE, message?, server_ts?}`
    （`shared/contract.ts:18,71-76`）。终端侧处置：`KICKED` → `kickedDelay()`：60 秒窗口内已有
    一次 KICK 则返回 null → **终态失败 session-conflict（state=kicked，停止）**，否则 2–5 秒后
    重连一次（`domain/retry.ts:26-30`；`web/transport.ts:389-394`）；`AUTH_FAILED` → 终态
    `device-not-owned`、`WRONG_PARAM` → 终态 `invalid-mobile-connection`（`web/transport.ts:402-407`）。
12. **单 owner 的直接实现证据**：官方生命周期注释「同一个远控链接建立第二条 terminal socket
    并被 relay 踢成 KICKED」（`packages/web/src/web-remote-control/app/webRemoteControlLifecycle.ts:29-33`）
    ——同链接（同 device_sid/role）的第二个终端连接会踢掉第一个；终端重试参数
    initial 500ms / max 10s（`web/transport.ts:45-49`），设备侧 initial 1s / max 30s
    （`domain/retry.ts:9-12` + 桌面 `packages/desktop/src/main/web-remote-relay/webRemoteControlTransport.ts:51`）。

**C. 对照面（设备角色与注销）**

13. 设备注册：`POST /api/v1/remote/device {device_mid}` → `device_sid`；注销：
    `POST /api/v1/remote/device/delete {device_sid}`；列表 `GET /api/v1/remote/device`
    （`shared/contract.ts:96-101,105-118`；`packages/desktop/src/main/web-remote-relay/webRemoteControlV2Http.ts:47-61`）。
14. 设备侧凭据持久化 = 桌面 App settings 的 `webRemoteControlRelayV2Device` 字段
    （`packages/desktop/src/main/web-remote-relay/webRemoteControlDeviceStorage.ts:11-24`）——
    官方桌面端同样**没有** OS keychain，注册记录（非凭据）落 settings；legacy 凭据有显式清除迁移。
15. 链接形状：`{zcodeOrigin}/remote/vN?sid=<device_sid>&mid=<device_mid>&name=<name>&app_version=<v>`
    （构造 `packages/shared/src/web-remote-control.ts:163-189`；解析 191-207：**带 legacy `hash`/`t`
    参数直接判 null 拒绝**；选择设备后 URL 才带 sid/mid，`entry.ts:186-199`）。与 A4 合同
    （E7：`https` + `zcode.z.ai` + `/remote/vN` + 非空唯一 `sid`）一致，且补充：vN 当前取
    v3/v4（按 appVersion 门控，`zcodeEndpoint.ts:371-377`）。

## 6.3 第三方可复用授权路径核查（B1 关键问题）

**结论：本仓库源码没有展示任何第三方可复用的授权返回通路。** 证据：

- 唯一的账号凭据获取流是官方网页自己的 OAuth：client_id 为第一方公开客户端（§6.2-2），
  redirect_uri 钉死官方 `/remote/v4` 回调（§6.2-3），exchange 回传同一 redirect_uri；
  仓库内没有第三方 client 注册、动态 redirect、PAT/设备码（device flow）或任何
  「把账号凭据交给非官方客户端」的机制（全仓树清查 + `packages/web/src/auth/` 全量阅读）。
- 设备角色（桌面端）的注册凭据是桌面 App 自己的账号 JWT，属**其他应用凭据**，按工作包 B
  约束禁止读取。
- 因此：`auth.status = interim`（不是 proven，也**尚不能**判 unsupported——缺失的不是流程知识，
  而是「官方授权服务器是否接受 Tuvis 控制的 redirect_uri」这一服务器侧行为，仓库无法证明或证伪）。
  **禁止**以习惯假设官方支持任意第三方 OAuth client；该问题只能由 §6.7-③ 的用户在场浏览器
  探测或官方渠道问答定案。

## 6.4 contract.json（auth/route 填写与证据类别）

- 文件：`tests/fixtures/zcode-official-bridge/contract.json`（schemaVersion=1，本轮新建）。
- `auth.status = "interim"`（计划枚举 proven|unsupported 之外的显式过渡值，附
  `interimMissing` 说明缺失项）；`flowName`/`tokenSourceKind`/`credentialPersistence`
  全部来自 §6.2 的 source 证据，`evidence[]` 逐条标 `class: source | fake`。
- `route.role = "terminal"`；`ownerScope` = 同 device_sid 单 owner、KICKED 冲突、探针即停口径；
  `workspaceBindingFields`/`sessionBindingFields` = 空数组 + `bindingNote`（relay 层只有
  `device_sid` 绑定；workspace/session 绑定属 RPC 层，**留 B2 真实帧取证，不猜测字段**）。
- `capabilities` 全 false（未实连，无真实帧）；`operations` 空数组（同因）。

## 6.5 fake relay 红测证据（tests/remote/zcodeBridgeRelay.test.ts）

- 文件首行 `// @vitest-environment node`（仓库既有约定，同 `tests/lib/errorCodesGate.test.ts:1`）；
  未新增任何依赖、未引入 tsx；WS 客户端用 Node ≥22 全局 `WebSocket`，fake 服务端为本文件内
  ~120 行的最小 RFC6455 实现（`node:http` upgrade + SHA1 accept + masked 帧解析）。
- **RED**（先写测试、`scripts/zcode-bridge-probe/relayAuth.ts` 尚不存在）：
  `pnpm exec vitest run tests/remote/zcodeBridgeRelay.test.ts` →
  `Error: Cannot find module '../../scripts/zcode-bridge-probe/relayAuth'`（1 failed suite）。
- **GREEN**（实现 `relayAuth.ts` 后同命令）：**5 passed / 0 failed**，无 MSW 干预
  （文件级 `beforeAll` 关闭全局 MSW，仅本文件作用域）：
  1. 未认证（错/空凭据）申请 token → 401×2、`tokenMinted=0`（没有任何可消费 token 产出）；
     裸 WS 无 token：`auth_init` 即 `error{AUTH_FAILED}`，其后 data 帧被丢弃不计数（`dataAccepted=0`）。
  2. 合成合法凭据：`role:"terminal"` 申请成功、token 合官方形状、`expires_in>0`；两次申请
     token 不同（一次性）；WS `auth_init→auth_ack(matched)` → paired；paired 后 data 被接受。
  3. 过期 token（60ms TTL）连接 → `rejected`；已消费 token 重连 → `rejected`；两者
     `sendData` 均 false（认证失败不发后续帧），`wsAuthFailed` 计数精确 +1/+1。
  4. SID 形态字符串冒充 Bearer → 401；冒充 WS token → `rejected`；`dataAccepted` 恒 0。
  5. KICKED 注入 → 客户端终态 `kicked`、不再发送；等待 100ms 后 `wsUpgrades` 仍为 1
     （**结构性无重连**：客户端没有重连代码路径，`relayAuth.ts` 无任何计时器）。
- 被测客户端 = `scripts/zcode-bridge-probe/relayAuth.ts`（Tuvis 参照实现）：单连接、无自动重连、
  token 一次性不落盘、未 paired 不发送、KICKED/拒绝即终态；每个常量与分支注释都标了
  官方源码行号。fake 侧「FAKE 策略」注释明确区分了本测试自定行为（如升级必成、auth_init 后
  才校验 token、未认证 data 丢弃并断开）与官方取证结论。

## 6.6 实连半场探针骨架（scripts/zcode-bridge-probe/auth-check.ts）

- 骨架仅声明 6 步（取链接→账号凭据通路定案→申请 terminal token→单连接握手→只读观察→
  断开落证）与硬安全闸（单连接、`reconnectPolicy:"never"`、KICKED 即停、只读白名单、
  60s 硬超时、凭据只入内存）。`main()` 在解锁旗标缺席时**一律抛错拒绝执行**；
  本轮不连接任何远端；不挂产品启动/扫描。

## 6.7 停等项（用户动作；全部完成前不执行任何真实连接/写实验）

1. **有效官方链接（阻塞实连）**：现存的两个月前链接已显示 Not Found，**禁止复用**。
   请用户本人在 ZCode APP 内开启移动远控→复制**当前有效**链接→经 Tuvis 设置页 ZCode 卡
   （A4）粘贴保存；链接本体按合同不入本文档/日志/夹具。
2. **专用测试目标（阻塞任何真实写实验，B2 前置）**：指定专用测试工作区/会话，并在本文档
   §4 追加小节登记别名与逐批授权动作清单（真实路径/ID 只留私有目录映射）。
3. **账号凭据通路探测授权（决定 interim→proven/unsupported，实连半场第一闸）**：
   官方终端 token 需要**账号 JWT**（§6.2-6），而 JWT 的官方获取流是浏览器 OAuth（§6.2-1—5），
   其 redirect_uri 钉死官方回调、仓库未见第三方通路（§6.3）。需要用户**在场**二选一：
   (a) 授权做一次浏览器 OAuth 通路探测（用户本人操作浏览器完成官方授权，验证官方授权服务器
   是否接受 Tuvis 本地回环 redirect——不接受即 `auth.status=unsupported` 定案）；
   (b) 或直接裁决走官方渠道询问/其他合法备选。Agent 不代登、不读浏览器/其他应用凭据、
   不要求用户粘贴 JWT 原文。
4. **同 role 互踢知情确认（实连前）**：单 owner 语义（§6.2-12）意味着探针连接可能踢掉用户
   正在使用的官方网页会话；实连时段需用户确认不依赖网页会话，且探针 KICKED 即停不互踢。

## 6.8 本轮遗留与风险登记

- `expires_in` 的真实时长是服务器侧值，仓库不可见；「token 短期性」只能实连观测（§6.7 全部
  完成后于实连半场验证）。
- 官方服务端对「未认证 data 帧」的处置未取证（fake 取保守丢弃+断开策略并标注 FAKE 策略）；
  Tuvis 客户端行为不受影响（发送侧门禁使未 paired 从不发出 data）。
- `terminal_sid`（auth_ack 可选字段）的确切语义/稳定性待实连帧确认，暂不入合同字段。
- B1 实连半场的验收门维持计划原文：「proven 且实际只读连到正确测试 APP」才进 B2。
