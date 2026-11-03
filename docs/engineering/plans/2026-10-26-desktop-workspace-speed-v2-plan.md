# 桌面四页导航与输出速度 V2 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复 WorkBuddy、接入 Codex 含等待响应观测，交付同窗四页及可切换双指标的输出速度页面。

**Architecture:** 复用 services/tps 的只读来源、事实表和安装链，在事实层增加响应时长，以 V2 查询聚合区间与时间桶。主窗口缓存已访问页面主体，保留应用级监控；速度界面消费同一份双指标快照。共享主题统一为浅色白底蓝紫、深色雾银及局部轻玻璃。

**Tech Stack:** 现有 Tauri 2 / Rust / SQLite / React 19 / TypeScript / React Query / Zustand / Tailwind v4 / shadcn；SVG 图表，不新增监控平台或组件库。

**Spec:** [桌面四页导航与输出速度 V2](../specs/2026-10-26-desktop-workspace-speed-v2-design.md)。[定稿视觉参考](../mockups/2026-10-26-desktop-workspace-v2.html)仅用于结构、层次与交互，禁止把演示数字写入产品。[Codex 只读依据](../evidence/2026-10-26-codex-response-observation.md)。

## Global Constraints

- 四页同窗：消息看板 / 工具资源 / 模型用量 / 输出速度；默认消息看板，设置保留独立入口。
- 浅色纯白底及原蓝紫强调，深色雾银；轻玻璃仅导航/浮层，正文/图表平面。按 Spec 色表实施，本轮结束配色探索。
- stream = 1000×Σ输出/Σ生成毫秒，response = 1000×Σ输出/Σ含等待响应毫秒；独立有效性与 n。Codex 仅 response，一级标“含等待 · 日志近似”，TTFT/生成窗不伪造。
- 近15m/1h/5h/24h，默认1h；桶宽1m/5m/10m/1h，按完成时刻归 `(from,to]`，边缘裁剪最多额外1桶。区间/桶不截最近20条，不累加旧滚动点。
- 已采集有效子集，不承诺全量。主任务筛掉已识别标题/自动摘要；失败/取消/不明重试不入速度，可靠用时可累计；TTFT 分母独立，未知为 null。
- 工具时长只有可靠归属才能进当前模型；现来源普遍为空，保持 `—`；保留模型用量原工作活动能力。
- 源单批≤200观察，事实/游标同事务。可见5秒有界读取、后台600秒、租约30秒心跳/90秒TTL；数值事实保留24h，查询不触发源扫描或模型请求。
- 不改模型 API/认证/provider/网关，不主动重启 Harness，不读认证来核订阅；不增原生玻璃依赖。
- 工作目录 `/Users/jarvis/Documents/tuvis`。实施前同步 main，确保新分支包含已审 TPS 产品基线 `6b9fc9b5` 与本设计/计划；若上期尚未合并，保留其实现，不能从不含 TPS 的 main 开始静默丢功能。功能分支 `codex/desktop-workspace-speed-v2`；worktree 只在 Documents 下。当前克隆旧对象损坏若阻碍同步，可沿已记录的健康克隆/自包含pack路线修复并核SHA，禁止reset/clean绕过；仍不能同步则停止代码实施并报告，不声称已同步。
- 所有测试影子 home/私有连接；原 reader 的 home 参数显式注入。仅设置 TUVIS_HOME 不等于全部源根隔离，禁止因此启动真实 runtime 采集做测试。修改 usage 共用 Rust 模块前按 AGENTS 停 dev 或隔离全部源根；不杀其他工作区的用户进程。
- 不改 MASTER-PLAN、旧 Token/费用账本/游标或用量计算。保留未跟踪评审报告和 `tuvis-setup-preview.html`；不自动合并、推送、发布、打 tag。

## Review Focus

1. 旧库/旧源游标：缺新列、WorkBuddy 旧EOF、Codex旧占位都须安全升级，进度与事实原子提交；Task1/2/5。
2. 真实结构/重复身份：字符串数值、原生与伴随用量、复制历史、交错工具和延迟封口不能凑窗或翻倍；Task2/4/5。
3. 口径切换：不同有效集合、独立TTFT、Codex临时fallback和历史空闲不能混单位或清空可靠数据；Task6/9/10/12。
4. 同窗/跨窗竞态：监听前导航、过期ACK、隐藏主窗唤起、切页后的监控/租约/原用量ticker不能漂移；Task7/12。
5. 范围/可读性：边缘桶、跨午夜、窄窗、空窗、深浅主题与减少透明度必须如实可读；Task6/8/10/13。

---

## 文件边界、阶段与依赖

阶段 A（Task1—6）交付双指标事实与只读查询；WorkBuddy 旧界面可先恢复流速。阶段 B（7—8）交付同窗页面宿主和共享主题；模型用量仍使用原主体。阶段 C（9—13）接入新速度界面并验收。一个协调计划保持公共类型一致，不并行修改同一组文件。

| 文件/目录                                                                                                                         | 职责                                           |
| --------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------- |
| `src-tauri/src/services/tps/model.rs`、`src-tauri/src/database/{schema,migration,dao/tps}.rs`                                     | 新时长字段、方法、兼容与持久化                 |
| `src-tauri/src/services/tps/sources/{workbuddy,dsh,zcode,kimi,claude,opencode,codex}.rs`                                          | 来源独立准入；Codex编排/游标                   |
| `src-tauri/src/services/tps/sources/codex/decoder.rs`                                                                             | Codex纯事件状态机，无文件/DB/原文持久化        |
| `src-tauri/src/services/tps/v2/{mod,model,sums,query}.rs`、`src-tauri/src/database/dao/speed.rs`                                  | 双指标DTO、充分统计、区间/桶只读聚合           |
| `src-tauri/src/services/workspace_navigation.rs`、`src-tauri/src/commands/workspace.rs`                                           | 主窗待导航目标及revision/ACK                   |
| `src/types/workspace.ts`、`src/stores/workspaceStore.ts`、`src/lib/workspaceNavigation.ts`、`src/hooks/useWorkspaceNavigation.ts` | 页签状态、跨窗打开和可靠消费                   |
| `src/components/usage/UsageDashboardBody.tsx`                                                                                     | 原用量主体，active门控；独立外壳只作旧入口跳转 |
| `src/types/speed.ts`、`src/lib/{api/speed,query/queries/speed,speed/view}.ts`                                                     | IPC镜像、查询、纯显示状态                      |
| `src/components/speed/{SpeedDashboard,SpeedSeriesList,SpeedMetrics,SpeedChart,SpeedSourcesPanel}.tsx`                             | 新速度页面及二级配置                           |

DAG：`1→2/3/4；4→5；1+2+3+5→6；7→8；6→9；9→10/11；7+8+9+10+11→12；2+3+5+6+12→13`。默认按1—13执行；7可独立开发，但不要与12同时改home/用量宿主。

## 公共类型与接口契约（后续任务逐字消费）

- `TimingMethod` 增加 `LogBoundaryApprox` / `log-boundary-approx`；`TpsObservation.response_elapsed_ms: Option<i64>` / IPC `responseElapsedMs`，serde旧记录默认null。`response_sample_ms(&self)->Option<i64>`：显式正响应时长优先；否则仅旧 `is_tps_valid()` 且原生完整时长可靠时兼容，响应不能短于同基准生成窗。`is_response_valid(&self)->bool` 还要求成功及正输出。
- `SpeedMetricMode={Stream,Response}` → `stream|response`；`SpeedRange={Last15m,Last1h,Last5h,Last24h}` → `15m|1h|5h|24h`。`SpeedSort={Tool,Model,Rate}` → `tool|model|rate`。
- `SpeedRate={value:Option<f64>,output_tokens:i64,duration_ms:i64,sample_count:i64}`；`SpeedMetrics={stream:SpeedRate,response:SpeedRate,avg_first_token_ms:Option<f64>,ttft_sample_count:i64,model_ms:Option<i64>,tool_ms:Option<i64>,candidate_count:i64,failed_count:i64,cancelled_count:i64,missing_timing_count:i64,session_count:i64,latest_stream_ended_at:Option<i64>,latest_response_ended_at:Option<i64>,output_scope:OutputScope,scope_mixed:bool}`。
- `SpeedBucket={start_ms:i64,end_ms:i64,is_partial_bucket:bool,metrics:SpeedMetrics}`。无样本 value=null；充分统计的零合计不冒充0速度。
- `SpeedSeriesSummary={series_key:SeriesKey,metrics:SpeedMetrics}`；`SpeedCapabilities={stream:bool,response:bool,stream_reason:Option<String>,response_reason:Option<String>}`。
- `SpeedSourceStatus=Ready|Unconfigured|AwaitRestart|NeedsCheck|Unsupported`（kebab-case）；`SpeedSourceView={source:String,status:SpeedSourceStatus,installed:bool,capabilities:SpeedCapabilities,last_record_at:Option<i64>,reason:Option<String>,detail:Option<String>}`；主界面不直接渲染detail。
- `SpeedDashboardQuery={range:SpeedRange,selection:Option<SeriesKey>,metric_mode:SpeedMetricMode,tool_id:Option<String>,search:String,sort:SpeedSort}`；`SpeedDashboardSnapshot={as_of:i64,range_from:i64,range_to:i64,disabled:bool,source_states:Vec<SpeedSourceView>,series:Vec<SpeedSeriesSummary>,selected_series:Option<SeriesKey>,buckets:Vec<SpeedBucket>}`。CamelCase镜像，所有可空字段显式null。
- V2入口：`query::dashboard(conn:&Connection,query:&SpeedDashboardQuery,now_ms:i64)->Result<SpeedDashboardSnapshot,UsageError>`；IPC `tps_speed_dashboard(query)`；TS `speedDashboard(query):Promise<SpeedDashboardSnapshot>`。
- 导航：`WorkspaceTab=Messages|Resources|Usage|Speed` → `messages|resources|usage|speed`；`WorkspaceNavigation={revision:u64,tab:WorkspaceTab}`。服务 `NavigationQueue::{request,peek,ack}`；IPC `workspace_request_navigation(tab)` / `workspace_peek_navigation()` / `workspace_ack_navigation(revision)`；事件 `tuvis-workspace-navigation` 携带相同对象。

### Task 1：响应事实、旧库迁移与旧数据兼容

**Depends on:** 无；先完成全局前置检查。

**Files:** Modify `src-tauri/src/services/tps/{model,policy}.rs`、`src-tauri/src/database/{schema,migration,dao/tps}.rs`；机械补齐所有 `TpsObservation` 构造点（六源、`tps_{store,native_primary,sampling,acceptance}_test.rs`）；Modify `src/types/tps.ts`、`src/lib/usage/tpsSeries.ts`、`src/i18n/locales/{zh,en}.json`；Create `src-tauri/tests/tps_v2_model_test.rs`。
**Interfaces:** 产生上文新字段、TimingMethod、`response_sample_ms/is_response_valid`；源构造器先显式None，Task2/3/5填实。DAO读写同字段，不改变旧point含义。

- [ ] Step1 写反例 `response_metric_and_legacy_migration`：100输出/生成4000ms/响应5000ms得25与20；旧正流式+native5000可兼容；旧retry无gen即使native>0也不入响应；负/比生成短/失败/取消不能入速度；旧表补列前行值与旧usage快照保留。

```rust
assert_eq!(sample.response_sample_ms(), Some(5000));
assert!(!retry_without_generation.is_response_valid());
assert_eq!(after_migration_old_usage, before_old_usage);
```

- [ ] Step2 跑 `cargo test --manifest-path src-tauri/Cargo.toml --test tps_v2_model_test`，确认新契约缺失导致FAIL。
- [ ] Step3 实现可空列ADD/ALTER与枚举DB映射、serde兼容及谓词；所有既有构造点只作字段机械适配，保持旧断言；前端旧方法映射补“日志近似”中英键。
- [ ] Step4 同命令及 `--test tps_store_test` PASS；序列化null、UPSERT重读、事务中途失败与迁移重跑保持正确。
- [ ] Step5 提交 `feat(speed): add response-duration facts and compatible storage`。**验收：旧数据不丢，新旧完整时长不被重复加等待。**

### Task 2：WorkBuddy真实数字兼容与旧EOF恢复

**Depends on:** Task 1。

**Files:** Modify `src-tauri/src/services/tps/sources/workbuddy.rs`；Create `src-tauri/tests/tps_workbuddy_v2_test.rs`、`src-tauri/tests/fixtures/tps-v2/workbuddy-qua.txt`；不改旧usage采集器。
**Interfaces:** 保留 `read_incremental(home:&Path,cursor:&SourceCursor,now_ms:i64)->Result<ReadBatch,UsageError>`；私有归一 `parse_native_integer(&Value)->Option<i64>`；游标JSON `format_version=2`，旧版仅该来源进入有界恢复。`SourceState.detail` 添加稳定 `parser-errors=<n>` 前缀，详情只含类型/字段/计数，不含整行载荷。

- [ ] Step1 用本机17条已核结构的脱敏最小夹具（仓库存txt，测试复制为影子日期目录下的.log）写 `numeric_strings_and_eof_recovery`：字符串6711/1402/339得gen5309、response6711（该真实行message_id脱敏为wb-known-6711，保留17行相对顺序）；数字形态同值；空串/布尔/小数/溢出诊断；旧EOF无事实恢复，已有同键不翻倍，失败批不推进。

```rust
assert_eq!(recovered.observations.len(), 17);
let sample = recovered.observations.iter().find(|o| o.stable_call_key == "wb-known-6711").unwrap();
assert_eq!(sample.response_elapsed_ms, Some(6711));
assert_eq!(after_replay_count, after_first_count);
```

- [ ] Step2 跑 `cargo test --manifest-path src-tauri/Cargo.toml --test tps_workbuddy_v2_test`，确认旧实现FAIL。
- [ ] Step3 归一三字段，保留终态/身份/正窗准入，response显式填total；旧游标的近24h文件stat预过滤后分批恢复，版本/进度与事实原子提交。整源读字节≤8MiB/轮、观察≤200，残行不跨过，旧账本/其他来源游标不动。
- [ ] Step4 重跑PASS，加“恢复完成下一轮只消费追加”“解析错误可被查询识别”“失败/取消两个速度均无值”；保留AM/PM及RFC3339正例。
- [ ] Step5 提交 `fix(speed): recover WorkBuddy timing records with string numbers`。**验收：刚才两轮已有记录能恢复两指标，不需再发探测请求。**

### Task 3：其余五个原生来源写入含等待响应时长

**Depends on:** Task 1。

**Files:** Modify `src-tauri/src/services/tps/sources/{dsh,zcode,kimi,claude,opencode}.rs`；Create `src-tauri/tests/tps_native_response_test.rs`，按需复用原脱敏fixture，不重造原始事件形态。
**Interfaces:** 六源统一在同调用验证后填 `response_elapsed_ms`；已观察到且无法对齐的retry不填响应值。流式字段/边界保持。

- [ ] Step1 `native_response_windows` 矩阵：DSH step开始→消息结算、ZCode完整native调用、Kimi latency+stream、Claude duration、OpenCode step窗；各断言包含等待但不含后续工具；ZCode/Claude完整时长型夹具缺F仍可response-only；retry/失败/取消不进响应速度。

```rust
assert_eq!(native.response_elapsed_ms, Some(5000));
assert_eq!(missing_first.generation_ms, None);
assert!(missing_first.is_response_valid());
```

- [ ] Step2 跑 `cargo test --manifest-path src-tauri/Cargo.toml --test tps_native_response_test`，FAIL。
- [ ] Step3 按各source现成的aligned/speed_eligible/retry判据构造响应字段，不从generation空值推定失败，也不无条件复用retry的native duration；不重置五源旧游标，旧流式有效事实由Task1兼容函数提供保守历史值。
- [ ] Step4 PASS并跑原 `tps_native_primary_test/tps_native_secondary_test/tps_claude_ingest_test/tps_opencode_test`；保留DSH共享预算、ZCode复合完成游标等已有反例。
- [ ] Step5 提交 `feat(speed): expose native inclusive response durations`。**验收：各源同一次输出可分别计算两种速度，等待只计一次。**

### Task 4：Codex纯响应分段与原生ID去重

**Depends on:** Task 1。

**Files:** Modify `src-tauri/src/services/tps/sources/codex.rs` 仅声明 `pub mod decoder`；Create `src-tauri/src/services/tps/sources/codex/decoder.rs`、`src-tauri/tests/tps_codex_decoder_test.rs`、`src-tauri/tests/fixtures/tps-v2/codex-{normal,tools,duplicates,aborted}.jsonl`。
**Interfaces:** `CodexDecoderState`（Default/Clone/Serialize/Deserialize，只有ID摘要/时刻/模型/未决标记）；`push_record(state:&mut CodexDecoderState,row:&Value,host_instance:&str)->DecodeResult`，`DecodeResult={observations:Vec<TpsObservation>,diagnostics:Vec<String>}`。response_id的SHA-256摘要为稳定键，同一根使用同一host_instance；关联ID在解码边界即转摘要，method=LogBoundaryApprox；generation/first-token/native_duration保持None。

- [ ] Step1 用已核订阅结构写 `native_response_boundary_and_identity`：输入@0、生成结束@5000、原生usage100输出→response20；工具执行5000..15000不进该分母，下一响应从最后匹配工具返回后开始；native+token_count只记一次；复制ID/多工具/子代理模型归属正确；重复封口插在工具返回后也不得改变下一响应起点；取消未闭合、错配ID、不明重试不凑窗。

```rust
assert_eq!(normal.response_elapsed_ms, Some(5000));
assert_eq!((normal.generation_ms, normal.first_token_at_ms), (None, None));
assert_eq!(deduplicated.len(), 1);
```

- [ ] Step2 跑 `cargo test --manifest-path src-tauri/Cargo.toml --test tps_codex_decoder_test`，FAIL。
- [ ] Step3 按Spec§6实现输入→生成→原生用量封口，原生与生成项先后延迟留未决；完整已封口响应不因之后回合取消被抹去；缺上下文/歧义记录诊断跳过，不从task_complete推导速度。
- [ ] Step4 PASS；断言所有Codex观察 `generation_ms==None && first_token_at_ms==None`，无任何“回合duration−TTFT”准入；fixture不含prompt/reasoning/tool正文，保留角色/类型/关联和时间间隔。
- [ ] Step5 提交 `feat(speed): decode Codex response windows from native records`。**验收：每条数值可追到同一原生响应身份及局部边界，明确是日志近似。**

### Task 5：Codex有界读取、游标升级与采集接线

**Depends on:** Task 4。

**Files:** Modify `src-tauri/src/services/tps/sources/codex.rs`；Create `src-tauri/tests/tps_codex_source_test.rs`；必要时补 `src-tauri/src/services/tps/sources/mod.rs`共用预算；沿用collector已注册codex入口。
**Interfaces:** 对外仍是统一read_incremental；`CodexSourceCursor={format_version:1,files:...}`保存offset/头信息/Task4未决状态；不重释旧“不提供流式”占位游标。事实提交继续走store::commit_batch。

- [ ] Step1 `bounded_resume_and_placeholder_upgrade`：旧占位→新格式可取候选；尾行撕裂/重启恢复/截断不丢未决；原生与伴随重复不翻倍；失败事务不推进；多文件合计输入/解析预算有界，观察≤200。

```rust
assert!(read_bytes_in_pass <= 8 * 1024 * 1024);
assert!(batch.observations.len() <= 200);
assert_eq!(cursor_after_failed_commit, original_cursor);
```

- [ ] Step2 跑 `cargo test --manifest-path src-tauri/Cargo.toml --test tps_codex_source_test`，FAIL。
- [ ] Step3 根定位复用现有Codex使用方式且显式home可注入；stat预过滤24h，首次仅有界头信息+最新完整边界，整源读字节≤8MiB/轮、完整JSON行处理≤4096/轮；超大行续扫不解析其正文；跳过坏行/超大行后使相关未决片段失效，等下个可靠输入边界再计速，不能把工具间隔混入。从尾中部进入的首个不完整响应不记速；无对应模型上下文时不拿当前设置补历史。
- [ ] Step4 PASS并用私有库走reader→commit_batch→读回；检查游标不保存正文、token_count不成为第二条事实、不读auth.json、不启动Codex/app-server/模型进程。
- [ ] Step5 提交 `feat(speed): collect Codex response facts with bounded cursors`。**验收：现有订阅日志可提供有依据的响应子集，流速缺口仍真实保留。**

### Task 6：区间/桶聚合与只读V2 IPC

**Depends on:** Task 1、2、3、5。

**Files:** Create `src-tauri/src/services/tps/v2/{mod,model,sums,query}.rs`、`src-tauri/src/database/dao/speed.rs`、`src-tauri/tests/tps_speed_dashboard_test.rs`；Modify tps/dao mod、`src-tauri/src/commands/tps.rs`、`src-tauri/src/lib.rs`注册；`src-tauri/src/services/tps/collector.rs`仅补失败健康状态持久化。
**Interfaces:** 产生公共DTO及 `dashboard(conn,query,now)`、IPC `tps_speed_dashboard(query)`；纯 `derive_metrics(sums:&SpeedSums)->SpeedMetrics`；`bucket_spans(from_ms:i64,to_ms:i64,range:SpeedRange)->Vec<BucketSpan>`。DAO只返回预聚合充分统计，不向前端传原始事实；输入过滤、有效性SQL与Task1谓词有矩阵契约测试；选择仅在筛选后的series中保留，否则取排序第一项，无匹配为None。

- [ ] Step1 `range_bucket_metric_contract`：500/5s+50/1s=550/6、响应等待另算；失败只进可靠累计；TTFT无输出也入独立n；辅助用途排除；双metric不同n；旧point重复不影响；同series跨session但不跨model/method；边界(from,to]、跨起点整条、边缘+1、跨午夜和空闲20分钟仍显示。

```rust
assert!((metrics.stream.value.unwrap() - 550.0 / 6.0).abs() < 1e-9);
assert_eq!((metrics.stream.sample_count, metrics.ttft_sample_count), (2, 3));
assert_eq!(empty_bucket.metrics.response.value, None);
```

- [ ] Step2 跑 `cargo test --manifest-path src-tauri/Cargo.toml --test tps_speed_dashboard_test`，FAIL。
- [ ] Step3 对tps_observation按完成窗/选择器参数化过滤，搜索按字面子串转义LIKE通配符；保留全部主任务候选，在CASE中分别算有效性，不能在WHERE先删掉无速度但有可靠等待/用时的记录；复用同一有效性CTE计算stream/response/TTFT；按series和有限桶聚合SUM，不load_recent后反复复制24h原始Vec。加source/model/channel/method/ended所需索引并核EXPLAIN；未知scope混合时output_scope=Unknown、scope_mixed=true。
- [ ] Step4 PASS；补总授权disabled不删历史、来源安装事实不依赖样本、任一metric可用即有数据、Codex能力仅response；parser-errors或ok=false→NeedsCheck但不抹其他源数据；旧Codex流式缺口前缀不把有效response整体归为Unsupported。collector读取Err只写失败SourceState、保持原游标不动，下一成功批恢复健康。查询路径不得调reader/旧采集。
- [ ] Step5 提交 `feat(speed): query deduplicated range and bucket metrics`。**验收：列表/总览/曲线来自相同事实且不被20条或10分钟旧规则截断。**

### Task 7：四页宿主、用量主体提取与可靠导航

**Depends on:** 无；可独立实施。

**Files:** Create `src-tauri/src/services/workspace_navigation.rs`、`src-tauri/src/commands/workspace.rs`、`src-tauri/tests/workspace_navigation_test.rs`、`src/types/workspace.ts`、`src/stores/workspaceStore.ts`、`src/lib/workspaceNavigation.ts`、`src/hooks/useWorkspaceNavigation.ts`、`src/components/common/WorkspaceTabs.tsx`、`src/components/usage/UsageDashboardBody.tsx`、`tests/workspace/workspace-navigation.test.tsx`；Modify `src/pages/{home,usage-dashboard}.tsx`、`src/main.tsx`、`src/components/common/main-title-bar.tsx`、`src/lib/usage/openWindow.ts`、`src-tauri/src/{lib,services/mod,commands/mod}.rs`、`src/lib/query/queries/{usage,usageV2}.ts`；`src-tauri/capabilities/default.json` 核对main显示/反最小化/聚焦权限；原用量子组件仅补active门控所需参数。
**Interfaces:** Rust `NavigationQueue::{request(tab)->WorkspaceNavigation,peek()->Option<...>,ack(revision)->bool}`，三个IPC分别返回WorkspaceNavigation、Option<WorkspaceNavigation>、bool；事件逐字用公共契约。TS `openWorkspaceTab(tab):Promise<void>`、`useWorkspaceNavigation():void`；Store `{activeTab,visitedTabs,appliedRevision}`。`UsageDashboardBody({active}:{active:boolean})`不含WindowFrame。

- [ ] Step1 写 `navigation_before_ready_and_stale_ack`：请求1早于监听→peek可取；请求2后ACK1不能清2；事件/peek乱序不回退；openUsageDashboard调用main导航与show/unminimize/focus，不创建usage-dashboard；四页反复切换用量状态不重置，监控/通知只挂一次。

```rust
assert!(!queue.ack(first.revision)); // 第二个目标已经在队列
assert_eq!(queue.peek().unwrap().revision, second.revision);
```

```tsx
expect(createdWindowLabels).not.toContain("usage-dashboard");
expect(store.activeTab).toBe("usage");
```

- [ ] Step2 跑Rust workspace_navigation_test及 `pnpm exec vitest run tests/workspace/workspace-navigation.test.tsx`，FAIL。
- [ ] Step3 队列先存后emit；main订阅后peek，显示/聚焦恢复时再peek；按revision去旧、应用tab后ACK。本窗页签点击也经同一请求revision排序，较早跨窗目标不能覆盖较晚本窗选择。main现关闭为hide（system_tray.rs），以现有main窗口show/unminimize/focus；缺窗口时报明确信息且保留目标。首次访问才mount主体，之后保留并以active隐藏；原会话摘要/工具chips只放消息页、监控Hooks仍在宿主；模型用量提取时即移除旧TPS卡，输出速度页在Task12前复用既有TpsCard作为可工作过渡；原用量各查询可选enabled门控、useUsageCollect增加可选active默认true，隐藏不设UI兜底ticker，恢复可见继续按原间隔，旧调用者行为保持。
- [ ] Step4 两命令PASS；更新旧usage-window断言为新唯一入口但保留主题/状态/唤起判别力。旧/usage只适配到main，不嵌套用量外壳；Toaster、会话轮询、通知和Rust后台采集不因切页重复。
- [ ] Step5 提交 `feat(workspace): host four pages in the main window`。**验收：旧详情入口都到同一主窗，模型用量原功能继续可用。**

### Task 8：落实已定深浅主题与轻玻璃

**Depends on:** Task 7。

**Files:** Modify `src/index.css`、`src/components/common/{window-frame,title-bar,main-title-bar,WorkspaceTabs}.tsx`、`src/components/ui/{button,input,select,card,dialog}.tsx` 的必要样式、`src/components/sessions/SessionCard.tsx`、`src/components/resources/ExtensionList.tsx` 的对比度例外及locale；Create `src/components/common/WorkspacePageHeader.tsx`、`tests/workspace/workspace-theme.test.tsx`；主题SSOT机制保持。
**Interfaces:** 语义token沿Spec色表；新增 `.workspace-glass` 仅导航/浮层，solid fallback。统一 `WorkspacePageHeader({title,description?,actions?})` 供四页用；不把mock HTML整段CSS粘到生产。

- [ ] Step1 只写必要行为反例：深浅主题同窗/设置同步，减少透明度/不支持blur时仍可读且可操作；标题/导航选中态有可辨属性。用截图清单验证色层，不为每个CSS值写镜像测试。

```tsx
expect(screen.getByRole("navigation", { name: "主导航" })).toBeVisible();
expect(mockThemeWindows.map((w) => w.label)).toEqual(expect.arrayContaining(["main", "settings"]));
expect(mockThemeWindows.every((w) => w.theme === "dark")).toBe(true);
```

- [ ] Step2 跑 `pnpm exec vitest run tests/workspace/workspace-theme.test.tsx`，确认新头部/材质契约FAIL。
- [ ] Step3 统一白底蓝紫浅色、雾银深色、正文/边框/焦点；内容平面、导航和浮层高不透明玻璃，禁止面板黄青绿染色。保留状态/品牌色、系统主题和原布局。不改mobile.css、宠物素材/透明背景、分享图固定浅色。
- [ ] Step4 PASS、`pnpm build`；浏览器mock在800×600和宽窗查四页/设置的深浅截图与普通文本4.5:1；缺少原生系统偏好证据时明确标未执行，不伪造实机结果。
- [ ] Step5 提交 `feat(workspace): unify desktop surfaces and restrained glass`。**验收：已定配色落实，可读性改善，不再进入配色讨论。**

### Task 9：前端双指标查询与显示状态契约

**Depends on:** Task 6。

**Files:** Create `src/types/speed.ts`、`src/lib/api/speed.ts`、`src/lib/query/queries/speed.ts`、`src/lib/speed/view.ts`、`tests/speed/speed-data.test.tsx`；Modify `src/tauri-mock.ts`、`tests/msw/tauriMocks.ts`、`src/i18n/locales/{zh,en}.json`、`src/lib/usage/mockFixtures.ts`。
**Interfaces:** DTO逐字镜像Task6；`SPEED_KEY=[...TPS_KEY,'v2']`；`useSpeedDashboardQuery(query,{active}):UseQueryResult<SpeedDashboardSnapshot>`（可见5秒、隐藏enabled=false）；`effectiveMetric(preferred,capabilities):SpeedMetricMode`、`rateForMode(metrics,mode):SpeedRate`、`seriesKeyId(key)`（JSON字符串化tool/model/channel/method四元组）、`sortSeries(series,sort,mode)`、`isSmallSample(rate)`（n>0且输出合计<50，沿既有阈值）。

- [ ] Step1 `metric_fallback_is_not_a_preference_change`：默认stream，Codex→response且偏好未改，回六源→stream；显式response可看七源；支持stream但区间n0时不擅自切换；两种rate/n/单位同取，未知排末；切用量范围无TPS请求。

```ts
expect(effectiveMetric("stream", responseOnly)).toBe("response");
expect(effectiveMetric("stream", bothCapabilities)).toBe("stream");
expect(preferredModeAfterCodexFallback).toBe("stream");
```

- [ ] Step2 跑 `pnpm exec vitest run tests/speed/speed-data.test.tsx`，FAIL。
- [ ] Step3 实现camelCase类型、API及纯函数；主请求只读，不自行调用采集。真实刷新复用现有tpsRefresh限频/单飞，忽略其旧snapshot，只失效TPS前缀重查V2。mock含native双值、Codex仅response、空桶/混scope/未知工具时长。
- [ ] Step4 PASS；假时钟断言active false没有5秒请求，重新可见恢复；API命令字符串只在lib/api，不污染旧usage轮询静态锁。
- [ ] Step5 提交 `feat(speed): add typed dual-metric queries and view state`。**验收：缺一种能力不伪装数值，也不破坏用户选择。**

### Task 10：双指标图表与区间四格

**Depends on:** Task 9。

**Files:** Create `src/components/speed/{SpeedChart,SpeedMetrics}.tsx`、`src/lib/speed/chart.ts`、`tests/speed/speed-chart.test.tsx`。
**Interfaces:** `SpeedChart({buckets,mode,from,to,onHover,onPin})`；`SpeedMetrics({metrics,mode,series,context,onClearPin})`；固定对象 `{seriesKey,mode,startMs,endMs,metricsSnapshot}`，不固定数组index。复用主题图表变量，灰虚线单独图层。

- [ ] Step1 `bucket_hover_pin_and_gap`：两模式单位/n正确；hover桶四值联动，空速度但有可靠TTFT/用时不整组清空；leave恢复，click固定快照不随refetch换桶，Esc/切mode解除；键盘可操作；gap无0值/无插值tooltip。

```tsx
expect(screen.getByTestId("speed-rate-unit")).toHaveTextContent("Token/s");
expect(screen.getByTestId("speed-first-token")).toHaveTextContent("—");
expect(pinned.metricsSnapshot).toEqual(snapshotAtClick);
```

- [ ] Step2 跑 `pnpm exec vitest run tests/speed/speed-chart.test.tsx`，FAIL。
- [ ] Step3 x按完整范围、y从0并用易读刻度；按宽度挑4—7标签，24h跨日期桶tooltip显示日期；实线只连有数据相邻桶，阴影与不可命中虚线作参考，1点也有坐标轴。Codex标题“响应累计用时”、TTFT `—`不带伪秒值；近似短标记一级可见。
- [ ] Step4 PASS；800px窄窗/只有1点/全部无值/部分桶/两模式候选集合不同都可读；没有pointer操作也能用键盘读数，不依赖原生title气泡。
- [ ] Step5 提交 `feat(speed): render inspectable bucket trends and metrics`。**验收：可以看区间，也可以明确查看某个时间桶，数值与时间范围不混。**

### Task 11：二级采集管理与同一套接入流程

**Depends on:** Task 9。

**Files:** Create `src/components/speed/SpeedSourcesPanel.tsx`、`tests/speed/speed-sources.test.tsx`；Modify `src/components/settings/UsageSection.tsx` 接入该面板，复用原TpsSourceStates的配置操作包装与脱敏函数，不复制安装算法。
**Interfaces:** `SpeedSourcesPanel({open,onOpenChange,sources}:...)`；沿用tpsInstallPreview/Apply/Uninstall、reasonKey及脱敏DTO。成功动作await失效TPS前缀；来源status与capabilities均来自Task6，不打印机器前缀。

- [ ] Step1 `source_actions_follow_install_fact`：active/needs-check/无样本已装均可卸载，installed=false有重装入口；Codex ready+response-only显示可用且无插件安装按钮；preview→原生信任说明→待重启明确；stale/transfer冲突和敏感header遮罩保留。

```tsx
expect(screen.queryByTestId("speed-install-codex")).not.toBeInTheDocument();
expect(screen.getByTestId("speed-install-claude")).toBeVisible(); // 卸载后仍有历史
```

- [ ] Step2 跑 `pnpm exec vitest run tests/speed/speed-sources.test.tsx`，FAIL。
- [ ] Step3 一级名称+状态+短能力+动作；技术说明进入details，用户不需读日志注释。设置与速度页共用组件；设置只在面板open且窗口可见时用Task9查询取sourceStates，不申请速度图租约；不要求手写配置、不调用Harness关闭/重启，不能泄露receipt明文凭证。
- [ ] Step4 PASS；完整“卸载保留历史→重装”流程，包含API返回错误/用户后改冲突仍保留数据和可重试入口；真实安装只跑原有影子库集成测试。
- [ ] Step5 提交 `feat(speed): present actionable collection setup and diagnostics`。**验收：用户能看懂能测什么、点哪里接入，详情可查而不占满首页。**

### Task 12：速度页面组合、筛选与应用驻留

**Depends on:** Task 7、8、9、10、11。

**Files:** Create `src/components/speed/{SpeedDashboard,SpeedSeriesList}.tsx`、`tests/speed/speed-dashboard.test.tsx`；Modify `src/pages/home.tsx` 挂载，`src/components/usage/UsageDashboardBody.tsx` 核实无旧TPS卡，必要时调整页面状态hook；清理无消费者的旧卡面组件前核引用，保留兼容API。
**Interfaces:** `SpeedDashboard({active}:{active:boolean})`；本地状态 `{range:'1h',preferredMode:'stream',selection,toolFilter,search,sort:'tool',pinned:null}`，运行中切页保留。数据Task9、图Task10、管理Task11；全局监控仍在Home应用层。

- [ ] Step1 `workspace_speed_full_flow`：三会话同series汇总、列表不铺多图、搜索/工具/排序正确；两模式全界面同步，Codex临时fallback显著且返回恢复；用量30d与速度1h互不联动；后台到前台历史不消失；切页通知仍在而速度租约false/轮询停。

```tsx
expect(screen.getByTestId("speed-mode")).toHaveAttribute("data-mode", "response");
expect(screen.getAllByTestId("speed-chart")).toHaveLength(1);
expect(lastHeartbeat.visible).toBe(false); // 切离速度页
```

- [ ] Step2 跑 `pnpm exec vitest run tests/speed/speed-dashboard.test.tsx tests/workspace/workspace-navigation.test.tsx`，FAIL。
- [ ] Step3 依定稿结构用SpeedDashboard替换Task7的过渡TpsCard并接入全部组件；当前选择无区间样本仍保留组合与空态，若过滤后选中项不可见则选排序后第一项并解除固定；asOf取server快照，不让列表/图各自猜now。源错误只给局部可重试提示，已加载其他数据不清空。
- [ ] Step4 PASS、`pnpm check:i18n`、`pnpm build`；宽窗/800×600、深浅、所有无值、数百组合（仅一张图）、配置后缓存失效与隐藏窗返回均有浏览器mock交互记录。
- [ ] Step5 提交 `feat(speed): integrate the range dashboard into the workspace`。**验收：用户反馈里的主工作流可完整操作，原模型用量不混入TPS卡。**

### Task 13：整链验收、双语说明与交付

**Depends on:** Task 2、3、5、6、12。

**Files:** Create `src-tauri/tests/tps_speed_acceptance_test.rs`、`tests/speed/speed-acceptance.test.tsx`、`docs/engineering/evidence/reports/2026-10-26-desktop-workspace-speed-v2-implementation.md`；更新中英11-usage手册及相关导航手册、CHANGELOG双语Unreleased；旧规格的入口/速度旧规则加新稿引用，MASTER-PLAN不改。
**Interfaces:** 仅消费前12任务，修接线，不追加来源能力/指标口径。报告逐任务提交、实际门禁、能力矩阵与平台未运行项。

- [ ] Step1 新整链反例：WB旧EOF字符串重放→事实→两指标区间/桶；Codex原生ID→仅response且TTFT空；混合成功/取消/重复调用不污染；旧usage快照不变，清理仅本域24h。

```rust
assert!(codex.response.value.is_some());
assert_eq!((codex.stream.value, codex.avg_first_token_ms), (None, None));
assert_eq!(old_usage_after, old_usage_before);
```

- [ ] Step2 跑两套新验收，确认修复前实际缺口FAIL，禁止以只改期望数值/不推进时钟代替修生产接线。
- [ ] Step3 最小连接修复，保持事务/源预算/去重，假时钟覆盖后台600s推进与可见5s、首尾部分桶，不真等1小时；测试全部tempdir/home注入。
- [ ] Step4 运行下方“统一门禁”全部命令，记录实际退出码、计数和未运行原因，不把实施者假定当通过。
- [ ] Step5 用browser mock做四页/双模式/配置/深浅交互验收，附截图；自动化不得启动会写真实账本或改真实Hook的原生dev应用。只有隔离全部源根、配置根和账本并核实后，才可额外做原生UI验证；否则把原生隐藏唤起、系统材质偏好、Windows实机/ACL留为明确未运行，不伪装browser为真机。
- [ ] Step6 双语说明双指标差异、Codex日志近似及TTFT缺失、工具归属缺口、采样非全量、虚线非推算、同窗入口与自然重启；README/capabilities随发版纪律核对，不提前宣称未验证路线。报告对照Spec每项验收与本计划DAG。
- [ ] Step7 整分支独立review后处理P0/P1/P2，提交报告与必要修复（`test(speed): finalize v2 acceptance and delivery report`）；返回分支、最终HEAD、逐Task提交、真实结果及未完成项给原设计会话review。**验收：功能与限制均可查证，无自动合并/推送/发布/tag。**

## 覆盖与自查

Spec§1/2→7/12；§3→8；§4→7/12/13；§5→1/3/6/9/10；§6→4/5；§7→6/10；§8→9/10/11/12；§9→2；§10→1/6/7/9；§11→13与各任务反例。Review Focus五项均有对应任务与判别输入。

执行前只需读现行AGENTS、MASTER-PLAN、Spec、本计划与Codex evidence；不用重新执行暂停的旧全区间计划，也不用把全面探测作为开工前置。实施过程中确有数据歧义则只降级该候选/来源并报告，不默改公式或编造缺失首点。

## 统一门禁

```bash
pnpm check
pnpm exec vitest run tests/speed tests/workspace tests/tps tests/pet
cargo test --manifest-path src-tauri/Cargo.toml --lib
cargo test --manifest-path src-tauri/Cargo.toml --test tps_store_test --test tps_native_primary_test --test tps_native_secondary_test --test tps_claude_ingest_test --test tps_opencode_test --test tps_sampling_test --test tps_acceptance_test --test tps_v2_model_test --test tps_workbuddy_v2_test --test tps_native_response_test --test tps_codex_decoder_test --test tps_codex_source_test --test tps_speed_dashboard_test --test workspace_navigation_test --test tps_speed_acceptance_test --test usage_two_round_incremental_test --test usage_ledger_test --test real_db_isolation_test
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo check --manifest-path src-tauri/Cargo.toml --target x86_64-pc-windows-gnu --lib
```
