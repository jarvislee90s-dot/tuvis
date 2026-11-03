# 模型速度统计 Implementation Plan

> **历史稿，已替代**：执行[TPS短时抽样计划](2026-10-24-tps-sampling-plan.md)，不得继续实施下面旧Task1—10。

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在现有模型费用卡显示四项有证据的性能统计，缺失为横杠，独立于费用Token记账。

**Architecture:** 复用七源已有扫描与解码，旁路生成幂等性能事实及独立游标；按完整包含区间查询事实，不建性能聚合或遥测接收器。前端独立查询性能，再按原始身份与渠道拼入费用卡，保留未知模型和无费用行。

**Tech Stack:** Tauri 2、Rust、SQLite/rusqlite、React 19、TypeScript、TanStack Query、Vitest；不新增依赖。

**Spec:** [正式设计](../specs/2026-10-24-model-speed-statistics-design.md)。同时阅读[证据索引](../evidence/2026-10-24-model-speed/README.md)、native/otel/dsh-output-scope三份增补。历史快照中的“候选”不覆盖正式能力表。

> **执行暂停（2026-10-24）**：用户指出六源TPS禁用未满足核心目标，正在重新选择运行时采集/插件路线。不要实施下面的Task1—10，等待spec与计划重新确认；原任务仅保留为历史版本。

## Global Constraints

- “样本完整位于所选区间才计入；跨区间整条剔除，不截断、不分摊；不改 Token/费用账本”。判据固定 `S >= A && E < B && E <= asOf && terminal`。
- “失败、取消即使有 partial usage，仍不计 TTFT/TPS”；可靠耗时仍计模型时间，详情拆分。
- “工具调用用时：同一 callId 的结果时间减调用时间；原生执行 duration 只作证据，不能与窗口混合求和”。
- “TPS：成功、流式、计数范围对齐样本的 `1000 × Σ总生成Token / Σ(E−F)`”；平均首token按有效请求数，不扣一个Token。
- “本版禁用”项永远返回null及原因；DSH TPS必须满足pi-ai/2/openai-completions固定profile、源模型一致及多时刻非空输出。
- “首版只新增两张表，不做小时/日性能聚合”；单批最多200事实，明细沿用detailRetentionDays默认90日。
- 七源全部有状态，OpenClaw不新增采集源；不新增Hook/遥测接收器/代理/路由修改/分享模板/跨工具TPS合计。
- 修改usage域前停止开发应用或隔离数据根；测试使用私有临时连接或现有support初始化，所有源根也须隔离，不能只设置TUVIS_HOME就扫描真实HOME。
- 查询不触发扫描，不进入3秒会话轮询；COLLECT_FLIGHT持锁时不得取DB锁；新文件遵守usage自省锁的显式测试排除区格式。
- 不改MASTER-PLAN；正文/计划/说明中文，代码标识英文，UI及手册中英同步；README中文侧按当前AGENTS指定README.md、英文README.en.md处理。
- 现工作分支codex/model-speed-design。实施前成功同步main并比较契约变化；失败即停止实施，不在旧基线上悄悄编码。工作区若有用户未提交文件保持原样，不纳入提交。

## Review Focus

1. 回填/中途落库失败：性能可恢复，原Token/费用绝不重放翻倍；Task 6提供故障注入与旧游标测试。
2. DSH计数scope/紧凑流/代际复制：未知profile、单时刻及重试不产生假TPS；Task 3测试。
3. 范围、时区及保留边界：内部跨桶正常计入，外部跨界整条排除，过期范围不冒充全量；Task 7测试。
4. 费用合并/缺费用/未知模型：保留原始成员，工具分开、空值可解释，不能内连丢数据；Task 8和9测试。
5. 工具调用生命周期：多工具同消息、前置审批、原生duration不同、失败/并发都不混淆；Task 4和5测试。

---

## 文件职责与依赖

以下新目录是一个功能域，不重构整个用量系统：

| 文件                                                                                              | 职责                                | 任务   |
| ------------------------------------------------------------------------------------------------- | ----------------------------------- | ------ |
| services/usage/performance/{mod,model,policy}.rs                                                  | 值对象、能力表、逐指标准入与算术    | 1      |
| database/dao/usage_performance.rs                                                                 | 两表DAO、事务、保留、独立游标       | 2      |
| services/usage/performance/sources/dsh.rs                                                         | DSH纯事件fold及紧凑流展开           | 3      |
| services/usage/performance/sources/sqlite.rs                                                      | ZCode/OpenCode行映射及归属          | 4      |
| services/usage/performance/sources/tools.rs                                                       | Claude/Codex/Kimi/WorkBuddy工具配对 | 5      |
| services/usage/performance/collect.rs                                                             | 性能水位/回填/写入隔离与源状态      | 6      |
| services/usage/performance/query.rs                                                               | 范围、过滤、渠道、详情汇总          | 7      |
| commands/usage_performance.rs                                                                     | 只读IPC                             | 7      |
| src/{types/usagePerformance.ts,lib/api/usagePerformance.ts,lib/query/queries/usagePerformance.ts} | 前端契约与缓存                      | 8      |
| src/lib/usage/performanceRows.ts                                                                  | 费用/性能成员精确拼接               | 8      |
| src/components/usage/UsagePerformanceGrid.tsx                                                     | 常驻网格及独立性能详情              | 9      |
| tests/usage/performance-_.test.ts(x)、src-tauri/tests/usage_performance___test.rs                 | 功能与整链验收                      | 各任务 |

后端表中路径均相对于`src-tauri/src/`；下面每任务使用完整仓库相对路径。

依赖DAG：`1→2；1→3/4/5；2+3+4+5→6；1+2+6→7；7→8；8→9；6+7+8+9→10`。默认顺序1到10；只有3/4/5的纯解析可以独立实现，集成Task 6必须等三者完成。每个任务经独立验收后提交，不把下一任务的未完成代码混进提交。

### Task 1: 冻结值对象、能力表及纯计算

**Files:** Create `src-tauri/src/services/usage/performance/{mod,model,policy}.rs`；Modify `src-tauri/src/services/usage/mod.rs`。测试在新policy.rs的cfg(test)模块。

**Interfaces:** Produces `PerformanceFact`（字段逐字镜像Spec§4.1）、`PerformanceCursor`（§4.2）、`PerformanceResult/PerformanceRow/PerformanceSourceStatus`（§5）；枚举`FactKind/Outcome/PerformanceMetric/PerformanceReason`。`policy::validate(fact: &PerformanceFact) -> MetricEligibility`；`policy::within(fact: &PerformanceFact, from_ms: i64, to_ms: i64, as_of_ms: i64) -> bool`。`MetricEligibility`四个bool，`PerformanceMetric`为model/tool/ttft/tps。另定义`PerformanceAccumulator::default()`、`push(&mut self, fact: &PerformanceFact, valid: MetricEligibility) -> Result<(), UsageError>`、`metrics(&self) -> PerformanceMetrics`，只接收已经通过区间过滤的事实；PerformanceMetrics对应Spec四个显式Option字段。累计器保存每指标分子/分母及success/failed/cancelled分类，供Task7直接复用。

- [ ] **Step 1:** 新增`golden_ratios_and_failed_exclusion`、`complete_containment_edges`、`source_support_is_fixed`。断言成功S0/F2000/E7000/O500为TPS100；再加等待1000/生成1000/O50得TTFT1500、TPS=550/6；再加失败3000模型总12000而速度不变。S<A/E=B排除，内部跨日纳入。零工具窗口0有效、零生成窗TPS为空。

```rust
assert_eq!(metrics.avg_first_token_ms, Some(1500.0));
assert!((metrics.tps.unwrap() - 550.0 / 6.0).abs() < 1e-9);
assert_eq!(after_failure.model_ms, Some(12000));
assert_eq!(after_failure.tps, metrics.tps);
```

- [ ] **Step 2:** Run `cargo test --manifest-path src-tauri/Cargo.toml --lib performance::policy`；预期因新类型/函数未实现失败，而非读取真实环境。
- [ ] **Step 3:** 实现上述类型和函数；持久化整数、枚举snake_case、IPC camelCase、Option显式null。MetricEligibility严格按Spec§2/3，不能把Kimi等禁用项因存在timing字段开启；统计均值用独立有效分母，溢出拒绝贡献。
- [ ] **Step 4:** 重跑同命令，全部PASS；类型可序列化并保留provider null与空串区别。新源码加现有自省锁要求的测试排除标记。
- [ ] **Step 5:** 独立验收：能以纯数据证明四项公式、失败排除、边界与固定能力；提交`feat(usage): define performance metric contracts`。

### Task 2: 两张事实/游标表与幂等DAO

**Files:** Create `src-tauri/src/database/dao/usage_performance.rs`、`src-tauri/tests/usage_performance_ledger_test.rs`；Modify `src-tauri/src/database/{schema,migration}.rs`、`src-tauri/src/database/dao/mod.rs`。

**Interfaces:** Consumes Task 1类型。Produces `upsert_batch(conn: &mut Connection, facts: &[PerformanceFact], cursors: &[PerformanceCursor]) -> Result<(), UsageError>`、`load_cursors(conn: &Connection, source: UsageSourceId) -> Result<Vec<PerformanceCursor>, UsageError>`、`purge_before(conn: &Connection, cutoff_ms: i64) -> Result<usize, UsageError>`、`load_facts(conn: &Connection, from_ms: i64, to_ms: i64) -> Result<Vec<PerformanceFact>, UsageError>`。所有函数只用传入连接，不取全局DB。

- [ ] **Step 1:** 在`support::open_ledger_db(tag)`私有临时连接上新增`same_fact_twice_is_one`、`fact_cursor_atomic_rollback`、`terminal_update_and_conflict`、`performance_migration_leaves_token_tables_unchanged`；断言二次UPSERT数量/总耗时不增，故障后事实与对应水位一起回滚，null/0往返原样，旧Token表四桶/请求/游标逐项不变。

```rust
assert_eq!(facts_after_second, facts_after_first);
assert_eq!(token_snapshot_after, token_snapshot_before);
assert_eq!(cursor_after_failed_batch, cursor_before_batch);
```

- [ ] **Step 2:** Run `cargo test --manifest-path src-tauri/Cargo.toml --test usage_performance_ledger_test`，预期缺表/DAO失败。
- [ ] **Step 3:** 按Spec§4建两表和三索引。合法inflight→terminal/缺字段补全可更新；冲突终态保留诊断，清空冲突指标贡献而非累加。批次事实≤200，事实+该批独立水位同事务；每批只推进至该批已提交的完整记录边界，不能提前写最终offset。源级回填状态存`file_key=@source-state`的cursor.state_json，不新增第三表。load_facts加载与区间相交或单边已知时间在区间内的事实，不能提前只选完整包含而丢掉排除/缺失计数。
- [ ] **Step 4:** 重跑测试PASS；补`retention_purges_only_performance`，按ended_at清理终态、按started_at清理过期未闭合事实，源状态不删除。新建/升级两次均幂等。
- [ ] **Step 5:** 独立验收：失败批可续跑、稳定键不重复、旧账本完全不变；提交`feat(usage): persist isolated performance facts`。

### Task 3: DSH纯解析及固定TPS profile

**Files:** Create `src-tauri/src/services/usage/performance/sources/{mod,dsh}.rs`；脱敏真实形态夹具放`src-tauri/tests/fixtures/performance/dsh-*.jsonl`。本任务不接真实扫描入口。

**Interfaces:** Consumes Task 1。Produces可serde的`DshPerformanceFold`，方法`new(session_id: &str)`、`accept(&mut self, event: &serde_json::Value)`、`drain(&mut self) -> Vec<PerformanceFact>`；未闭合状态可存入性能cursor.state_json。

- [ ] **Step 1:** 测试`compact_dt_nonempty_first_output`、`only_verified_profile_allows_tps`、`retry_and_generation_replay_do_not_inflate`。断言time0=2000、dt=[0,1000]逐成员展开，首空成员跳过；pi-ai/2/openai-completions且source一致才允许总output分子；profile缺失/错版本/单时刻输出/零窗/attempt/retry均TPS=null；只有TTFT合格不强迫TPS合格。

```rust
assert!(policy::validate(&verified_profile_fact).tps);
assert!(!policy::validate(&unknown_profile_fact).tps);
assert!(!policy::validate(&retried_fact).tps);
```

- [ ] **Step 2:** Run `cargo test --manifest-path src-tauri/Cargo.toml --lib performance::sources::dsh`，预期fold缺失失败。
- [ ] **Step 3:** 实现fold，按session+turn+step与callId稳定去重，source模型取消息；模型端点取模型结算事件，不用step/end。若先有失败attempt后成功，逻辑模型窗口只结算一次；没有成功且step关闭，则以最后可验证失败attempt结束作失败窗口。仅step/end无模型端点时贡献为空。
- [ ] **Step 4:** 重跑PASS；补`same_step_conflict_is_excluded`、`call_result_uses_issuer_model`、`out_of_order_partial_frame_is_retained`。在模型终结前不得drain已失败attempt为最终贡献，否则后续成功会重复。
- [ ] **Step 5:** 独立验收：profile正例实际有非null TPS，反例为null且有正确原因；代际副本不增时长；提交`feat(usage): parse DSH performance evidence`。

### Task 4: ZCode与OpenCode数据库映射

**Files:** Create `src-tauri/src/services/usage/performance/sources/sqlite.rs`、`src-tauri/tests/usage_performance_sqlite_test.rs`；Modify各collector的行投影在Task 6统一接入，本任务提供纯映射及私有库测试。

**Interfaces:** Produces `zcode_model(row: &rusqlite::Row<'_>) -> rusqlite::Result<PerformanceFact>`、`zcode_tool(row: &rusqlite::Row<'_>) -> rusqlite::Result<PerformanceFact>`、`opencode_tools(session_id: &str, message: &serde_json::Value) -> Vec<PerformanceFact>`。SQL列名使用Spec中的原生列；映射读取列名而非易漂移的数字位置。

- [ ] **Step 1:** 测试`zcode_failed_time_and_no_tool_ttft`、`zcode_native_duration_does_not_replace_window`、`zcode_ambiguous_model_is_unknown`、`opencode_v2_prevents_duplicate_v1`。断言ZCode窗口10000、native6000工具贡献10000；retry_count>0/attempt_index>0/tool_call_count>0使TTFT无效；ZCode TPS始终null。OpenCode包含工具的消息总时长不进入模型时间。

```rust
assert_eq!(zcode_tool_fact.tool_window_ms, Some(10000));
assert_eq!(zcode_tool_fact.native_duration_ms, Some(6000));
assert!(!policy::validate(&zcode_model_fact).tps);
```

- [ ] **Step 2:** Run `cargo test --manifest-path src-tauri/Cargo.toml --test usage_performance_sqlite_test`，预期映射未实现失败。
- [ ] **Step 3:** 按Spec§3.1/3.3实现映射；ZCode工具以`part.data`的`$.callID`与tool_call_id精确连接，part.message_id回message及model_usage.assistant_message_id，所有边唯一才赋模型，否则未知；保持provider三态。retry_count字段缺失不当0，版本不匹配按禁用速度。OpenCode有v2表就不再采v1镜像，每个content工具单独窗口。SQL fixture含running原地UPDATE后completed。
- [ ] **Step 4:** 重跑PASS；验证未终态重读后原键替换、缺起止不补0、不使用immutable忽略WAL。
- [ ] **Step 5:** 独立验收：两个schema路径均有测试，旧镜像不双计、原生/窗口不混算；提交`feat(usage): map SQLite performance windows`。

### Task 5: 其余四源的工具窗口与固定降级

**Files:** Create `src-tauri/src/services/usage/performance/sources/tools.rs`、`src-tauri/tests/usage_performance_tools_test.rs`；夹具`src-tauri/tests/fixtures/performance/{claude,codex,kimi,workbuddy}-tools.jsonl`。

**Interfaces:** Produces可serde的`ToolWindowFold`，`new(source: UsageSourceId, session_id: &str)`、`accept(&mut self, record: &serde_json::Value)`、`drain(&mut self) -> Vec<PerformanceFact>`。每源映射规则在Spec§3.3，不在此添加模型时间/速度的新推导。

- [ ] **Step 1:** 测试`claude_multiple_tools_one_message`、`codex_new_old_same_call_one_window`、`kimi_preapproval_outside_call_not_invented`、`workbuddy_pairs_call_id_not_record_id`。Claude一条assistant两工具得到两窗口；Codex相同call新旧投影只一条；Kimicall100/result300/native100得到窗口200；WB结果行id不同仍以callId匹配，未返回调用不补0。

```rust
assert_eq!(claude_facts.len(), 2);
assert_eq!(codex_deduplicated_facts.len(), 1);
assert_eq!(kimi_fact.tool_window_ms, Some(200));
```

- [ ] **Step 2:** Run `cargo test --manifest-path src-tauri/Cargo.toml --test usage_performance_tools_test`，预期fold未实现失败。
- [ ] **Step 3:** 实现四源adapter和统一pending配对状态；Kimi遍历所有agents文件，模型仅同agent/turnStep唯一请求；Claude逐tool_use_id；Codex调用发起时context；WB调用providerData.model。缺模型保留工具事实空model，失败工具有完整窗口也计入。
- [ ] **Step 4:** 重跑PASS；添加四源禁用模型/TTFT/TPS字段为null、两并行工具各10秒合计20秒、重复记录不增、缺callId只诊断测试。
- [ ] **Step 5:** 独立验收：四源各有正工具窗口和明确禁用指标，互不串会话/agent；提交`feat(usage): collect remaining tool call windows`。

### Task 6: 一遍扫描、独立回填与写入隔离

**Files:** Create `src-tauri/src/services/usage/performance/collect.rs`、`src-tauri/tests/usage_performance_collection_test.rs`；Modify `services/usage/{delta,collect,ledger}.rs`及七个collector、`performance/mod.rs`。只在确需适配只读事务的DAO处改动，禁止重构整个旧账本。

**Interfaces:** SourceDelta新增`performance_facts: Vec<PerformanceFact>`及`performance_cursors: Vec<PerformanceCursor>`；Produces `performance::collect::apply(source: UsageSourceId, facts: &[PerformanceFact], cursors: &[PerformanceCursor]) -> Result<(), UsageError>`、`resume_backfill(now_ms: i64) -> Result<(), UsageError>`。Task 2 DAO负责批事务。

- [ ] **Step 1:** 用全部七源隔离fixture写`two_rounds_preserve_token_and_performance`、`performance_failure_never_replays_token`、`old_token_cursor_is_not_performance_watermark`、`budget_resume_does_not_double_count`、`retention_30_7_30_requeues_only_performance`。第一轮每源确有数据；第二轮Token四桶、requests、旧cursor及性能总数/时长都不增长；失败后只重试性能。保留期30→7→30时，源存在恢复第8—30天性能，源已删则报告不可恢复，两支Token快照均不变。

```rust
assert_eq!(token_snapshot_round2, token_snapshot_round1);
assert_eq!(performance_snapshot_round2, performance_snapshot_round1);
assert_eq!(token_snapshot_after_perf_retry, token_snapshot_round1);
```

- [ ] **Step 2:** Run `cargo test --manifest-path src-tauri/Cargo.toml --test usage_performance_collection_test`，预期SourceDelta及独立状态缺失失败。
- [ ] **Step 3:** 七源已有事件/SQL循环旁路调用Task3/4/5，不为每指标另读文件。统一读取从Token/性能所需较早位置开始，两套emit水位分别门控；性能历史区间不再emit旧Token。正常旧账本成功提交后才提交性能，性能失败单独source status，不回退已成功Token游标或再次调用旧apply。Token失败时性能水位不越过该轮未消费记录。
- [ ] **Step 4:** 独立性能回填仅保留期内、每后台批10MiB读取预算，事实≤200一事务；完整记录/压缩帧边界续读。超过预算单体记source失败及原因，不无界解压或永久自旋。@source-state按Spec固定保存retainedFromMs/coveredFromMs/backfillRevision/state/errorCode/processedCount/unrecoverableRanges；缩短保留期推进coverage，扩大时新增历史窗口置pending并递增revision，仅重置性能读取水位；已删除历史记录fromMs/toMs不可恢复范围而非无限重试。新源码添加自省锁标记与静态锁覆盖，不能删除断言逃避。
- [ ] **Step 5:** 重跑PASS，再运行`cargo test --manifest-path src-tauri/Cargo.toml --test usage_two_round_incremental_test --test usage_ledger_test --test real_db_isolation_test`。独立验收：单文件一遍、部分失败不改旧账、重启续读等价一次成功；提交`feat(usage): integrate resumable performance collection`。

### Task 7: 精确区间查询、渠道与只读IPC

**Files:** Create `src-tauri/src/services/usage/performance/query.rs`、`src-tauri/src/commands/usage_performance.rs`、`src-tauri/tests/usage_performance_query_test.rs`、`src-tauri/tests/usage_performance_ipc_test.rs`；Modify `commands/mod.rs`、`src-tauri/src/lib.rs`、`services/usage/v2/{model,cost_query}.rs`。

**Interfaces:** Produces `query::query_with_conn(conn: &Connection, range: &UsageRange, filters: &UsageFilters, now_ms: i64) -> Result<PerformanceResult, UsageError>`、`query::query(range: &UsageRange, filters: &UsageFilters, now_ms: i64) -> Result<PerformanceResult, UsageError>`、IPC `usage_performance(range: UsageRange, filters: UsageFilters) -> Result<PerformanceResult, UsageError>`。CostRow新增`performance_identities: Vec<IdentityKey>`，IPC为performanceIdentities；PerformanceRow附channelKey。

- [ ] **Step 1:** 测试`complete_range_not_completion_bucket`、`retention_crossing_is_null`、`issuer_unknown_and_channel_conflict`、`missing_times_have_no_fake_current_time`、`ipc_is_read_only`。内部跨小时成功计入；S<A/E=B剔除且crossBoundary计数；旧保留范围四项null；disabled不扫描；失败3000仅模型时间。null provider/空串分别匹配。

```rust
assert_eq!(result.rows[0].counts.cross_boundary, 1);
assert_eq!(expired_result.rows[0].metrics.tps, None);
assert_eq!(collect_calls_after_query, collect_calls_before_query);
```

- [ ] **Step 2:** Run `cargo test --manifest-path src-tauri/Cargo.toml --test usage_performance_query_test --test usage_performance_ipc_test`，预期查询/命令未注册失败。
- [ ] **Step 3:** 复用现有range、filters、渠道/别名与父子守卫；只读快照按Spec§5聚合所有明细计数，四指标用Task1 PerformanceAccumulator，诊断计数在包含过滤之前分流。渠道映射不唯一归ambiguous。保留期不足整段null；@source-state补回填状态；每个源给metricSupport/readState/reasons。CostRow分组时保留原始成员，不改既有key/费用/币种结果；性能无费用也返回。
- [ ] **Step 4:** 重跑PASS，另跑`cargo test --manifest-path src-tauri/Cargo.toml --test usage_cost_test --test usage_ipc_test`；断言命令async+spawn_blocking、不持飞行锁、不调用collect，不改变现有价格/导出。
- [ ] **Step 5:** 独立验收：Spec§5全部响应字段存在，性能与费用范围差异明确，禁用源有null原因；提交`feat(usage): expose bounded performance queries`。

### Task 8: 前端类型、查询与费用成员拼接

**Files:** Create `src/types/usagePerformance.ts`、`src/lib/api/usagePerformance.ts`、`src/lib/query/queries/usagePerformance.ts`、`src/lib/usage/performanceRows.ts`、`tests/usage/performance-api.test.ts`、`tests/usage/performance-rows.test.ts`；Modify `src/types/usageV2.ts`、`src/tauri-mock.ts`及现有`tests/msw/handlers.ts`对应mock。

**Interfaces:** `usagePerformance(range: UsageRange, filters: UsageFilters): Promise<PerformanceResult>`；`useUsagePerformanceQuery(range: UsageRange, filters: UsageFilters)`；`attachPerformance(costs: CostRow[], result: PerformanceResult|null, queryState: PerformanceQueryState): PerformanceCardModel[]`。PerformanceQueryState为loading/error/ready。PerformanceCardModel含`cost: CostRow|null`、原始identity、channelKey和`perTool: PerformanceToolModel[]`；PerformanceToolModel含`sourceId/row: PerformanceRow|null/sourceStatus: PerformanceSourceStatus|null/backfillState/queryState`，null源状态只允许结果尚不可用时。类型逐字镜像Task1/7 camelCase，不另造sourceId枚举。

- [ ] **Step 1:** 测试invoke参数为`{range,filters}`、key以`['usage','v2','performance']`开头且含完整range/filters；`nullIsNotZero`、`unknownAndUnpricedRemainVisible`、`mergedCostRowKeepsToolsSeparate`、`sameNameDifferentProviderNotJoined`、`emptyRowsKeepDistinctSourceStates`。最后一例result.rows=[]，分别输入unsupported/failed/absent/backfill-running及query-error，费用卡网格保留匹配状态而非统一无数据。

```ts
expect(invokeMock).toHaveBeenCalledWith("usage_performance", { range, filters });
expect(joinedUnknown.cost).toBeNull();
expect(joinedMerged.perTool).toHaveLength(2);
```

- [ ] **Step 2:** Run `pnpm test -- tests/usage/performance-api.test.ts tests/usage/performance-rows.test.ts`，预期新模块缺失失败。
- [ ] **Step 3:** 实现类型、包装、缓存及拼接；复用USAGE_KEY失效，refetchInterval=false，挂载/范围变化查询，不启动采集。按channelKey及performanceIdentities匹配；同身份多个价格分片可重复显示同一性能但无合计。未知/无费用性能形成cost=null卡；费用有而性能无则perTool指标全部null及原因。mock提供正/空/失败三种状态。
- [ ] **Step 4:** 重跑PASS，`pnpm build`通过；旧CostRow测试夹具同步新增成员字段，不用可选字段掩盖后端缺字段。
- [ ] **Step 5:** 独立验收：不按显示名猜归属、不丢无费用数据、不生成跨工具TPS；提交`feat(usage): join performance by raw identity`。

### Task 9: 四项常驻网格、失败详情与本地化

**Files:** Create `src/components/usage/UsagePerformanceGrid.tsx`、`tests/usage/performance-grid.test.tsx`；Modify `src/components/usage/{UsageCostCard,UsageCostTable}.tsx`、`src/i18n/locales/{zh,en}.json`。

**Interfaces:** `UsagePerformanceGrid({model,t}: {model: PerformanceToolModel; t: TFn})`；UsageCostCard查询Task8结果并将PerformanceCardModel传入表格，不能在每卡再次invoke；旧计费详情折叠保持原行为。

- [ ] **Step 1:** 测试`fourMetricsVisibleWithBillingDetailsClosed`、`failureBreakdownOnlyInPerformanceDetails`、`dashZeroAndSampleCounts`、`multiToolNoCombinedTps`、`performanceOnlyUnknownCard`、`englishLabelsAndReasons`、`emptyRowsShowSourceFailureOrBackfillState`。断言四项顺序、TPS100→100.0 tok/s、合法工具0显示0而null为—，失败3000只在详情，禁用项解释unsupported-timing；row=null仍正确展示源失败/未支持/无源/回填状态，query-error不遮盖费用。

```tsx
expect(screen.getByText("模型用时")).toBeVisible();
expect(screen.getByText("平均首 token")).toBeVisible();
expect(screen.getByText("100.0 tok/s")).toBeVisible();
```

- [ ] **Step 2:** Run `pnpm test -- tests/usage/performance-grid.test.tsx`，预期组件/文案缺失失败。
- [ ] **Step 3:** 实现2×2网格与独立性能详情，复用fmtDur/主题/tabular-nums；同卡多工具逐行网格；常驻标注累计时长可能重叠、完整样本与费用范围不同、相同价格分片为模型区间性能。性能错误只影响网格，费用原值保留。新增zh/en同键同义文案，不放实现术语进主流程。
- [ ] **Step 4:** 重跑PASS，再Run `pnpm check:i18n`和现有`tests/usage/v2-cost-table.test.tsx`、`v2-cost-card.test.tsx`、`v2-page.test.tsx`。窄屏无横向滚动，键盘能开性能详情；价格关闭/缺价仍可看性能，不以定价enabled决定性能可见性。
- [ ] **Step 5:** 独立验收：四项始终可见、失败不污染速度、旧费用/计费折叠/分享模板不回归；提交`feat(usage): show model performance alongside costs`。

### Task 10: 整链验收、说明与交付记录

**Files:** Create `src-tauri/tests/usage_performance_acceptance_test.rs`、`tests/usage/performance-acceptance.test.tsx`；Modify `docs/product/manual/{zh,en}/11-usage.md`、`CHANGELOG.md`、`CHANGELOG.en.md`，README.md/README.en.md仅各一句功能入口说明；不修改MASTER-PLAN。

**Interfaces:** 消费Task1—9既有接口，不新增运行功能。产出分支实施报告`docs/engineering/evidence/reports/2026-10-24-model-speed-implementation.md`，列Task完成情况、提交、命令/退出结果、已知缺口和真实数据隔离证明。

- [ ] **Step 1:** 增加端到端影子账本用例：七源各有事实或明确禁用；DSH命中profile正例TPS非空，未知profile为空；一条跨界被排除；重复采集原Token不变；未知模型/无费用在UI可见；失败速度不变；清理后旧范围为空。先运行新增测试，确认失败来自整链未闭合而非空fixture或错误命令。

```rust
assert_eq!(token_snapshot_after_backfill, token_snapshot_before_backfill);
assert!(verified_dsh_row.metrics.tps.is_some());
assert!(unknown_profile_row.metrics.tps.is_none());
```

- [ ] **Step 2:** 仅修复上述整链连接缺口，不扩大来源能力表；新fixture必须由脱敏真实结构提取并保存来源形态说明，不复制会话正文/密钥。
- [ ] **Step 3:** 按Spec§7逐项填验收结果；运行`pnpm check`、`pnpm test -- tests/usage tests/pet/usage-worksummary.test.tsx`、`cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check`、`cargo test --manifest-path src-tauri/Cargo.toml --test usage_performance_acceptance_test`、`cargo test --manifest-path src-tauri/Cargo.toml --lib services::usage`及Task6/7回归套件、`cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`。记录完整命令及结果；已有基线失败须单独报告，不能称全部通过。
- [ ] **Step 4:** 更新双语手册和Unreleased账本，说明当前准入范围、TTFT/TPS独立样本、失败详情、完整区间、未知模型/无费用、保留期、禁用源原因；README只一句带手册链接。禁止宣称七源四项全支持或服务端解码速度。
- [ ] **Step 5:** 最后独立审查diff及全部验收结果；提交`test(usage): verify performance isolation and document behavior`。交付报告含最终HEAD、无未完成Task、不上传真实数据；不发版、不打tag、不自行合并或部署。

## Spec覆盖与顺序自查

| Spec                      | 实现任务       | 验收锚点                                   |
| ------------------------- | -------------- | ------------------------------------------ |
| §1四项和范围              | 1、8、9        | 固定能力、常驻网格、无新增采集路由         |
| §2公式/失败/完整区间      | 1、7           | golden_ratios、complete_range、failure详情 |
| §3逐源、scope、重试、归属 | 3、4、5        | 每源正例和禁用反例，未知profile/null       |
| §4两表、去重、保留、回填  | 2、6           | rollback、two_rounds、retention、旧游标    |
| §5接口/筛选/渠道/成员     | 7、8           | IPC只读、原始身份、缺字段/跨范围           |
| §6UI与双语                | 8、9、10       | null/0、多工具、未知/无费用、手册          |
| §7全部验收                | 10及各归属任务 | 逐行对照，无“后续决定”                     |

作者自查必须确认：每个Produces都有唯一任务定义、后继只消费前置产物；Task3/4/5不提前改主采集；Task6不遗漏七源；Task7只在写入/水位完成后依赖真实查询；Task8/9使用同一camelCase字段；所有Review Focus均有具体测试；最终不把仍禁用的速度说成兼容。执行者按Task提交及报告，不在本计划范围启动产品发布。
