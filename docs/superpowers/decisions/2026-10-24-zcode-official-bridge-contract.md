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
| E3 | 上述方法在**本机安装版随包 CLI** 中存在（PACKAGE，不升级运行 PASS） | P0 复核 §2.2 末段静态计数：interaction/requestPermission×1、interaction/requestUserInput×1、resolveInteraction×5、session/stop×4；本轮确认 zcode.cjs 哈希（§1.2）使该计数对应当前文件 | PACKAGE |
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
