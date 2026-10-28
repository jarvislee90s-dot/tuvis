# ZCode 能力对齐 · Task 9 验收记录（UI 合规 + 未知 build e2e + 文档 + 全量门禁）

- 分支：`codex/zcode-unified-remote`
- 验收代码基线：`191bcc31`（T1–T8 交付）+ 本任务改动（Task 9 收口 commit，见 git log）
- 验收平台：macOS 27（darwin 27.0.0 arm64，开发机）
- 工具链：rustc 1.97.1 / cargo 1.97.1 / Node v26.8.1 / pnpm 11.23.0
- 范围：spec R5/§7.6 UI 能力呈现合规、未知 build 接口候选 + send 首次确认的全链 e2e、
  spec 实现偏差小节、用户手册 zh/en、CHANGELOG zh/en、全量门禁
- 纪律：不改 `docs/MASTER-PLAN.md`；e2e 全链回环假官方（HTTP/WS），零真实官方端点、
  零真实凭据；无 JWT / 链接 / 正文 / 答案进入账本 / 日志 / DTO；未 push / merge / tag

## 一、总判定

| 维度 | 判定 | 依据 |
| --- | --- | --- |
| `code_complete`（本任务范围） | **是** | UI 合规钉住测试 + 未知 build 全链 e2e 全 GREEN；全量门禁 9 条全绿（§三） |
| `platform_validated` | **否** | 实机未知 build 验证、send 指纹推进形态取证、takeover 真机端到端**未执行**（§四） |
| `real_official_verified` | **否** | e2e 使用回环假官方 + 合成会话/任务索引；未连接真实官方 relay / 账号 |
| `spec_complete` | **本任务是** | R5/§7.6 UI 验收项逐条钉住；§7.3 的 e2e 证明面已闭合（§二）；偏差已在 spec §12 记录 |

**一句话结论：Task 9 代码与自动化门禁完成；实机（真实未知 build / 真实 send 指纹形态 /
真实手机抢回）验收未跑，如实留待平台验收。**

## 二、验收证据

### 2.1 UI 能力呈现合规（spec R5/§7.6）——钉住测试

T1–T8 已把面板做成合规形态，本任务**逐条核对后未发现缺口，以测试钉住**（GREEN on
arrival；先读 `src/mobile/ZcodeControlPanel.tsx` 现状，未重复造轮子）：

| 验收项 | 现状核对 | 钉住测试 |
| --- | --- | --- |
| 无 pending 不显示审批/问答卡 | `session.pending.map(...)` 驱动，无 pending 零卡片 | `tests/mobile/ZcodeControlPanel.test.tsx` → `no probe UI and no approval card without pending` |
| 无 selection 目录/档位不显示模型控件 | `selection !== null` 才渲染两个 select | 同上（`zcode-model-select` / `zcode-thought-select` 为 null） |
| 只有 `capabilities.*.enabled` 才显示可用 | send/interrupt/model/thoughtLevel 全部先读矩阵；UI 零本地推断 | 同上（send enabled → 按钮可见可用、无原因行）+ 既有 `send_capability_disabled_shows_reason` 等 |
| 不可用给原因（本地化） | `zcodeReasonText` 19 码词表 + `data-reason-code` 冻结码 | 既有 `all_reason_codes_have_zh_en_copy` / `zh_and_en_reason_copy_differ` / `en_panel_has_no_hardcoded_chinese` |
| 无探测页/按钮/徽标 | 面板无 `zcode-probe`、无任何「探测/probe」字样元素 | 同上（`queryByTestId("zcode-probe")` 为 null + 文本断言） |

桌面设置卡（`ZcodeRemoteSettingsCard.tsx`）核对结论：卡片只承载链接配置/授权/连接，
**不含能力矩阵或探测 UI**，无需新增钉住测试（brief 的「若需」条件不成立）。

### 2.2 未知 build 全链 e2e（spec §7.3/§8）

新增 `unknown_build_send_confirms_without_probe_message`
（`src-tauri/src/services/zcode_control/wire_e2e.rs`），真实生产组件 × 回环假官方：

- **装配**：`PartsBackend::production_test_with_host_build`（新测试装配缝，确定性注入
  未知 build `9.9.9.9999`）+ `ZcodeHub::with_pump_policy_source_and_confirmation`
  （策略矩阵 = 生产同源 capability resolver，确认落账与 resolver 共用同一 DB 账本；
  不注入静态矩阵——这是与既有 `full_chain` 用例的本质差别）。
- **断言面**（全部经真实 HTTP/router/Hub/Relay/Bridge/CommandSender/事件泵）：
  1. resolver 候选开放：GET session → `send.enabled=true` 且无原因码；形状缺席的
     singleSelect/multiSelect/freeText/model/interrupt 保持 `needs_evidence`；
  2. 发一条真实 send → `accepted`；命令信封恰一帧（正文原样、`clientId` 来自配对门）、
     零终端注入回落；
  3. revision 前进：send 后会话水位 1 → 2（假体按宿主形态推跟进快照，见 2.3）；
  4. 确认流转：事件泵观察「revision 前进 + 新用户消息指纹（logEpoch:seq）」→ 账本行
     `state=confirmed`、判据词 `revision_advanced+new_user_message`（结构化，无正文）；
  5. 确认后 GET 仍可用（confirmed 不降级）；
  6. **无探针消息**：业务请求序列逐条钉死为
     `bootstrap → listing → 枚举桥 → 控制器订阅 → 会话桥 → hello → clientHello →
     会话订阅 → 用户的那一条 send`，静置观察窗后仍恰一帧命令信封。
- **RED → GREEN**：先装配缝 + 测试、假体不推跟进快照 → RED
  （`send 后会话 revision 必须前进到 2`，5s 等待超时；确认账本同样无 confirmed 行）；
  补上假体跟进快照后 GREEN，且 13 个既有 wire_e2e 用例无一破坏。

### 2.3 假体跟进快照的边界（非探针消息的说明）

假体在 `sendConversationCommandV4` **Accepted** ACK 后推一条 revision=2 的会话快照：
它不由生产代码发出、不携带任何文本，只是让假体作为「宿主状态」的形态来源给出 R4
确认判据要求的观察证据（宿主吸收消息后 revision/日志头前进）。`rejected`/`malformed`
ACK 不推（宿主状态未前进），与既有 `rejected/malformed` 用例语义一致。

## 三、全量门禁（brief Step 4 逐字命令，最终代码状态重验）

| # | 命令 | 结果 |
| --- | --- | --- |
| 1 | `cargo test --lib services::zcode_control::` | **GREEN**：550 passed / 0 failed |
| 2 | `cargo test --features marker-helper` | **GREEN**：全目标 0 failed（含 lib 3521 + 集成各套件） |
| 3 | `cargo test --tests --features marker-helper,zcode-bridge-evidence` | **GREEN**：全目标 0 failed |
| 4 | `cargo clippy --all-targets --features marker-helper -- -D warnings` | **GREEN** |
| 5 | `cargo clippy --all-targets --features marker-helper,zcode-bridge-evidence -- -D warnings` | **GREEN** |
| 6 | `cargo fmt --check` | **GREEN** |
| 7 | `pnpm test` | **GREEN**：194 文件 / 2371 passed + 5 skipped / 0 failed |
| 8 | `pnpm check` | **GREEN**（含一次既有红灯修复，见下） |
| 9 | `pnpm build:mobile` | **GREEN**（2464 modules transformed） |

**既有红灯修复（T9 收口内完成，非掩盖）**：`pnpm check` 的 `check-error-codes.mjs`
在**未含本任务改动的 HEAD（`191bcc31`）上即失败**——T2 交付的
`database/dao/zcode_capability.rs` 有 5 条未迁移错误串 + 裸 `{e}` 体同串多码 +
4 个 locale 缺键。修复：账本读/写错误正文补独立描述（消除同串多码）、
`validate_evidence_token` 5 条文案挂 `EVIDENCE_TOKEN_INVALID` 码前缀、
zh/en `errors` 段补 `capabilityLedgerReadFail` / `capabilityLedgerWriteFail` /
`evidenceRejected` / `reasonCodeRejected` / `evidenceTokenInvalid` 五键。
行为面零变化（这些 Err 只内部消化为 warn / 空 Vec），`database::dao::zcode_capability`
8 用例保持 GREEN。修复后整门禁重跑全绿（上表即最终状态）。

## 四、真机未跑项清单（如实登记，不伪造）

| 项 | 状态 | 说明 |
| --- | --- | --- |
| 实机未知 build 验证 | `not_run` | 真实官方 APP 升级后的未知 build 上验证「接口候选开放 + 首次确认转正 + 失败降级」需真实官方端点与真实账号，自动化环境不可得 |
| send 指纹推进形态取证 | `not_run` | `logEpoch:seq` 指纹作「新用户消息可见」代理判据的真实宿主推进形态（含 epoch 轮换/seq 回退边界）需真机 wireshark/日志取证 |
| takeover 真机端到端 | `not_run` | 真实手机 + 真实官方页面在线时的「断开并抢回」全链（含官方页反抢时序）未跑；自动化覆盖为回环假体单飞/幂等/失败码 |
| 真实系统保险箱 × 新账本 | `not_run` | 账本落 `~/.tuvis/tuvis.db` 的真实库迁移已在单测覆盖（in-memory migration 全套），真实用户库升级路径随发版验收 |

## 五、文档交付

- `docs/superpowers/specs/2026-10-28-zcode-capability-parity-spec.md`：新增「§12 实现偏差」
  （schema_fingerprint 占位 `"v4"` / probe_epoch 0；send 指纹 = `logEpoch:seq` 代理判据；
  确认窗 5s vs 回执 rpc 10s 时序；takeover「确认」= 常驻说明 + 单击即发），其余章节未改。
- `docs/user-manual/zh/08-mobile-remote.md` + `en/08-mobile-remote.md`：ZCode 统一控制节
  重写为 how-to——能力按 build 逐项开放（含未知 build 候选与首次确认）、能力不可用看
  原因、无探测 UI 申报、冲突态「断开并抢回」操作步骤与单飞/不循环边界；中英同步。
- `CHANGELOG.md` / `CHANGELOG.en.md`：`[Unreleased] → Added` 各新增两条（build 级能力解析、
  手机冲突态断开并抢回），两份逐条对齐、各自单语。
