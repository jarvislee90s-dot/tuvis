# ZCode 官方桥生产验收报告（T8.2）

- 分支：`codex/zcode-unified-remote`
- 计划基线：`8e663f09`（计划与审计文档提交 `06b7f759`）
- 验收入口 HEAD：`981ee02b`（`feat(zcode): arm one evidence action from desktop`）
- 工作树：验收开始时干净；本报告与决策 §8.11 为唯一新增改动，未改产品代码
- 范围：T1.1–T8.2 的代码完成度、自动化门禁、已取得的实机证据、T8.2 产品验收行与剩余缺口
- 纪律：`docs/MASTER-PLAN.md` 未改动；无 push / merge / tag / 发版；不写 SID、JWT、正文与私有 transcript

## 一、总判定

| 维度 | 判定 | 依据 |
| --- | --- | --- |
| `code_complete` | **是（本批代码）** | T1.1–T7.2 全部落地；T8.1 代码/测试落地（含已申报偏差）；T8.2 能力策略、证据许可、桌面许可入口与闸门接线落地；全量自动门禁绿 |
| `platform_validated` | **否（部分）** | 仅有 macOS 探针级实机证据 B1–B4；T8.2 产品 UI 验收行未跑；Windows 未验；手机体验未由用户确认 |
| `spec_complete` | **否** | T8.1 的 loopback HTTP+WS 假官方端缺口（I1）仍在；T8.2 强制实机矩阵无产品证据；八项控制能力全部保持关闭 |

**结论一句话：代码与自动门禁完成，平台验收与 spec 闭合未完成；不把「门按预期关闭」写成功能已交付。**

## 二、任务状态（T1.1–T8.2）

| 任务 | 状态 | 交付与边界 |
| --- | --- | --- |
| T1.1 | 完成 | OAuth（BigModel 回环已实跑）、JWT 仅内存、terminal token 每次现取；B1 只读连接已证 |
| T1.2 | 完成 | 单连接 relay、KICKED 终态不自动重连、心跳/断线/代际；无自动重发 |
| T2.1 | 完成 | 二进制 RPC 编解码、CRC32、分片/重组/上限、Initialize 向量 |
| T2.2 | 完成 | workspace bridge、任务索引、精确 session 绑定、投影/恢复；会话扫描预算遵守 |
| T3 | 完成 | 动作账本（`zcode_actions`）、actionId 幂等、单发、ACK 20s、保守 unknown |
| T4.1 | 完成 | pending 册、单选/多选/自由文本答案构造、snooze 绑题、宿主撤卡才 confirmed |
| T4.2 | 完成 | 有守卫 stop、`primaryTurn` 限定、初始 control 未确认零触发/零 stop |
| T5 | 完成 | 既有 permission 卡应答、`allowOnce`/`deny` 白名单、`fullAccess` 硬拒绝；发送催卡仍为负结果 |
| T6 | 完成 | 同 provider 模型/档位切换、按目标模型校验档位、CAS、新鲜快照确认、保留用户选择 |
| T7.1 | 完成 | 单实例 Hub、GET/actions/SSE、配对门、代际、吊销、读链不依赖控制成功 |
| T7.2 | 完成 | 手机控制面板、发送/卡面/打断/档位、19 个冻结错误码 zh/en、无 legacy/无头回退 |
| T8.1 | **部分完成（代码完成，规格评审 FAIL / I1）** | 进程内 `FakeOfficial` 在 `HubBackend`/`CommandPort` 边界覆盖 Hub+HTTP+命令层；缺 brief 要求的 loopback HTTP+WS 假官方端，`RelayClient ↔ BridgeManager ↔ CommandSender` 组合只有构造冒烟 |
| T8.2 | **代码完成；实机行 blocked/not_run** | `CapabilityPolicy`（宿主 key 全关）、`evidence.rs`（debug 单次许可）、桌面 IPC、Hub/CommandSender 闸门、无许可全关；产品实跑未执行，未开任何能力 |

## 三、能力矩阵（HEAD `981ee02b`）

图例：`实现` = 代码与自动测试已落地；`闸门` = 生产默认值；`证据` = 现有最强实机证据；`产品行` = T8.2 要求的产品 UI 验收。

| 能力 | 实现 | 生产闸门 | 现有证据（探针/合同层） | T8.2 产品行 |
| --- | --- | --- | --- | --- |
| 发送 send | 是（T3，单发/幂等/账本/审计） | **关闭**（`needs_evidence`） | B2 live：受理 accepted + 用户确认 marker 落在指定会话并被处理（强于 ACK） | `not_run` |
| 问答·单选 | 是（T4.1，原题/原值回传，宿主撤卡确认） | **关闭** | B3 live：ACK accepted；ACK 后约 5ms teardown，**未见宿主撤卡** → 终态未证 | `not_run` |
| 问答·多选 | 是（T4.1，按原 value 数组/归一） | **关闭** | 仅 fake/合同层；无 live | `not_run` |
| 问答·自由文本 | 是（T4.1，`freeTextAllowed` 派生） | **关闭** | 仅 fake/合同层；无 live | `not_run` |
| 审批应答（既有卡） | 是（T5，原 optionId、白名单、fullAccess 拒绝） | **关闭** | B3 live：approve 后文件被删除、reject 后文件保留（用户侧产物核对，强于 ACK）；**仅应答既有卡** | `not_run` |
| 打断 interrupt | 是（T4.2，守卫 id、零隐式 sendText） | **关闭** | B3 live：ACK accepted；ACK 后约 4ms teardown，**未见 activeWorks 守卫消失** → 执行停止未证 | `not_run` |
| 模型 model | 是（T6，同 provider、CAS、新鲜快照） | **关闭** | B4 live：fresh snapshot 确认切换；恢复原值仅探针清理 | `not_run` |
| 思考强度 thought | 是（T6，按目标模型自己的档位） | **关闭** | B4 live：`max → low → max` 新鲜快照确认 | `not_run` |
| 只读阅读 | 既有 A + T7.1/T8.1 | 不受能力闸限制 | B1 live：bootstrap/workspace-list ok；T8.1 e2e 读链 200；A 回归绿 | 自动回归通过；真机未跑 |

补充：`CapabilityPolicy::for_host` 已产出（APP build + 协议 + schema/hello 任一不匹配 → 全关）。`Hub::session` 现按下发手机的会话能力矩阵现算：基线为「未知宿主」组合（八项全关），debug 证据构建再叠加本会话/本代际**尚未消费**的单次许可（消费/过期/换代后下一次 GET 立即回到全关，手机按该字段硬门）。但宿主 build/协议/schema/hello 的**运行时来源仍未接线**（`contract.json` `production.knownGaps` 已登记 I-C）：即便拿到真实证据，也没有代码路径据此开闸。开闸前必须先接线，不能只改布尔值。

## 四、实机证据（B1–B4，macOS 探针）

合同身份（`tests/fixtures/zcode-official-bridge/contract.json`）：

- 平台：macOS 27.0.1 arm64（B0 run `macos-B20261024T142609+0800-k0zat1`）
- 官方 APP build：`3.16.2.8769`（CFBundleVersion）
- 固定源码：`3c5b227649371507ddc4eb46ac5133afe6db1cab`
- 证据目录：`~/Documents/tuvis-verify-logs/zcode/<run_id>/`（仓库只引用 run id；不含 SID/JWT/正文）

| 阶段 | run id | 动作 | 实际观察 | 终态口径 |
| --- | --- | --- | --- | --- |
| B1 | `macos-B120261024T105940Z-NarmSw` | BigModel 本人登录 → 回环授权 → terminal token → WS paired → 只读 bootstrap/workspace-list | OAuth PROVEN；auth 后约 131ms matched；两项只读均 ok | **终端可读证据**（非 ACK） |
| B1（负/辅助） | `macos-B120261024T102434Z-SbxvWA`；`macos-B120261024T104607Z-iykCdg`；`macos-B120261024T104720Z-OMksKw` | ZAI 通道 / 缺 mid 连接 | ZAI callback-timeout（账号在 BigModel 侧，inconclusive）；缺 mid 恒 waiting → pair-timeout | 负结果已登记，不冒充成功 |
| B2 | `macos-B220261024T132316Z-prItvw` | 精确 workspace/session 绑定 → sendText 合成 marker | 受理 accepted；framesSent=1、peerAck=true；用户确认 marker 以用户消息落在指定会话且 agent 回复（时间戳与 transcript 一致） | **ACK + 用户可见产物**（发送闭环） |
| B3 问答 | `macos-B320261025T012945Z-aMtnqA` | 单选 AskUserQuestion → 原值应答 | CommandAck accepted；ACK 后约 5ms teardown，未观察撤卡 | **仅 ACK**；宿主 pending-clear 未证 |
| B3 打断 | `macos-B320261025T013128Z-X8HkPg` | 有守卫 stop | CommandAck accepted；ACK 后约 4ms teardown，未观察 activeWorks 变化 | **仅 ACK**；执行停止未证 |
| B3 审批 approve | `macos-B320261025T032425Z-K7I6ZA` | 用户 APP 亲发 guarded 提交催卡 → allow-once | 卡 3 选项（fullAccess 被客户端硬拒）；文件被删除 | **终端产物已证**（应答既有卡） |
| B3 审批 reject | `macos-B320261025T034803Z-noorSA` | 同法再开卡 → deny | 文件仍在 | **终端产物已证**（应答既有卡） |
| B3 催卡负结果 | `macos-B320261024T230311Z-aTrTIg`；`macos-B320261025T031402Z-QjC2hA` | terminal/探针 sendText（默认与 `mode:"guarded"`） | 均不弹 permission 卡、直接执行（共 4 轮负结果） | `approvalForTerminalSend=false` |
| B4 档位 | `macos-B320261025T025928Z-3WyTFA` | `max → low → max` | ack accepted；新鲜 snapshot 每次确认；恢复仅探针清理 | **终端投影已证** |
| B4 模型 | `macos-B320261025T030637Z-bS5dOg` | `GLM-5.3-Flash → GLM-5.3`（同 provider） | `getView` 目录可用；ack accepted；新鲜 snapshot 确认；随后恢复 | **终端投影已证** |

口径纪律：

- **ACK ≠ 终态**：问答/打断的 accepted 只证明命令受理，不证明宿主撤卡/执行停止；审批的产物核对强于 ACK，但仅限「应答既有卡」。
- **探针 ≠ 产品链**：B1–B4 是 `scripts/zcode-bridge-probe` 的探针证据，不是 Tuvis 产品 UI + 手机操作链证据；不得直接用于开生产能力。
- **合同宽泛 true ≠ 生产能力**：`contract.json` 的 `capabilities` 只记录探针观察，`CapabilityPolicy` 不复制。

## 五、T8.2 产品验收行（未执行，禁止伪造 PASS）

未执行原因：当前无用户本人官方登录的专用测试会话、无用户指定的测试目标、无 Windows 主机；不能在缺证据时改权限或源码造卡。

macOS 生产构建（APP build `3.16.2.8769`，生产 commit `981ee02b`，run id `—`，目标别名 `—`）：

| 场景 | 动作 | 预期 | 实际 | 终态 | 判决 |
| --- | --- | --- | --- | --- | --- |
| 官方授权/连接 | Tuvis 登录 → paired → 重启 | 重启后 needs_auth、无旧 JWT | 未执行 | `—` | `not_run` |
| 手机发送 | 指定 session → 发送合成 marker | APP 可见且被处理；审计一次 | 未执行 | `—` | `not_run` |
| 单选 | 原题/原值回传 | 宿主撤卡 + 后续行为一致 | 未执行 | `—` | `not_run` |
| 多选 | 原 value 数组 | 同上 | 未执行 | `—` | `not_run` |
| 自由文本 | 原文回传 | 同上 | 未执行 | `—` | `not_run` |
| 打断既有执行 | 用户先启动可安全终止任务 → stop | 宿主 id 消失、APP 停止、不误伤新执行 | 未执行 | `—` | `not_run` |
| 审批批准/拒绝 | 原 optionId → 产物变化/不变 | 宿主撤卡；fullAccess 不可用 | 未执行 | `—` | `not_run` |
| Tuvis 发送→审批 | 不改权限发起动作 | 确实弹卡并可应答 | 未执行（探针有负结果） | `—` | `blocked`（spec 缺口，`approvalReply` 保持关闭） |
| 模型/思考强度 | 同 provider 各切一次 | 新鲜 config == 目标；离开再回一致 | 未执行 | `—` | `not_run` |
| 两台手机 | 共用一条 relay、同 actionId | 仅一条上游写；不抢 seq/不误答 | 未执行 | `—` | `not_run` |
| 断线/ACK 丢失/重启 | 断网、重启、重连 | 不自动重发；unknown 如实；旧 pending 不可提交 | 未执行 | `—` | `not_run` |
| 失效链接/更新/删除 | 改链接/吊销 | 旧代际失效；正文/文件仍可读 | 未执行 | `—` | `not_run` |
| 官方网页同时连接 | 第二 terminal 连接 | KICKED → conflict，零自动互抢 | 未执行 | `—` | `not_run` |
| A 包实机回归 | 六段/PIN/默认值/ID/锚点 | 全部保持 | 仅自动回归 | `—` | `not_run`（自动部分通过） |

Windows：全部行 `blocked`（无 Windows 主机，未构建、未连接、未跑）；不以 macOS 探针结果替代 Windows 产品验收。

## 六、自动门禁（本 HEAD 实跑）

| 命令 | 结果 |
| --- | --- |
| `pnpm test` | **2340 passed / 5 skipped（194 文件）** |
| `cd src-tauri && cargo test --features marker-helper` | **3765 passed / 0 failed / 46 ignored**（T8.1 评审时记录 3759；本 HEAD 含后续新增用例） |
| `cd src-tauri && cargo test --features marker-helper,zcode-bridge-evidence` | **3772 passed / 0 failed / 46 ignored**（T8.2 证据特性 +7） |
| `cargo test --lib services::zcode_control::types::tests::policy`（evidence 构建） | 4 passed / 0 failed |
| `cargo test --lib services::zcode_control::evidence::tests`（evidence 构建） | 4 passed / 0 failed |
| `cargo test --test zcode_control_e2e --test zcode_control_remote` | 8 + 25 passed / 0 failed |
| `pnpm check`（format/lint/i18n/errcode/tsc/build） | exit 0 |
| `pnpm build:mobile` | exit 0 |
| `cargo fmt --check` | exit 0 |
| `cargo clippy --all-targets --features marker-helper -- -D warnings` | exit 0 |
| `cargo clippy --all-targets --features marker-helper,zcode-bridge-evidence -- -D warnings` | exit 0 |

关键 T8.2 用例：`policy_never_uses_broad_probe_true_as_production_enable`、`unsupported_protocol_stays_readable_but_control_disabled`、`unknown_schema_or_hello_is_needs_evidence`、`verified_fixture_is_test_only_and_not_default`、`evidence_permit_single_use_and_expires`、`evidence_permit_does_not_bypass_target_or_permission_guards`、`evidence_permit_invalidates_on_generation_change`、`release_evidence_build_rejected`、`evidence_permit_allows_one_action_then_closes`、`snooze_binds_permit_without_consuming`、`evidence_permit_does_not_bypass_full_access_or_target_guards`。

## 七、已知缺口与最小用户协助

### 必须由用户在场/授权才能推进

1. **T8.2 产品实跑**：用户本人官方登录，指定一个专用测试会话（原 session ID）与测试目标；按强制矩阵逐行执行。无登录/无指定会话时只能保持 `blocked`。
2. **Tuvis 发送→审批催卡**：探针已有 4 轮负结果（terminal sendText 不弹卡）。需用户在同一会话以产品 UI 发起动作并确认是否弹卡；仍不弹则 `approvalReply` 保持关闭，整批只能标「能力受限」。
3. **Windows 验收**：需 Windows 主机与同一 APP 构建；未跑前不宣称双平台。
4. **手机体验确认**：发送/卡面/打断/档位的手机侧观感由用户确认；当前无此证据。

### 工程侧未闭合

1. **T8.1 I1**：brief 要求的 loopback HTTP+WS 假官方端未交付；`RelayClient ↔ BridgeManager ↔ CommandSender` 组合只有构造冒烟。不得以进程内假体冒充「整条生产链」；需补 loopback fake 或书面移交。
2. **问答/打断终态**：单选 answer 与 interrupt 仅有 ACK，未观察宿主撤卡/执行停止；需产品实跑补终态。
3. **多选/自由文本**：仅 fake/合同层，无 live。
4. **审批范围**：仅「应答既有卡」已证；`approvalForTerminalSend=false`，不得把 guarded 模式宣传为对 Tuvis 发送的保护。
5. **宿主 key 接线**：`CapabilityPolicy::for_host` 尚无运行时 APP build/协议/schema/hello 来源，生产装配使用全关默认矩阵；开闸前必须接线。
6. **手机「验收模式」标识**：brief 要求手机明确显示验收模式；本批只交付后端许可与桌面发放，前端标识尚未落地（未开放手机发放接口）。
7. **合同/手册/CHANGELOG**：`contract.json` 的 T8.2 门字段、`08-mobile-remote.md`、`CHANGELOG{,.en}.md` 未随本次验收提交；发版矩阵与 release-notes 按发版纪律另行同步。

### 明确未做

- 未修改 `docs/MASTER-PLAN.md`；未 push、未合并、未打 tag、未发版。
- 未开放任何生产能力：八项控制能力在普通构建中全部 `enabled=false` / `needs_evidence`；`zcode-bridge-evidence` 仅 debug 构建可用，release + 该 feature 直接编译失败。
- 未使用真实账号凭据、SID、JWT、正文或私有 transcript 入仓；夹具均为合成值。

## 八、复核入口

- 决策记录：`docs/superpowers/decisions/2026-10-24-zcode-official-bridge-contract.md` §8.10、§8.11
- 合同：`tests/fixtures/zcode-official-bridge/contract.json`
- 计划与审计：`docs/superpowers/plans/2026-10-25-zcode-official-bridge-implementation.md`、`docs/superpowers/reviews/2026-10-25-zcode-production-plan-audit.md`
- 任务账本：`.superpowers/sdd/2026-10-25-zcode-official-bridge-implementation/progress.md`、`task-T8.1-spec-review.md`、`task-T8.1-report.md`
