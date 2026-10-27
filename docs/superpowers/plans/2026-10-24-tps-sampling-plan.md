# TPS短时抽样 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 交付独立于Token区间的近期TPS抽样曲线、最近完成调用及当前回合有效样本，不再等待七工具全量探测。

**Architecture:** 独立services/tps只读既有来源/本机遥测/观察插件，保存24小时最小事实与实际采样点；固定120秒取样、后台600秒/可见60秒出点。旧usage账本及费用查询保持原样。

**Tech Stack:** 现有Tauri2/Rust/SQLite/React19/TypeScript；复用现有图表，不引入Grafana平台或测速网关。

**Spec:** [TPS抽样收口设计](../specs/2026-10-24-tps-sampling-design.md)。旧2026-10-24-model-speed-statistics-plan.md继续作为暂停历史，不交叉执行。

## Global Constraints

- Token/TPS区间独立；TPS范围仅2m/10m/1h/today，默认1h。
- 每点观察前120秒完成调用，每series最多20条；ΣToken/Σ生成时间；调用可跨窗口开始，不切Token。
- 后台600秒、可见读取5秒/出点60秒、manual最小5秒；view心跳30秒、租约90秒；仅保留24小时，不回补伪装实时的历史点。
- 成功/正生成/同调用计数才入速度；不全局用1秒门槛，不以回合墙钟、字符估算或不对齐histogram凑值。
- 统计设置由软件完成，API/认证不变；保留既有目标，原生确认用户点；不主动重启、不改安装包、不新增系统守护进程。
- 旧Token表/游标/采集节奏不变；修改usage共用模块前停dev或隔离所有源根/影子账本，测试不写真实数据。
- 实施前同步main并保留用户未提交文件；分支codex/，worktree只在Documents下；不得改MASTER-PLAN或发布合并。

## Review Focus

1. 空窗/旧数据：gap不画0，超过10分钟不当当前值；Task1/8。
2. 重复点与重叠样本：同点一次，不对点均值求全量均值；Task1/2。
3. 观察器错误：AsyncIterable/Effect适配正确、ID不丢、不吞错；Task6。
4. 采集成本与退出：5秒只读增量、后台600秒、lease过期；Task7。
5. 原生近似与缺证据：方法标注，Codex不造TPS，WinWB不混旧CLI；Task3/4/5。

---

## 文件与顺序

新增`src-tauri/src/services/tps/{mod,model,policy,store,scheduler,query,install,collector}.rs`与`sources/{dsh,zcode,kimi,workbuddy,claude,opencode,codex}.rs`；DAO位于`database/dao/tps.rs`；IPC在`commands/tps.rs`。前端新增`src/types/tps.ts`、`src/lib/{api/tps,query/queries/tps}.ts`、`src/components/usage/TpsCard.tsx`、`src/lib/usage/tpsSeries.ts`。观察插件独立放`resources/tps/opencode/`并分发，不改Harness安装包。

依赖：1→2；1→3/4；1+2→5/6；2+3+4+5→7；7→8；7+8→9，Task6的已实现适配由7注册但不作为原生调度的编译前置。默认顺序1—9；Task6遇版本问题提交明确needs-adapter状态后继续，不反复要求用户探测。原生来源先可交付，OC/WinWB不成功只能标该适配未通过，不能冒充支持或阻断已成功来源的验收。

### Task 1: 值对象、有效性与点计算

**Files:** Create `src-tauri/src/services/tps/{mod,model,policy}.rs`；Modify `services/mod.rs`；unit tests在policy.rs。
**Interfaces:** `TpsObservation/TpsPoint/TpsSelection/TpsSnapshot/SourceState/SourceCursor/ReadBatch`对应Spec§6；`build_point(observations: &[TpsObservation], known_series: &[SeriesKey], run_id: &str, at_ms: i64) -> Vec<TpsPoint>`；`snapshot(observations: &[TpsObservation], selection: &TpsSelection, now_ms: i64) -> TpsSnapshot`。PointStatus为Value/Gap，PointTrigger为Startup/Scheduled/Manual；空窗必须仍有known_series对应gap。

- [ ] Step1 写红测试：同series取最近20条；500Token/5秒及50/1秒得550/6；失败100Token/1秒不改变；正原生806ms不因小于1秒丢弃；零窗为空。

```rust
assert!((point.tps.unwrap()-550.0/6.0).abs()<1e-9);
assert_eq!(point.sample_count,2);
assert_eq!(empty.status, PointStatus::Gap);
```

- [ ] Step2 Run `cargo test --manifest-path src-tauri/Cargo.toml --lib services::tps::policy`，预期新接口缺失FAIL。
- [ ] Step3 实现固定方法/输出scope/purpose/outcome枚举、稳定series key；取end在(at−120000,at]且正窗样本；跨开始边界纳入整条。TPS取最近20个有效；同读取批次可靠失败/取消耗时另入样本modelMs及分类数，各有效分母按Spec独立。当前回合依原生turn，method不同不混。
- [ ] Step4 重跑PASS；补重复key、未知turn、超过10分钟snapshot为空及age、source/provider null区别断言。
- [ ] Step5 验收公式与gap后提交`feat(tps): define sampled throughput semantics`。

### Task 2: 三表与24小时保留

**Files:** Create `src-tauri/src/database/dao/tps.rs`、`src-tauri/src/services/tps/store.rs`、`src-tauri/tests/tps_store_test.rs`；Modify schema/migration/dao mod。
**Interfaces:** `store::commit_batch(conn: &mut Connection, batch: &ReadBatch) -> Result<(), UsageError>`；`save_points(conn: &mut Connection, points: &[TpsPoint]) -> Result<(), UsageError>`；`load_recent(conn: &Connection, since_ms: i64) -> Result<Vec<TpsObservation>, UsageError>`；`purge(conn: &Connection, now_ms: i64) -> Result<usize,UsageError>`；state按key保存JSON。commit_batch原子写事实/nextCursor/sourceState。

- [ ] Step1 写红测试：事实重复UPSERT不增，点唯一，gap保留，24小时删除，原usage_*四桶/费用/cursor不变。

```rust
assert_eq!(after_twice,before_second);
assert_eq!(old_usage_after,old_usage_before);
```

- [ ] Step2 Run `cargo test --manifest-path src-tauri/Cargo.toml --test tps_store_test`，FAIL。
- [ ] Step3 建Spec三表与source/ended_at、sampled_at/series索引；每事务≤200；只保存元数据；安装记录与native游标在tps_state，不借旧usage_cursor。
- [ ] Step4 重跑PASS，注入批失败验证原子性；恢复后只补事实，不生成过去缺失的采样点。
- [ ] Step5 验收幂等/保留/隔离并提交`feat(tps): store observations and actual sample points`。

### Task 3: DSH与ZCode原生读取

**Files:** Create `services/tps/sources/{dsh,zcode}.rs`、`src-tauri/tests/tps_native_primary_test.rs`。
**Interfaces:** 每source `read_incremental(home: &Path, cursor: &SourceCursor, now_ms: i64) -> Result<ReadBatch, UsageError>`；只读源，不提交Tuvis账；读取不先改已提交cursor，结果内带完整sourceState/diagnostics/nextCursor。

- [ ] Step1 用实际脱敏结构写红测试：DSH紧凑dt首空跳过、思考计入、同消息Token；ZCode缺F/失败/retry跳过速度；重复代际一次。

```rust
assert_eq!(zcode_missing_f.tps(),None);
assert_eq!(dsh_replayed.len(),dsh_once.len());
```

- [ ] Step2 Run `cargo test --manifest-path src-tauri/Cargo.toml --test tps_native_primary_test`，FAIL。
- [ ] Step3 DSH用模型终结而非含工具step/end；ZCode native F→completed、只读WAL，SQL用rowid尾200而非COUNT全表轮询；阶段字段到同call，保留原生边界说明。
- [ ] Step4 重跑PASS；DSH最后成功attempt可对齐才用，否则剔除；读取只新增，不每5秒解完整历史；字段缺失返回诊断不报0。
- [ ] Step5 验收两源各有正例点与失败反例后提交`feat(tps): read DSH and ZCode samples`。

### Task 4: Kimi与WorkBuddy原生读取及缺口状态

**Files:** Create `services/tps/sources/{kimi,workbuddy,codex}.rs`、`src-tauri/tests/tps_native_secondary_test.rs`。
**Interfaces:** 沿用Task3 read_incremental签名；Codex返回observations为空且sourceState=insufficient-generation-timing的ReadBatch，不造观察值。

- [ ] Step1 红测试：Kimi嵌套step事件的timing+usage同条，806ms正窗有效；WB qua.end同请求成功，标题purpose不入主曲线；Windows只有hasUsage=true不计；Codexturn−TTFT不能产TPS。

```rust
assert!(kimi_subsecond.tps().is_some());
assert!(codex_turn_only.is_empty());
```

- [ ] Step2 Run `cargo test --manifest-path src-tauri/Cargo.toml --test tps_native_secondary_test`，FAIL。
- [ ] Step3 原生native计时不要求逐token轨迹；取消/无usage跳过；provider-reported思考未声明留元数据，不加内容估算。WB当前路由无数值usage则缺口，不能借别的旧CLI调用；内部retry不可识别标限制。
- [ ] Step4 重跑PASS，Mac5.8.3真实结构和Windows路由缺字段分别验收；残缺尾行重读不丢。
- [ ] Step5 验收源状态清晰、不扩大支持并提交`feat(tps): read native Kimi and WorkBuddy timings`。

### Task 5: Claude本机遥测与一次统计设置

**Files:** Create `services/tps/{install.rs,sources/claude.rs}`、`src-tauri/tests/tps_claude_ingest_test.rs`；仅安装/接收逻辑测试，不写用户真实settings。
**Interfaces:** `install::preview(tool: &str) -> Result<InstallPreview, UsageError>`、`apply(preview_id: &str, choice: InstallChoice) -> Result<InstallReceipt, UsageError>`、`uninstall(tool: &str) -> Result<(), UsageError>`；InstallChoice=AppendOnly/TransferStatisticsTarget；`claude::decode_trace(body: &[u8]) -> Result<Vec<TpsObservation>, UsageError>`。InstallReceipt逐键保存oldExists/oldValue/installedValue/原指纹，仅私有存储。

- [ ] Step1 红测试：Mac四span原始dur90136/out3709，原生ttft口径复算42.279而非54.8；产品近似口径用first_content且标method，不混基线；attempt>1排除速度；接收超限/无token拒绝。

```rust
assert_eq!(observation.method,TimingMethod::NativeEventApprox);
assert_eq!(retried.tps(),None);
```

- [ ] Step2 Run `cargo test --manifest-path src-tauri/Cargo.toml --test tps_claude_ingest_test`，FAIL。
- [ ] Step3 本地/v1/traces只读OTLP JSON元数据，1MiB、loopback、Bearer、固定持久端口；解析同span/trace/attempt，工具span独立。安装preview展示统计键与原值指纹，apply只写用户确认自有键；旧目标冲突保留，API/auth不碰。
- [ ] Step4 重跑PASS：已有目标→明确转交→卸载恢复原目标，原先不存在则删除自有项；用户后改/预览过期则不覆盖。原配置其它字段/注释/未知项保留；旧trace重复不增；工具间隙不进生成窗；首块开始不能标严格非空delta。
- [ ] Step5 一条影子JSON正例走入采样点、一次配置预览/应用/冲突/卸载测试通过后提交`feat(tps): ingest Claude native timing safely`。

### Task 6: OpenCode最小扩展（正常编码任务，不再要求用户全面探测）

**Files:** Create `resources/tps/opencode/`（观察器JS、匹配宿主的manifest及许可证）、`services/tps/sources/opencode.rs`、`tests/tps/opencode-observer.test.ts`；Modify `services/tps/install.rs`嵌入分发资源，扩展安装复用Task5所有权机制。
**Interfaces:** 扩展输出`{eventKey,sessionKey,messageKey,ordinal,attempt,type,created,started,tokens,finish,hasNonEmptyDelta,deltaChars}`；ID分别稳定hash，不截前8位，不保存正文；Rust侧read_incremental沿用Task3。

- [ ] Step1 红测试：标准AsyncIterable自身无next仍能消费、订阅失败有诊断、两会话/两message交错不串、空delta不设F、重复event幂等、累计usage不再累加。

```ts
expect(observed.map((e) => e.eventKey)).toEqual(["e1", "e2"]);
expect(firstNonEmpty).toBe(2000);
```

- [ ] Step2 Run `pnpm test -- tests/tps/opencode-observer.test.ts`，FAIL。
- [ ] Step3 对实际宿主2.0.22/2.1.2核对公开SDK，选定对应Effect Stream或AsyncIterable适配；不凭残留v1类型猜接口。参考ChiR24/opencode-tps-meter的server/meter生命周期、事件分派与结束校准，锁commit并保留真实MIT许可；不整套引入UI、字符估算或未经实现的retry处理。资源以include_str!/include_bytes!嵌入Tuvis并写入自有插件目录，不能仅开发仓库可用；断言分发包具有可加载JS/manifest/许可，必要的宿主匹配依赖仅在插件自有目录，不改用户其他依赖。
- [ ] Step4 正确消费迭代器并诊断错误，不静默吞错；保存独立session/message/ordinal，已识别不明retry剔除。单个官方观察插件自动/点击安装，正常服务下真实一条开始→delta→结束为扩展支持验收，不能loaded就算过。不能成功时交明确适配失败，不反推用户账号/宿主永远不支持。
- [ ] Step5 单元PASS及一条正常宿主正例/或明确未通过报告后提交该任务；标支持须有正例，未通过状态不能冒充全计划适配成功。原生来源可先交付，不另开用户七工具探测。

### Task 7: 调度、查询、视图租约与IPC

**Files:** Create `services/tps/{scheduler,collector,query}.rs`、`commands/tps.rs`、`src-tauri/tests/tps_sampling_test.rs`；Modify services/mod.rs、commands/mod.rs、lib.rs仅注册新服务。
**Interfaces:** `Sampler::tick(now_ms: i64, visible: bool)`、`heartbeat(view_id: &str, visible: bool, now_ms: i64)`；IPC逐字实现Spec§6七命令，selection字段tool/model/channel/method，view_range为2m/10m/1h/today。

- [ ] Step1 假时钟红测试：后台1小时6个计划点，可见60秒点/5秒增量，租约90秒过期恢复后台，manual5秒限频，120秒点内最多20调用；Token区间切换无影响。

```rust
assert_eq!(scheduled_points_in_hour,6);
assert_eq!(after_lease_expiry.mode,SamplerMode::Background);
```

- [ ] Step2 Run `cargo test --manifest-path src-tauri/Cargo.toml --test tps_sampling_test`，FAIL。
- [ ] Step3 跨源单飞/隔离失败，ReadBatch成功落库才推进cursor；最近事实可补不伪补过去点，后台不跑5秒timer；复用usage.enabled总开关，关闭停采集和collector、查询返回disabled但不删历史，重开新runId；查询只读不扫描。源status分别保留未配置/待重启/无样本/字段不足/有效。known_series来自近24h存储及当前用户选择，空窗仍产gap；新runId或gap处曲线断线。Task6未通过时注册needs-adapter占位状态，不引用缺失模块阻断原生来源。
- [ ] Step4 重跑PASS；故障注入验旧usage_*没写；已退出视图不能永远维持前台；SQLite只尾查，文件读取预算有界；IPC与API错误一致。
- [ ] Step5 验收资源节奏、真实点、源状态后提交`feat(tps): schedule bounded on-demand samples`。

### Task 8: 独立TPS区块与曲线

**Files:** Create `src/{types/tps.ts,lib/api/tps.ts,lib/query/queries/tps.ts,lib/usage/tpsSeries.ts,components/usage/TpsCard.tsx}`、`tests/tps/tps-card.test.tsx`；Modify usage-dashboard.tsx、zh/en.json和mock。
**Interfaces:** `TpsCard({selection}: {selection:TpsSelection})`、`useTpsSnapshotQuery(selection)`、`useTpsPointsQuery(selection,viewRange)`，新queryKey前缀['tps']，不共享Token range状态；复用已有图表组件并传实际sampled_at与gap。

- [ ] Step1 红测试：Token今天→30天不改TPS，TPS2m→1h不改费用；6稀疏点不补60点、gap不画0；过期snapshot不显示当前数值，n/方法/age可见。

```tsx
expect(tokenRange).toBe("last30d");
expect(tpsRange).toBe("1h");
expect(renderedMeasuredPoints).toHaveLength(6);
```

- [ ] Step2 Run `pnpm test -- tests/tps/tps-card.test.tsx`，FAIL。
- [ ] Step3 显示最新有效完成/当前回合有效样本/采样曲线；没有进行中证据时回合标“最近一轮”。样本四项用时不冒充累计；方法/思考scope在详情，近似有标记。Codex不给假TPS；不同模型/方法分series。复用现有SVG风格，必要新增小型TpsChart，不向Token图伪造Token字段。IntersectionObserver+窗口visibility心跳；可见时snapshot每5秒/points每60秒refetch，隐藏停前端轮询，后台只600秒。
- [ ] Step4 重跑PASS、pnpm check:i18n、pnpm build；性能错误不遮盖费用；无样本有age说明，安装引导只点击并提示自然重启，不主动终止Harness。
- [ ] Step5 UI验收后提交`feat(tps): show independent sampled throughput`。

### Task 9: 两平台收口验收与交付

**Files:** Create `src-tauri/tests/tps_acceptance_test.rs`、`tests/tps/tps-acceptance.test.tsx`；更新双语手册11-usage、CHANGELOG中英和实施报告`docs/superpowers/reports/2026-10-24-tps-sampling-implementation.md`。
**Interfaces:** 只消费前8任务接口，不新增采集能力或改测量口径。

- [ ] Step1 红测试涵盖原生来源→点→独立图表全链、取消/无F不进速度、源失败不阻塞其它、旧Token快照不变、24小时清理。
- [ ] Step2 实现仅整链连接修复，不新增全面探测。用已有脱敏真实数据重放；每平台最多一次正常有效请求、一次自然取消来确认源入口，不要求七家都新跑一遍。未安装/无法用正常账号运行的写缺口，不冒认通过。
- [ ] Step3 跑`pnpm check`、`pnpm test -- tests/tps`、`cargo test --manifest-path src-tauri/Cargo.toml --lib services::tps`、`cargo test --manifest-path src-tauri/Cargo.toml --test tps_store_test --test tps_native_primary_test --test tps_native_secondary_test --test tps_claude_ingest_test --test tps_sampling_test --test tps_acceptance_test --test usage_two_round_incremental_test --test usage_ledger_test --test real_db_isolation_test`、`cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check`、`cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`；记录真实结果，不把基线失败称本任务通过，假时钟不用等待1小时。
- [ ] Step4 双语说明“抽样非全量、不是服务器基准、只含上报思考、源方法差异、空窗/更新时间、点击安装/自然重启”。报告列平台/宿主/版本/路由真实支持、提交、测量与未通过项；不把OC loaded或Codex回合TTFT称TPS通过。
- [ ] Step5 独立review spec/计划/diff和依赖；提交交付报告，不发布合并。执行Agent返回HEAD+报告给原设计会话review。

## 覆盖与自查

Spec§1/2→Task1/7/8；§3→Task1/3/4/5/6；§4→Task3/4/5/6；§5→Task5/6/7；§6→Task2/7/8；§7→Task9及各任务反例。每个任务有红绿循环及独立交付；不要求先补齐所有真实平台样本才写实现。

作者须核对函数/JSON名与后继消费一致、DAG无环、五类Review Focus均有测试。无法复现的某源不拖住原生来源，但报告必须诚实列出该适配未完成，不能写九任务全通过。
