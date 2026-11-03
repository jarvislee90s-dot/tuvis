# opencode 2.x 适配批实施计划（D0–D3）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** MAM 全产品对 opencode 2.x 的兼容恢复——发现层双 schema 适配（2.x `session_v2`/`session_message`/无 `part` + 1.x 冻结表并存兼容）、状态判定升级（idle 列）、create 四家恢复（`--standalone`）。

**Architecture:** 先 D0 取证深化（改动探测，零代码）产出适配定案，再 D1 parser 双 schema 分派重写、D2 create 腿恢复、D3 门禁+实机验收。**不动注入引擎**（2.x 注入通道已 GO，定案见 `docs/research/refs/phase2-消息注入/2026-10-02-opencode2-复验定案.md`——本批 SSOT）。

**Tech Stack:** Rust（rusqlite 读路径、双 schema 分派）、复验样例库做 TDD 夹具。

**上位文档：**
- 复验定案 SSOT：`docs/research/refs/phase2-消息注入/2026-10-02-opencode2-复验定案.md`（schema 断裂清单 §4 / 锚兼容 §5 / 勘误 §6）
- AGENTS.md 扫描预算契约（opencode 属 SQLite 类豁免 L2/L3——适配后必须保持豁免形态，不得引入目录扫描）

## §0 总则

1. TDD：D1 每步先失败测试（夹具 = 双端样例库导入的 fixture db）；D0 纯取证。
2. 门禁：后端任务 `cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`；D3 六门禁全量。
3. 提交纪律：每任务至少一个 commit，全部本地不 push；D3 后停点（主线评审 → 真机验收）。
4. 零接触：真实 opencode 库一律**三件套拷贝副本**查询（WAL 必拷）；E2E 只碰自建临时目录，结束清场。
5. 冲突处置：计划与实况不符 → 停下登记回报，不自行改判。

---

### Task D0: 改动探测（取证深化，零代码，产出适配定案）

**Files:** Create `docs/research/refs/phase2-消息注入/2026-10-02-opencode2-adaptation定案.md`（本地归档）；证据入 `%USERPROFILE%\mam-probe-m6r\evidence\create-probe\oc2adapt-<run-id>\`

- [ ] **D0-1 TailSignal 语义映射取证**：旧 `part.state` 尾信号链（`get_session_tail_part`/`message_has_step_part`/`TailSignal` 消费的每个信号：tool call 在跑/完成/文本/状态快照形态）→ 在 2.x `session_message.data.content[]`（type: reasoning|tool|text + state + time）中的对应形态逐个取证。方法：对样例库（Mac `/tmp/mam-oc2-20260902-181639-xdg/` 已保留 + Windows 本机真实库副本）跑真实会话，抓「工具开始/结束/文本输出」三个时点的 data.content[] 结构样本。产出映射表。
- [ ] **D0-2 状态判定映射取证**：`idle` 消息类型（`{"outcome":"succeeded"}` 等变体全枚举）+ `session_v2.time_idle/idle_outcome/time_suspended` → MAM `SessionStatus` 枚举（绿/黄/红等待判定链）的映射表。outcome 变体不全时如实登记未观测值。
- [ ] **D0-3 `opencode session list` CLI 评估**：双平台各跑一次——输出形态（结构化?可解析?）、时延、与 db 查询结果一致性；结论：可否作发现层辅助/替代（如目录→会话列表的捷径）或维持 db 直读。**只评估不接入**。
- [ ] **D0-4 双 schema 并存取证**：1.x 冻结三表与 2.x 新表在同库的共存形态（升级后老库长什么样）；表存在性探测判据（`sqlite_master` 查 `session_v2`/`session`）；**历史 1.x 会话在 v2 读路径下的可见性**（历史卡片必须仍可读——分派策略取证：按表存在性整库分派 vs 按行混合）。用本机真实库副本 + 样例库对照。
- [ ] **D0-5 Mac 带凭据信任框复核**（create E2E 必过项 R5）：Mac 有 auth.json 环境冷启 2.x 一轮，确认首屏是否有信任/授权框（Windows 已实证无框）。
- [ ] 汇总：定案表（含 D1 实现要点确认/推翻）+ 简报（每子项一行结论 + 证据路径）。

**验收标准**：① 五个子项各有证据文件且结论可指证；② 映射表覆盖旧链全部信号（未观测值如实登记）；③ 双 schema 分派策略有明确定案建议；④ 零代码零 commit（research 归档除外）。

### Task D1: 发现层双 schema 适配

**Files:** Modify `src-tauri/src/monitor/opencode_parser.rs`（主战场）、`src/monitor/session_scan.rs`（如缓存键受影响）；Test 夹具 `src-tauri/tests/fixtures/oc2/`（从样例库导入 v2 fixture db）

- [ ] **Step 1** 导入 v2 夹具：样例库 → fixtures（含 v1 老夹具并存，命名区分）。
- [ ] **Step 2** 失败测试：① v2 库会话列表/消息读取（现路径全断——`no such table`）；② v1 夹具零回归；③ 混合库（v1 冻结表 + v2 表并存）分派正确。
- [ ] **Step 3** 实现（按 D0 定案）：
  - schema 探测分派：打开库（三件套副本口径不变）→ `sqlite_master` 查表存在性 → v2 路径 / v1 路径（分派粒度按 D0-4 定案）；
  - v2 读路径：`session_v2`/`session_message` 改名直查（列名保留）；`directory` 匹配键不变；
  - TailSignal 重写：`data.content[]` 解析替代 `part` 三查（映射按 D0-1）；
  - 状态判定：v2 走 idle 消息/idle_outcome（映射按 D0-2），v1 沿旧启发式；
  - 保持 SQLite 类豁免形态：查询即过滤，不引入目录扫描。
- [ ] **Step 4** 测试过 → 门禁 → Commit `feat(opencode2): 发现层双 schema 适配（v2 三查重写+idle 状态判定+v1 兼容）`

**验收标准**：① Step 2 三组测试全绿（v2 全通 / v1 零回归 / 混合分派）；② 真实库（副本）冒烟：本机 2.x 会话能出卡、1.x 历史会话仍可读；③ lib 测试零回归；④ clippy/fmt 绿。

### Task D2: create 的 opencode 腿恢复

**Files:** Modify `src-tauri/src/inject/resume.rs` 或 `create.rs`（起窗命令表：opencode 裸命令 → `opencode --standalone`）+ 对应测试；`src-tauri/tests/create_e2e.rs`（矩阵恢复四家）；`docs/release-notes/create-acceptance-checklist.md`（#26 翻案记录）

- [ ] 命令改 `--standalone`（一行 + 单测断言命令形态）——依据：复验定案 §2 端口竞争陷阱（bare 形态在有常驻服务时不出 TUI）+ Mac A 组实证 `--standalone` 必出 TUI。
- [ ] E2E 四家矩阵恢复 opencode 腿（真实 2.x 环境，锚=idle 前缀匹配零改动）；台账补行。
- [ ] 清单 #26 补「2.x 适配后翻案」记录行。

**验收标准**：① 四家 E2E 全过（opencode 2.x 实机）；② 命令表测试锁定 `--standalone`；③ 清单与台账同步。

### Task D3: 门禁终跑 + 实机验收 + 停点

- [ ] 六门禁全量零回归；E2E 复跑记录。
- [ ] 实机验收：① 看板出现本机 2.x opencode 会话卡（真实库）；② 历史会话（1.x 时代）仍可点开读消息；③ 手机端对 2.x opencode 会话收发一条消息全链；④ create 四家各一轮。
- [ ] Commit → **停点**：简报（任务×commit×门禁×验收表 + 自裁决/冲突登记）→ 主线评审 → 用户真机验收 → 统一 push。

**验收标准**：六门禁绿；四条实机验收全过；简报四件套齐；未 push。
