# 桌面四页导航与输出速度 V2 · 实施报告（Task 13 整链验收与交付）

> 日期：2026-10-26。分支：`codex/desktop-workspace-speed-v2`。
> 本文是 13 任务计划（`docs/superpowers/plans/2026-10-26-desktop-workspace-speed-v2-plan.md`）的交付报告：
> 逐任务提交、实际门禁实录、逐源能力真实支持范围、Spec §11 验收逐行结论与证据指针、
> browser-mock 截图清单、平台未运行项与已知限制。所有结论以**实际运行的命令与测试**为准，
> 不把实施者假定当通过。

## 1. 分支与最终 HEAD

- 功能分支：`codex/desktop-workspace-speed-v2`（工作目录 `/Users/jarvis/Documents/tuvis`）。
- 验收开工时 HEAD：`f56615dc`（Task 12 提交，分支上最后一个功能提交）。
- **最终交付提交 = 本报告所在提交**：`test(speed): finalize v2 acceptance and delivery report`
  （内容 = Task 13 新增的两套整链验收测试、本报告、双语文档、旧规格指针、验收截图）。
  验收套件未迫使任何生产代码修改（见 §4 与 §8），故只有这一个交付提交。

## 2. 与 main 的关系

- 上期 TPS 短时抽样产品基线 `codex/tps-sampling@6b9fc9b5` **尚未合并进 main**
  （main 为 `5cc233b5`）。按计划全局约束，分支建立时以 `bbcffa70` 完成设计/计划/证据定稿后，
  经 `0205561c` 将 `origin/main` 并入**同时保留 TPS 实现**（merge 提交），未从不含 TPS 的
  main 静默丢功能。
- 因此本分支相对 main 的 diff 同时包含：TPS 采样基线（依赖）+ 本轮 12 个功能任务。
  **合并次序已由用户裁决（2026-10-27）：TPS 与本轮界面改动同一 PR 一并提交**，不拆成「先并 TPS
  再并本分支」两步；本分支已含 TPS 基线并已并入 main `00167e7a`（§12），可直接作为该 PR 的头。
  本分支不自动合并、推送、发布或打 tag。

## 3. 逐 Task 提交表（已对 `git log --oneline 0205561c..HEAD` 核验）

| 阶段       | 提交                                              | 内容                                                                                                  |
| ---------- | ------------------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| 分支基线   | `bbcffa70`                                        | docs(speed) finalize design/plan/evidence                                                             |
| 分支基线   | `0205561c`                                        | merge origin/main（TPS 基线 `6b9fc9b5` 保留为依赖；main `5cc233b5` 并入）                             |
| Task 1     | `85b46a8d`                                        | feat(speed): add response-duration facts and compatible storage                                       |
| Task 2     | `a32b1f46`                                        | fix(speed): recover WorkBuddy timing records with string numbers                                      |
| Task 3     | `4ed78e3b`                                        | feat(speed): expose native inclusive response durations                                               |
| Task 4     | `c0141367`                                        | feat(speed): decode Codex response windows from native records                                        |
| Task 5     | `3cf0ec97` + `1c5aa16f` + `545eb348` + `2a142d55` | collect Codex response facts with bounded cursors（含评审修复：占位断言退役/检查点无损/封口停批分离） |
| Task 6     | `259a06c0`                                        | feat(speed): query deduplicated range and bucket metrics                                              |
| 错误码棘轮 | `ffd4872f`                                        | fix(speed): keep new TPS diagnostics ASCII for the error-code ratchet                                 |
| Task 7     | `fd572796` + `6427a713` + `f34595fd`              | feat(workspace): host four pages in the main window（含隐藏页查询门控两轮修复）                       |
| Task 8     | `3352683d`                                        | feat(workspace): unify desktop surfaces and restrained glass                                          |
| Task 9     | `06db20f7` + `5c40a255`                           | feat(speed): add typed dual-metric queries and view state（含 reason-token i18n 契约修复）            |
| Task 10    | `952013c3`                                        | feat(speed): render inspectable bucket trends and metrics                                             |
| Task 11    | `433a147d` + `a0dea6c5`                           | feat(speed): present actionable collection setup and diagnostics（含设置侧来源拉取终态修复）          |
| Task 12    | `f56615dc`                                        | feat(speed): integrate the range dashboard into the workspace                                         |
| Task 13    | 本提交                                            | test(speed): finalize v2 acceptance and delivery report                                               |

## 4. 实际门禁实录（统一门禁逐条）

运行环境：macOS（arm64）。全部命令在本分支工作目录执行；未触其他 worktree 的进程与目录。

| #   | 命令                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              | 退出码 | 结果                                                                                                                                                                                                                                                                                            |
| --- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | `pnpm check`（format + lint + i18n + errcode + build）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            | 0      | 全绿（新文件经 `prettier --write` / `cargo fmt` 归位后通过；首轮曾因新测试文件格式失败，属交付物格式而非功能缺陷）                                                                                                                                                                              |
| 2   | `pnpm exec vitest run tests/speed tests/workspace tests/tps tests/pet`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            | 0      | 71 文件，553 passed / 5 skipped / 0 failed（5 skipped 为既有用例）                                                                                                                                                                                                                              |
| 3   | `cargo test --manifest-path src-tauri/Cargo.toml --lib`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           | 0      | 2985 passed / 0 failed / 38 ignored（既有 ignore）                                                                                                                                                                                                                                              |
| 4   | `cargo test … --test tps_store_test --test tps_native_primary_test --test tps_native_secondary_test --test tps_claude_ingest_test --test tps_opencode_test --test tps_sampling_test --test tps_acceptance_test --test tps_v2_model_test --test tps_workbuddy_v2_test --test tps_native_response_test --test tps_codex_decoder_test --test tps_codex_source_test --test tps_speed_dashboard_test --test workspace_navigation_test --test tps_speed_acceptance_test --test usage_two_round_incremental_test --test usage_ledger_test --test real_db_isolation_test` | 0      | 18 个二进制共 247 passed / 0 failed（其中 `tps_speed_acceptance_test` 4 passed 为本轮新增）                                                                                                                                                                                                     |
| 5   | `cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 | 0      | 通过                                                                                                                                                                                                                                                                                            |
| 6   | `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  | 0      | 通过（0 警告）                                                                                                                                                                                                                                                                                  |
| 7   | `cargo check --manifest-path src-tauri/Cargo.toml --target x86_64-pc-windows-gnu --lib`                                                                                                                                                                                                                                                                                                                                                                                                                                                                           | 0      | 仅编译检查通过（无 Windows 执行声明，见 §8）                                                                                                                                                                                                                                                    |
| 8   | `pnpm exec vitest run tests/`（全量前端）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         | 0      | 193 文件，2320 passed / 5 skipped / 0 failed。**首轮实录**：1 failed（`tests/tps/opencode-observer.test.ts`「生产默认 hash」用例，`rows[0]` 未定义）——该文件独立重跑两次均 9 passed，全量重跑亦全绿；与本题改动无耦合（未触碰 OpenCode 观察器），判定为并行 worker 下的既有偶发，如实记录不掩盖 |

## 5. 各来源两种指标的真实支持范围（对照 Spec §5.2 逐源）

「支持」= 具备计算路径，不表示每条记录都齐全。下表为**实测/测试背书**的支持范围；
缺什么、为什么，逐源写明。测试指针均为本分支实际用例。

| 来源          | 流式输出（TPS）                                           | 含等待响应（Token/s）                                                               | 缺口与原因（测试背书）                                                                                                                                                                 |
| ------------- | --------------------------------------------------------- | ----------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| DSH           | 有效子集（同模型 step 开始→消息结算；重试不能对齐则跳过） | 有效子集（同调用原生完整时长，已含等待不双加）                                      | 重试不可对齐的记录跳过（`tps_native_primary_test` 45 例）                                                                                                                              |
| ZCode         | 支持有 F 的调用（start/completed 对齐）                   | 支持可靠原生完整调用（不含后续工具执行）                                            | 无 F 调用不入流式；`tps_native_primary_test` / `tps_speed_dashboard_test`                                                                                                              |
| Kimi          | 原生有效子集（latency 与 stream duration 同事件组合）     | 同 step 完整时长                                                                    | 内部重试不可见属来源限制；`tps_native_secondary_test` 18 例                                                                                                                            |
| WorkBuddy Mac | 修复后支持（整数字符串兼容）                              | 修复后支持（qua.end 的 total_cost_ms）                                              | **字符串数值兼容 + v1 EOF 有界恢复**：`tps_workbuddy_v2_test` 11 例、整链 `tps_speed_acceptance_test::workbuddy_v1_eof_recovery_*`；Windows 无数值 usage 路线仍缺证据（未运行）        |
| Claude        | 原生近似（first_content 为块开始近似）                    | 有效 span（完整 span duration 含等待）                                              | 已识别重试不对齐则跳过；`tps_claude_ingest_test` 31 例                                                                                                                                 |
| OpenCode      | 观察插件支持的宿主                                        | 支持完整 step 窗的宿主                                                              | 同 session/message/step 身份，不用跨工具墙钟；`tps_opencode_test` 16 例                                                                                                                |
| Codex         | **暂缺**（无同调用生成窗）                                | 日志近似支持有效子集（原生 response_id + 日志边界，timing = `log-boundary-approx`） | 逐响应 TTFT 不可得 → `avg_first_token_ms` 恒 null；`tps_codex_decoder_test` 8 例、`tps_codex_source_test` 18 例、整链 `tps_speed_acceptance_test::codex_native_reader_response_only_*` |

聚合口径（`tps_speed_dashboard_test` 19 例 + `tps_v2_model_test`）：两个速度独立有效集与 n、
TTFT n 独立；失败/取消不入速度但可靠用时进累计池；辅助用途整体排除；同 series 跨会话合并；
半开完成窗 `(from,to]`；边缘部分桶 ≤ 名义+1；旧 `tps_point` 对 V2 求和零影响；SQL 有效性
CTE 与 Rust 谓词逐记录一致；EXPLAIN 走索引。

## 6. Spec §11 验收表逐行结论与证据指针

| 验收面     | 结论                                           | 证据                                                                                                                                                                                                                                                                                                        |
| ---------- | ---------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 双指标公式 | ✅ 通过                                        | brief 算术（100 Token/等待1s/生成4s → 流式25、响应20）与 Σ/Σ 聚合：`tps_speed_dashboard_test::brief_arithmetic_*`、`response_metric_is_computed_independently`；等待不双加/工具间隙不进响应分母由各源解码器契约锁（`tps_native_response_test` 13 例）                                                       |
| 独立有效性 | ✅ 通过                                        | 缺F只入response / 双 n 独立 / 失败取消不凑值 / 其他格独立显示：`tps_speed_dashboard_test::failed_cancelled_enter_candidate_and_model_ms_but_neither_speed`、`sql_validity_matches_rust_predicates_matrix`；整链 `tps_speed_acceptance_test::mixed_pollution_*`                                              |
| Codex      | ✅ 通过（回放）                                | 真实读取器回放 → 去重 response 观测、stream 与逐响应 TTFT 恒 null、复制历史/工具/多响应/取消/不完整尾行：`tps_codex_decoder_test`、`tps_codex_source_test` 18 例、整链 `tps_speed_acceptance_test::codex_native_reader_response_only_in_dashboard_and_duplicate_pass_single_row`（brief 断言逐字）          |
| WorkBuddy  | ✅ 通过                                        | 数值/字符串双形态两指标；旧 EOF 游标有界恢复且幂等、不动旧账本：`tps_workbuddy_v2_test` 11 例 + 整链 `workbuddy_v1_eof_recovery_whole_chain_dual_metrics_and_idempotent_replay`（≈63.85 / ≈50.51 锚定）；整链前后 usage_* 快照逐行不变：`old_usage_ledger_untouched_by_whole_chain_and_purge_scoped_to_tps` |
| UI切换     | ✅ 通过（vitest + browser-mock）               | 列表/主数字/单位/图轴/n 同步切换；Codex fallback 显著且切回六源恢复偏好：`tests/speed/speed-dashboard.test.tsx`、整链 `tests/speed/speed-acceptance.test.tsx` 用例 2；截图 `acceptance/` 双模式页                                                                                                           |
| 区间与图   | ✅ 通过（offset 钉死 + browser-mock）          | 四档/边缘桶/跨午夜：`tps_speed_dashboard_test::bucket_spans_align_clip_and_respect_offset`；空闲20分钟历史可见：`idle_gap_keeps_series_and_surfaces_latest`；悬停/固定/键盘 + 无样本不补0：`tests/speed/speed-chart.test.tsx`；截图 `acceptance/light-1280x800-speed.png`（缺口阴影+虚线图例）              |
| 导航与用量 | ✅ 通过（vitest + browser-mock）               | 四页独立状态/旧入口到 main/租约与监控不残留：`tests/workspace/workspace-navigation.test.tsx` 9 例、整链 `tests/speed/speed-acceptance.test.tsx` 用例 1/5/6；Rust 队列 `workspace_navigation_test` 3 例；用量原折线/真实0/分区/记录/导出/费用由既有 usage 测试族锁定（全量 vitest 2320 passed）              |
| 主题       | ✅ 通过（vitest 行为契约 + browser-mock 截图） | 800×600 及宽窗深浅四页可读、玻璃降级声明、跨窗同步：`tests/workspace/workspace-theme.test.tsx`；截图 `acceptance/` 16 张（两主题 × 两尺寸 × 四页）；移动端/桌宠透明面/分享图未污染（未触碰其样式入口）                                                                                                      |

## 7. 浏览器 mock 交互与截图清单（**browser-mock，非真机**）

方式：`pnpm dev --port 5199`（纯 Vite + `src/tauri-mock.ts` 浏览器 mock；Playwright 驱动，
1280×800 与 800×600）。**全部为浏览器 mock 证据，不代表 Tauri 真机**；未启动任何原生
dev 应用。新增 19 张，位于
`docs/superpowers/evidence/2026-10-26-desktop-workspace-v2/acceptance/`：

| 截图                                                                                  | 内容                                                                                                                                                                                                                                                                                                                                                                                                                       |
| ------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `{light,dark}-{1280x800,800x600}-{messages,resources,usage,speed}.png`（16 张，新拍） | 四页 × 两主题 × 两尺寸整版（Task 8 的 `theme/` 截图早于 Task 9–12 的速度页接入，速度页已过时，故全套重拍）                                                                                                                                                                                                                                                                                                                 |
| `acceptance-scale-322-series-dark-1280x800.png`                                       | Task 12 遗留 (a) 数百组合规模：**运行时包装** `__TAURI_INTERNALS__.invoke` 把 `tps_speed_dashboard` 快照的 7 个基础组合克隆至 **322 行**（7×46，模型名加变体后缀）——dev 侧内存包装，**未提交任何 300 系夹具**，方法即此段脚本；宽列表 322 行渲染、窄窗 select 同样可列                                                                                                                                                     |
| `acceptance-apply-invalidate-receipt.png`                                             | Task 12 遗留 (b) 配置后缓存失效：来源面板 → Claude 接入预览（脱敏）→ 应用 → 回执「下次启动生效」；invoke 记账实证 apply 后 `tps_speed_dashboard` 由 0 → 1（["tps"] 前缀失效重取）                                                                                                                                                                                                                                          |
| `acceptance-hidden-return-restore.png`                                                | Task 12 遗留 (c) 隐藏窗返回：暂缓导航监听注册（模拟主窗隐藏/监听未就绪）→ `workspace_request_navigation{speed}` 只滞留队列（页面停在消息看板，peek 可见 pending）→ 放行注册 + `visibilitychange` → hook peek 恢复：speed 页 `data-active=true`、pending 清空                                                                                                                                                               |
| 复用（页面未变）                                                                      | Task 12 的 `speed/` 8 张状态截图（dashboard / Codex fallback / 固定读数 / 来源面板 / 空搜索 / 窄窗两态）——速度组件自 `f56615dc` 未再改动，原样复用（本报告初版定稿时分支 HEAD 为 `19eafe5b`；终审修复波后的 HEAD 见 §11）。**时效标注**：其中 Codex fallback 一张摄于终审修复波（§11.1）之前——该波更改了 fallback 态下列表的排序依据，图中列表顺序为修复前形态；fallback 行为本身（data-mode/禁用流式按钮/显著标注）未变。 |

交互记录（均为浏览器 mock 内实测）：四页切换/状态保持、双模式切换、Codex fallback 与恢复、
采集管理预览→应用→回执、深浅主题切换、隐藏返回恢复——与 vitest 判据一致。

## 8. 未运行 / 平台限制清单（如实申报，不外推通过）

- **原生 Mac UI 未验证**：本轮仅 browser-mock；未启动 Tauri/dev 应用（纪律：自动化不得启动
  会写真实账本或改真实 Hook 的应用）。原生窗口材质、系统托盘、桌宠、迷你条实机表现未跑。
- **系统材质偏好**：`prefers-reduced-transparency` / `backdrop-filter` 缺失的**实机**降级只
  有 vitest 行为契约（`data-glass-fallback`）与 Task 8 静态截图，无实机复核。
- **Windows 实机 / ACL**：`x86_64-pc-windows-gnu` 仅为编译检查；Windows 终端注入、ACL、
  WebView2 实机渲染均未运行。
- **真实宿主采集**：全部 Rust 验收走影子 home + 私有库 + 显式时钟；未对真机
  `~/.workbuddy`、`~/.codex`、dsh 宿主等做实采核对。WorkBuddy Windows 路线依旧缺证据。
- **README / `docs/capabilities.json`**：按发版文档纪律属发版批次同步物，本轮**未改**；
  能力矩阵的「输出速度」行随发版补录。

## 9. Deferred minors 分级清单（汇总自各任务台账；不在本轮修复）

**属终审范围（建议 final review 裁决，多为注释/措辞/测试补强）**：
Task 2（数字形态浮点/溢出拒绝缺测试、response-only-when-ttft-missing 未断言）；
Task 3（ZCode 陈旧诊断措辞、Kimi/OpenCode response-only 覆盖）；
Task 4（call_id 静默区、turn_context 丢弃对齐 pending seal）；
Task 5（mtime 基线测试、陈旧字段注释、legacy state 下 remember_digest 失步、无换行尾文件）；
Task 6（debug_assert 前置、model_ms 镜像测试局部、LIKE ASCII 大小写、DST offset 钉死 `to`）；
Task 7（导航失败 UI 死键、陈旧注释、BillingAssignmentDialog 隐藏页未门控）；
Task 8（ResourceKindDialog 10px、color-mix 未入 @supports 守卫）；
Task 10（15m 三标签边缘、滚动窗自动清除 UX、aria-live、800px 几何测试）；
Task 11（hook 间隔选项、可安装集合三处重复、共享可见性 hook、陈旧注释、亚秒刷新 5s 延迟）；
Task 12（Step4 三条浏览器记录 → 本轮 (a)(b)(c) 已补、test-6 实时计时 flaky、testid 撞名、
死回退分支、title-only 说明、搜索无防抖）。

**随发版批次处理**：README / `docs/capabilities.json` 能力矩阵补录；CHANGELOG 双语
[Unreleased] 已写入（发版时与 0.6.2 小节合并裁断）。

**已知限制（如实申报，非遗漏）**：

- Task 1：migration guard 在 Task 6 登记；en.json「Log approx」命名已随 Task 9 落地；
  显式路径负 generation_ms 透传待后续。
- Task 2：per-file 8MiB 冷启动窗；永久撕裂的尾块在 v1→v2 升级后不恢复（有界恢复按现状字节界）。
- Task 4：摘要集上限 128（裁决 R2a）；超限跨文件去重交给 DB UPSERT。
- Task 5：cursor 体积裁剪规则（安静超窗读尽文件剪枝）；被剪文件回窗后冷启动重进、重叠事实
  靠 UPSERT 收敛。
- Task 7：隐藏页 BillingAssignmentDialog 等对话框内查询不随 active 门控（见上，属终审）。
- Task 8：原生 reduced-transparency 实机未验证（见 §8）。
- Task 10：aria-live 与部分桶 UX 细化留终审。
- 旧中文诊断串（workbuddy.rs / codex.rs SOURCE_LIMITS）在错误码棘轮 `ffd4872f` 之外，
  按棘轮约定不再新增、存量随终审清点。

## 10. 结论

- 统一门禁 8 条全部实际执行且退出码 0（全量 vitest 首轮 1 例与本题无关的既有偶发，
  独立与全量重跑均绿，已如实记录）。
- Spec §11 验收表 8 行全部有测试/截图证据支撑；四条整链反例（WB 旧 EOF 恢复、Codex 仅响应、
  混合污染、旧 usage 独立与清理作用域）落在 `src-tauri/tests/tps_speed_acceptance_test.rs`
  （4 例），前端整链落在 `tests/speed/speed-acceptance.test.tsx`（6 例）。
- 验收未迫使任何生产代码修改——Task 1–12 的接线在整链反例下成立； brief 断言三行
  （`assert!(codex.response.value.is_some());` /
  `assert_eq!((codex.stream.value, codex.avg_first_token_ms), (None, None));` /
  `assert_eq!(old_usage_after, old_usage_before);`）逐字在场并通过。
- 未自动合并、推送、发布、打 tag；MASTER-PLAN、README、`docs/capabilities.json`、模型/API
  配置零改动；未触其他 worktree 与用户进程。

## 11. 终审修复波（2026-10-26 · whole-branch review 2 Important + 4 Minor）

整支评审（HEAD `19eafe5b`）返回 2 条 Important 与 4 条廉价 Minor；本节为一次性修复波的
修法、回归与门禁实录（单提交，仅涉本题路径；其余 minors 仍按 §9 台账留后续）。

### 11.1 Important 1 —— 排序依据未随临时回退切换（Codex 回退 + sort=rate）

**修法**（裁决 = option B）：`SpeedDashboard` 中当显示模式因来源能力临时回退
（`effectiveMetric(偏好, 选中能力) ≠ preferredMode`，仅 Codex stream 偏好→response 显示
一态）时，以显示口径 `sortSeries(shown.series, sort, mode)` 客户端重排**列表数据**
（view.ts 与后端 `sort_series` 同语义、已有独立测试）；查询载荷不改——仍传
`preferredMode`，后端排序语义保持单一来源。已申报例外：仅临时回退分支触发，
`effective === preferred` 的正常分支仍直出服务端顺序（`SpeedDashboard.tsx` listSeries
注释 + 文件头口径段双处声明；`SpeedSeriesList` 本身不重排的纪律不变，prop 文档同步）。
实现形态为直排计算而非 `useMemo`：React Compiler 的
`react-hooks/preserve-manual-memoization` 会拒绝该手动记忆化（把 const 原语 `mode`
误报为 "may be modified later" 而跳过整组件优化）；正常分支直出引用零开销、回退分支
重排为组合数量级小排序，逐渲染重算可忽略。

**测试**（`tests/speed/speed-dashboard.test.tsx` 新增 1 例）：Codex 选中 + stream 偏好 +
sort='rate' ⇒ 列表渲染顺序与 response 值降序逐行一致（62.1→61.3→58.8→55.6→47.9→41.3→33.2，
Codex 不再因 stream=null 被钉末位、按其 response 值落位）；切回原生源 ⇒ 恢复服务端
stream 降序直出（与 mock 原序一致）。

### 11.2 Important 2 —— Codex 双倒序封口 → 响应窗跨响应重叠

**修法**（`codex/decoder.rs`）：观察**成功提交**后把该文件的 `boundary_ts` 置 None
（封口即阶段边界）——下一响应只认新鲜输入边界，缺失以既有 `missing-input-boundary`
诊断保守跳过，绝不复用封口前的旧边界（否则双倒序序列把上一响应的生成段+间隔整个并入
下一响应窗：时长虚高、速度低估）。只在提交时清、不在封口行到达时清（倒序配对需要封口
前的边界结算生成项）。与未决封口（pending seal）机制的交互核对结论：延迟级联里更早
响应的中间提交发生在**同一封口行内、先于新未决保存**，故 `PendingSeal` 增加
`start_ts` 字段（`#[serde(default)]` 兼容旧游标状态），在封口行到达时（resolve 之前）
捕获边界快照——未决封口凭这份自捕获上下文结算，不受清空影响。结算起点按关闭者分两路：
**封口行关闭**（延迟写序）用捕获快照（快照缺失回退生成项冻结起点——旧状态无缝升级）；
**输入边界关闭**要求生成项自冻结的新鲜起点（有中间提交则 None → 保守跳过）。原
`PendingSeal::commit_with` 改为共用自由函数 `commit_response`（正常序/两路结算同体），
清位在成功 push 之后。

**测试**：新增夹具 `tests/fixtures/tps-v2/codex-double-reverse-seal.jsonl`（同族脱敏规则：
合成 `resp_`+50 hex id、时间锚定整体平移、零正文）+ 解码器回归
`double_reverse_seal_never_reuses_pre_seal_boundary`：R1 倒序配对不受清位影响
[+200,+1100]；R2（生成项先于工具返回₂到达）以 missing-input-boundary 保守跳过、身份未
消费；R3 从新鲜工具返回边界 [+2000,+2100] 起算；显式断言 `r3.start ≥ r1.end` 不重叠。

### 11.3 Minor 3/4/5/6（廉价项就地修复）

- **Minor 3**：`sources/codex.rs` `CodexFileCursor.decoder` 字段文档「有界 512」→
  「有界 128（decoder 的 DIGEST_CAP）」。
- **Minor 4**：`sources/zcode.rs` 模块文档「缺 F 的（工具参数）响应跳过速度」→
  「缺 F 的完整调用为 response-only 有效，流式口径跳过（响应窗照入）」；诊断串
  「completed 行缺 first_token_at → 无生成窗，跳过速度」→「…跳过流式速度，响应窗照入」。
  无既有测试断言旧串（断言面零改动）；`node scripts/check-error-codes.mjs` exit 0
  （311 码，改串不改计数）。
- **Minor 5**：`SpeedSeriesList` 行级 testid `speed-rate-${tool}` / `speed-sessions-${tool}`
  撞名 → `speed-rate-${seriesTestSuffix(seriesKey)}` 派生：`seriesKeyId`（JSON 串）经
  FNV-1a 32 位 hex 安全化（8 位 `[0-9a-f]`，无 querySelector 敏感字符；跨快照稳定，
  安全化选型已在注释声明）。全库 grep 无测试/代码引用旧 testid，无需更新断言面。
- **Minor 6**：§7 复用行「速度页自 f56615dc（=HEAD）」→「速度组件自 f56615dc 未再改动
  （初版定稿时 HEAD 为 19eafe5b，修复波后 HEAD 见本节）」；本节即修复波小节。

### 11.4 修复波门禁实录（全部实际执行）

| #   | 命令                                                                                                                                                                        | 退出码 | 结果                                                                                                                                                                                                                                                   |
| --- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| 1   | `cargo test … --test tps_codex_decoder_test --test tps_codex_source_test --test tps_speed_dashboard_test --test tps_speed_acceptance_test --test tps_native_secondary_test` | 0      | 9 + 18 + 18 + 4 + 19 = 68 passed / 0 failed（decoder 8 既有原样通过 + 1 新回归；source 18 原样通过，含延迟级联/封口对/门控停批三处与清位交互的用例）                                                                                                   |
| 2   | `cargo test … --lib`                                                                                                                                                        | 0      | 2985 passed / 0 failed / 38 ignored（既有 ignore）                                                                                                                                                                                                     |
| 3   | `pnpm exec vitest run tests/speed tests/workspace`                                                                                                                          | 0      | 7 文件 92 passed / 0 failed                                                                                                                                                                                                                            |
| 4   | `pnpm exec vitest run tests/`（全量）                                                                                                                                       | 0      | **首轮实录**：1 failed（`tests/mobile/SessionDetail.test.tsx`「书签跨加载窗口跳转」，与本轮改动零耦合——本轮未触 mobile 域；该文件独立重跑 93 passed 全绿，全量重跑 193 文件 2321 passed / 5 skipped / 0 failed），判定为并行 worker 既有偶发，如实记录 |
| 5   | `node scripts/check-error-codes.mjs`                                                                                                                                        | 0      | 311 码对账通过（zcode 改串不改计数）                                                                                                                                                                                                                   |
| 6   | `pnpm check`                                                                                                                                                                | 0      | 通过（首轮实录：SpeedDashboard 的 useMemo 触发 react-hooks/preserve-manual-memoization 1 error，见 11.1 改为直排计算后通过；新 Rust 代码经 `cargo fmt` 归位后 fmt check 通过）                                                                         |
| 7   | `cargo fmt --all -- --check`                                                                                                                                                | 0      | 通过                                                                                                                                                                                                                                                   |
| 8   | `cargo clippy --all-targets -- -D warnings`                                                                                                                                 | 0      | 0 警告                                                                                                                                                                                                                                                 |

提交：`fix(speed): follow the effective mode when sorting and bound Codex response windows`
（单提交；只 stage 本题 9 个路径：decoder.rs、codex.rs、zcode.rs、SpeedDashboard.tsx、
SpeedSeriesList.tsx、speed-dashboard.test.tsx、tps_codex_decoder_test.rs、新夹具
codex-double-reverse-seal.jsonl、本报告；未触碰 preserve-list 未跟踪文件，无
git clean/reset，未触 `/Users/jarvis/Documents/tuvis-worktree`）。

## 12. 第二版复审响应（048f3441 之后）

第二支评审（整支，2 必修 + 3 护栏 + 4 顺手项）一次性修复；逐条处置如下表。
基线 HEAD `e7afb5d4`（已并 main `00167e7a`）。

| #    | 发现                                                                                                                                                                                                | 处置                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| ---- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| D1   | 窄窗指标 2×2 断点与列表不一致：`SpeedMetrics` 用 `md:grid-cols-4`，列表在 `lg:` 收展 → 800px 主窗出现「列表已收、四格横排」混合形态，违反 Spec §2.8                                                 | `md:grid-cols-4` → `lg:grid-cols-4`（基线 2×2 不变），格内断点与 `SpeedSeriesList` 的 `lg:flex`/`lg:hidden` 对齐；`tests/speed/speed-chart.test.tsx` 新增类级锁定用例（jsdom 无法做真实响应式布局，锁 className：`grid-cols-2` + `lg:grid-cols-4` 在场、`md:grid-cols-4` 不在场）；800×600 窄窗截图双主题重拍并覆盖旧文件（`speed/light\|dark-800x600-speed-narrow.png`，收起列表 + 2×2 可视核对）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| D2   | 标题栏用量按钮实际保留（点击直达模型用量页签），但 zh 手册写「已收进」、spec 写「移入」、`main-title-bar.tsx` 注释仍描述独立窗口                                                                    | 采评审建议裁决 = 保留按钮、文档对齐现实：zh 手册 §11「原标题栏的用量按钮保留在标题栏，点击直达模型用量页签」；en 手册同步 "the usage button in the title bar remains and now opens the Usage tab directly"；spec §2.3 首句「移入」→「指向」（设计文档修订系第二支评审指示，在此申报）；标题栏注释改写为当前现实（经 `openUsageDashboard` → 主窗导航至模型用量页签，不再创建独立 usage-dashboard 窗口，已核 `src/lib/usage/openWindow.ts` 实际行为）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| B1   | WorkBuddy `started_at_ms = ended_at_ms − total` 无下界：宿主写 `total > 行时刻` → 负起点撞库表 `CHECK(started_at_ms >= 0)` → 整批 Err → 游标不推进 → 同批重放死循环                                 | `checked_sub(total)` + 非负过滤；None/负值 → ASCII 诊断 `negative-start field=total_cost_ms record-skipped`、走既有跳过路径（游标照常越过该行，批仍 ok）。回归：`tps_workbuddy_v2_test::negative_start_from_oversized_total_is_skipped_and_cursor_advances`（total=9e15 ms 坏行 + 好行同批：坏行跳过带诊断、好行入批、游标推进到文件尾、`commit_batch` 零错误、仅好行落库）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| B2   | Codex 封口→生成→封口→生成→封口且中间无输入/工具返回行：未决封口捕获的 `start_ts` 是上一响应已消费的旧边界，`.or(生成项冻结起点)` 救不回 → 发射重叠窗（时长虚高、速度低估）——终审 Important 2 的残余 | `CodexDecoderState` 增加 `last_committed_end: Option<i64>`（`#[serde(default)]`，旧游标零迁移加载为 None）：`commit_response`（唯一共用提交路径）发射前若候选起点 < 护栏 → `stale-start-after-commit start_ms=… last_end_ms=… skipped` 保守跳过（身份不消费）；成功发射后护栏前进为该观察终点；任何跳过不回退、`invalidate_inflight` 不清（已发射事实不因在途丢弃而消失）。回归：新夹具 `codex-triple-seal.jsonl`（同族脱敏：合成 `resp_`+50 hex、时刻锚定平移、零正文）+ `tps_codex_decoder_test` 两例——`triple_seal_without_boundary_rows_never_emits_overlapping_windows`（仅 R1 [+200,+500] 发射，S2/S3 身份未消费，诊断带 start_ms/last_end_ms）与 `monotonic_guard_survives_serde_round_trip`（护栏跨批次 serde 往返持久；旧游标缺字段可加载）。**既有用例修订 1 处（如实申报）**：`tps_codex_source_test::seal_pair_at_emit_cap_resumes_without_loss` 原期望第二封口 9002 发射 [t, t+900]——该窗恰是本修复禁止的重叠窗（与 9001 的 [t, t+700] 完全重叠），旧断言编码的是被修行为；现改为 9001 发射 + 9002 带诊断保守跳过 + 库 200 行，停批检查点/跨轮无损/零重放断言原样保留。其余 decoder/source 用例全部原样通过 |
| B3-1 | 死 i18n 键 `speed.responseBadge`（zh/en 各一，全库零消费，已 `rg` 确认）                                                                                                                            | 双语删除（`pnpm check:i18n` 键集一致通过）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| B3-2 | 前端 `sortSeries` 稳定回退含 `method` 层，后端 `sort_series` 比较器是 tool→model→channel 三元组——前端自造了后端没有的并列层                                                                         | 对齐后端：删 `method` tiebreaker，注释声明规范比较器 = tool→model→channel（稳定排序，服务端 SQL `ORDER BY …, method` 的同键相对次序在并列时保留）；`tests/speed/speed-data.test.tsx` 注释/用例名同步，并新增锁定用例（仅 method 不同的并列键保输入稳定序）。既有断言无一依赖 method 序（逐条核对），非注释-only 降级                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| B3-3 | 陈旧注释：`lib.rs` 导航命令处写「队列状态在 invoke_handler 之后的 .manage 注册」，实际 `.manage(NavigationQueue)` 在 `invoke_handler` 之前                                                          | 注释改为实况（上方先于 `invoke_handler` 的 `.manage` 注册）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| B3-4 | `v2/sums.rs` 「可靠模型用时池（成功或可计时失败/取消；native>0 优先，否则显式响应窗）」与 SQL 实况不符                                                                                              | 核对 `dao/speed.rs` `model_ms_val`（CASE：`native>0` 直接取——**无 outcome/输出门槛**；否则显式响应窗有效才取（>0 且不短于生成窗，同样不分成败）；双缺 = missing_timing；旧兼容分支在 ELSE 内不可达）与矩阵契约测试的 Rust 镜像（`native(>0).or_else(response_sample_ms)`），注释改为「不分成败：native>0 直接取（无 outcome/输出门槛）；否则显式响应窗有效才取（>0 且不短于生成窗）；双缺 = missing_timing，不填 0」                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |

### 截图（browser-mock，800×600 重拍覆盖）

`speed/light-800x600-speed-narrow.png` 与 `dark-800x600-speed-narrow.png`：均可见组合列表收为
「选择组合」选择入口 + 四格 2×2（当前模式速度/首 Token 平均等待 · 累计用时/工具累计用时），
不再有 md 区间的混合形态。**环境备注（如实申报）**：本机浏览器 mock 里点一级页签未切页
（mock 的事件回放在 StrictMode + @tauri-apps/api 2.12 `unregisterListener` 缺失下失灵，
`workspace_request_navigation` 已入队但监听不达）——截图经直接驱动同一模块实例的
workspaceStore 完成；该 quirk 与本题改动无关（导航行为由 `tests/workspace/workspace-navigation.test.tsx`
9 例在 vitest 层锁定且全绿），留作 browser-mock 已知问题。

### 门禁实录（全部实际执行，退出码全 0）

| #   | 命令                                                                                                                                                                                                                                | 退出码 | 结果                                                                                                                                      |
| --- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------ | ----------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | `node scripts/check-error-codes.mjs`                                                                                                                                                                                                | 0      | 311 码对账通过（新增诊断全 ASCII token，workbuddy.rs / decoder.rs 棘轮计数不升）                                                          |
| 2   | `pnpm check`（format + lint + i18n + errcode + build）                                                                                                                                                                              | 0      | 通过（首轮实录：view.ts 单行链式比较器触发 prettier 1 warn，`--write` 归位后全绿）                                                        |
| 3   | `pnpm exec vitest run tests/speed tests/workspace`                                                                                                                                                                                  | 0      | 7 文件 94 passed / 0 failed（+2：类级断点锁、method 不参与排序锁）                                                                        |
| 4   | `pnpm exec vitest run tests/`（全量）                                                                                                                                                                                               | 0      | 193 文件 2323 passed / 5 skipped / 0 failed（首轮即全绿）                                                                                 |
| 5   | `cargo test … --test tps_workbuddy_v2_test --test tps_codex_decoder_test --test tps_codex_source_test --test tps_speed_dashboard_test --test tps_speed_acceptance_test --test tps_acceptance_test --test tps_native_secondary_test` | 0      | 12 + 11 + 18 + 18 + 4 + 19 + 5 = 87 passed / 0 failed（workbuddy +1、decoder +2 新回归；codex source 18 原数量含 1 例语义修订，见 B2 行） |
| 6   | `cargo test … --lib`                                                                                                                                                                                                                | 0      | 2985 passed / 0 failed / 38 ignored（既有 ignore）                                                                                        |
| 7   | `cargo fmt --all -- --check`                                                                                                                                                                                                        | 0      | 通过                                                                                                                                      |
| 8   | `cargo clippy --all-targets -- -D warnings`                                                                                                                                                                                         | 0      | 0 警告                                                                                                                                    |

未触碰：MASTER-PLAN、README、`docs/capabilities.json`、模型/用量账本/旧游标；未自动合并、
推送、发布、打 tag；preserve-list 未跟踪文件原样；未触 `/Users/jarvis/Documents/tuvis-worktree`
与 `/Users/jarvis/Documents/tuvis-worktree2` 的进程与目录（截图用的 Playwright 临时产物落在其
`.playwright-mcp/` 会话输出目录，属 MCP 工具自身工作区，未改其中任何被跟踪/保留文件）。

## 13. 用户裁决记录（2026-10-27，设计会话第二版复审后）

两项复审列为「用户裁决点」的事项已由用户裁决；两者均不要求改动代码，仅更新记录。

- **MASTER-PLAN D16「桌面主窗维持现状」的解释**：用户裁决其含义为——**消息看板与工具资源两个
  sheet 维持原样，模型用量与输出速度作为新增 sheet 加入同一主窗口**。与实施核对：Task 7 保留了
  两页的基本布局与操作流程（Spec §2.5），模型用量/输出速度以新增页签接入；两页仅按 Spec §1/§3.3
  统一了页头组件与主题 token，未改业务字段、资源双视图与操作流。实施与该解释一致，无需改动。
  （此前控制器在预检中以「D16 针对三期大看板操作台、四页宿主只收编既有功能」判定不冲突——结论
  相同，现以用户解释为准。）
- **合并顺序（复审 §3-C2）**：用户裁决 **TPS 与界面改动同一 PR 一并提交**（见 §2）。本分支现状即
  为该 PR 的完整内容：TPS 基线 `6b9fc9b5` → V2 全部任务 → 两轮复审修复 → 已并 main `00167e7a`。
  代价如复审所述：PR diff 含整个 TPS 基线、TPS 不能单独回滚；由用户知情接受。

## 14. scoped 评审 4 条 Minor（随 PR 带上）

第二支评审的 scoped 复核返回 4 条非阻塞 Minor，裁决随 PR 一次带上（基线 HEAD `889c578f`，
单提交）。逐条处置：

| #    | 发现                                                                                                   | 处置                                                                                                                                                                                                                                                        |
| ---- | -------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| ①    | WorkBuddy 零起点边界无用例锁：守卫 `Some(s) if s >= 0` 下 `total == 行时刻` ⇒ `started_at_ms == 0` 合法放行，未来误收紧为 `s > 0` 不会有测试报红 | `tps_workbuddy_v2_test` 新增 `zero_start_boundary_total_equals_line_time_is_admitted`：`total_cost_ms` 恰等于行前缀时刻（字符串三件套 + 成功收尾），断言观察照常发射、`started_at_ms == 0`、无 `negative-start` 诊断、相邻正常行照常入批（批 2 条）、游标推进到文件尾、落库成功（CHECK(started_at_ms >= 0) 恰等通过） |
| ②    | `last_committed_end` 升级空窗：旧游标经 serde default 首轮为 None，若恰带 stale 待决快照，至多漏防一窗   | 纯注释（`decoder.rs` 字段 doc 补「升级空窗」如实申报：一次性有界，次轮起护栏生效）；逻辑零改动                                                                                                                                                                |
| ③    | D1 类锁只锁指标网格，列表两侧（`hidden…lg:flex` / `lg:hidden`）未锁，断点仍可各自漂移                    | `speed-chart.test.tsx` 类锁改双向：同一条用例真实 props 渲染 `SpeedSeriesList`，断言桌面列表容器（`speed-series-list`）含 `lg:flex` 且不含 `md:flex`、窄窗选择器容器（`speed-series-select` 外层 div）含 `lg:hidden` 且不含 `md:hidden`——任一侧漂离 `lg:` 报红 |
| ④    | 前后端比较引擎差异未申报：JS 按 UTF-16 码元、Rust 按字节序，仅对纯 ASCII 等价                            | 纯注释双向补齐：`view.ts` `stableSeriesCompare` doc 加「比较引擎备注」；`v2/query.rs` `fallback_cmp` doc 加同款中文备注指向同一假设（现实键均 ASCII；非 ASCII 键两引擎并列序可能不同 = 已知近似，注释即契约）                                                    |

### 门禁实录（全部实际执行，退出码全 0）

| #   | 命令                                                                                                                                                          | 退出码 | 结果                                                                                              |
| --- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------ | --------------------------------------------------------------------------------------------------- |
| 1   | `cargo test … --test tps_workbuddy_v2_test --test tps_codex_decoder_test`                                                                                       | 0      | 13 + 11 passed / 0 failed（workbuddy +1 零起点边界用例）                                            |
| 2   | `cargo test … --lib`                                                                                                                                            | 0      | 2985 passed / 0 failed / 38 ignored（既有 ignore）                                                  |
| 3   | `pnpm exec vitest run tests/speed`                                                                                                                              | 0      | 5 文件 79 passed / 0 failed（类锁改双向，用例数不变）                                               |
| 4   | `node scripts/check-error-codes.mjs`                                                                                                                            | 0      | 311 码对账通过                                                                                      |
| 5   | `pnpm check`                                                                                                                                                    | 0      | 通过（首轮实录：speed-chart.test.tsx 1 处 prettier warn，`--write` 归位后全绿；workbuddy 新用例经 `cargo fmt` 归位后 fmt check 通过） |
| 6   | `cargo fmt --all -- --check`                                                                                                                                    | 0      | 通过                                                                                               |
| 7   | `cargo clippy --all-targets -- -D warnings`                                                                                                                     | 0      | 0 警告                                                                                             |

## 15. 实机反馈轮（2026-10-27）

用户在本分支实机（Tuvis v0.6.2 四页签）截图反馈 7 条；根因与裁决见
`.superpowers/sdd/2026-10-26-desktop-workspace-speed-v2-plan/feedback-round-2026-10-27.md`。
本轮只改前端 + 文档 + 一行 tauri.conf，**未触碰 `src-tauri/src/**`**（用户 tauri:dev 跑在本源根且
写真实账本，采集域纪律禁止其运行时改采集 Rust；未跑 cargo）。Batch 1 落地 #1 / #3b / #6 / #2：

| # | 现象 | 根因 | 修复 | 测试 | 截图 |
|---|---|---|---|---|---|
| 1 | 会话卡首行项目名被挤成「tu…」、会话名「##…」；长分支尾部可能压到状态点 | 首行塞了徽标+项目名+会话名尾巴+分支，分支 `shrink-0` 不截断吃掉整行宽；首组无 overflow-hidden | 会话名移**独立第二行**（mono 次文字、truncate、title 全文；删 `@min-[374px]` 隐藏与 `[flex-shrink:5]` 尾巴 hack）；首行只留 徽标+项目名+配对歧义图标+分支；分支改 `min-w-0 truncate [flex-shrink:5]`（先让路）+ `title`；首组加 `overflow-hidden pr-1`（溢出裁在状态点之前）；项目名 JS 预算 15 不变 | `sessionCardHeader.test.tsx` 结构断言改两行形态 + 新增 3 例（尾巴独立行 / 分支 truncate+min-w-0 无 shrink-0 / 首组 overflow-hidden）；既有语义断言（配对图标、未读环、dismiss）全保留 | `feedback/messages-two-row-{wide,narrow}.png`（1419×800 / 800×600，浏览器 mock；mock 增补一张长项目名+长分支名卡） |
| 3b | 用量页左侧竖排「模 型 用 量」 | 页头 actions 槽 `shrink-0`、标题容器可收缩：导出工具栏超宽时 h1 被挤到 1 字宽、CJK 逐字换行 | `WorkspacePageHeader` 标题块 `shrink-0` + h1 `whitespace-nowrap`；actions 槽 `min-w-0 flex-wrap justify-end`（动作换行不挤标题）；`UsageExportActions` 根 div 与「已保存路径」反馈条加 `min-w-0`（truncate 链穿透 flex） | `workspace-theme.test.tsx` 用例 6：真实用量页挂载点类锁（jsdom 不做换行，类锁即机制）——标题块 shrink-0 / h1 nowrap / 动作槽 flex-wrap+min-w-0，并断言 h1 文案=页标题；其余三页头部截图目验正常 | `feedback/usage-header-1280.png`（1280×800：标题完整横排、工具栏整行放下） |
| 6 | 采集管理 opencode/claude 动作写明插件 | `speed.sources.actionInstall/Uninstall` 文案为「接入」/「卸载」 | zh 改「安装插件」/「卸载插件」、en "Install plugin"/"Uninstall plugin"；细节区文案保持准确不改（Claude 实为统计配置） | speed 套件用 testid 不锁文案，无断言需改（grep 旧串仅命中注释与他键状态标签）；i18n 键集门禁通过 | —（按钮文案，截图无信息增量） |
| 2 | 用量看板需竖向展示；问默认几乘几 | 原 800×600 横向、无 min | `tauri.conf.json` main：860×900 + `minWidth:800 / minHeight:600`；改 conf 使 tauri dev 重启用户应用一次（按批次放在最后一步） | 无（纯配置）；`pnpm check` 覆盖 json 解析 | — |

门禁实录（全部实际执行）：定向 vitest（sessionTitleAndPairing / projectNameBudget / workspace /
speed / session / pet）72 文件 540 passed；全量 `pnpm exec vitest run tests/` 193 文件
2327 passed / 5 skipped（既有 skip）；`pnpm check` 退出码 0（i18n 键集 + 错误码棘轮 + build 含内）。
提交拆两笔：`fix(ui)`（#1+#3b+#6+测试+截图）、`chore(window)`（#2+CHANGELOG zh/en `[Unreleased]`
升级须知条目，CHANGELOG 手工排版不入 prettier）。

**如实申报（浏览器 mock 环境问题，非本轮改动引入）**：截用量页时发现浏览器 mock 里四页签点击
导航事件链不生效（`tuvis-workspace-navigation` 事件派发未达已注册回调；`_unlisten` 处
`__TAURI_EVENT_PLUGIN_INTERNALS__` 缺失的 TypeError 噪音同源）——本轮未触碰任何事件/导航文件，
jsdom 导航套件 9 例全绿、mock parity 绿，判为浏览器 mock 与 `@tauri-apps/api` 2.12 的既有偏差；
截图改经 store 模块 `setState` 直切页签（等效呈现目标页头），真机不受影响。

Batch 2 落地 #4a / #4b / #4c / #5（速度页四条，同日实机反馈轮续批）：

| # | 现象 | 根因 | 修复 | 测试 | 文档与截图 |
|---|---|---|---|---|---|
| 4a | 竖版（<lg）速度页不显示排名，只有下拉选择器 | `SpeedSeriesList.tsx`：行表 `hidden … lg:flex`、<lg 渲染 `<select>`（spec §2.8 原设计「窄窗收为选择入口」） | 行表两布局**同一份 DOM**：<lg 限高滚动（`max-h-56 overflow-y-auto`）、≥lg 解除限高铺满（`lg:max-h-none`，保留 `min-h-0 flex-1`）；删除下拉选择器整块与「选择组合」标签（i18n `speed.list.selectSeries` 键随之移除，zh/en 同步） | `speed-chart.test.tsx` 类锁改写：列表容器无 `hidden`、带 `max-h-56` + `lg:max-h-none`、不漂 md:；`speed-series-select` 断言不存在；指标 2×2（`grid-cols-2` + `lg:grid-cols-4`）锁不变；dashboard 套件补「窄宽同一份 DOM」用例 | spec §2.8 改「窄窗把组合列表以紧凑排名表置于详情上方（限高滚动）」（用户指示的措辞）；`feedback/speed-narrow-ranked-list-800x600.png`（800×600：排名表在详情上方、2×2 四格、近5小时在位） |
| 4b | 默认落在近 1 小时 | `SpeedDashboard.tsx` `useState<SpeedRange>("1h")` | 默认改 `"5h"`（注释与档位行注释同步） | `speed-acceptance.test.tsx` 两处 + `workspace-navigation.test.tsx` 一处默认断言改 `speed-range-5h`；两处用例标题 1h→5h | spec §2.6「速度首次默认近5h」、§7 表默认行移到近5小时；CHANGELOG zh/en `[Unreleased]` 输出速度条目「默认近5小时 / 5 h by default」（就地改，不入 prettier）；手册 11-usage zh/en 各两处默认值；截图 800×600 内近5小时在位 |
| 4c | 口径只有按钮没有解释 | 模式切换区无任何口径说明 | 两模式按钮加 `title`（stream 启用时=口径定义，禁用时保留能力缺口解释；response 恒=口径定义）+ 切换区下一行 12px 次文字整句说明（i18n `speed.modeHint.stream/response/explainer` 三键，zh/en）；指标计算一字未动 | `speed-dashboard.test.tsx` 新用例：`speed-mode-hint` 在场且含「差额即等待 / Codex 仅有响应口径」、两按钮 title 各含口径定义 | `feedback/speed-mode-hint-1280.png`（1280：切换区下方整句说明可见） |
| 5 | 横版组合名常截断；无悬浮全名；排序缺 组合名/速度/会话数 升降序 | 列轨 `minmax(240px,300px)` 偏窄；名单元格 truncate 无 `title`；排序仅 tool/model/rate 无方向，且列表「服务端直出」纪律与 fallback 口径冲突 | 列轨 `minmax(280px,340px)`；名单元格 `title` 全文（保留 truncate）；`view.ts` 扩展 `SpeedListSortKey(name/rate/sessions)` + `SortDirection` + `defaultDirectionFor`（name 升序、rate/sessions 降序），`sortSeries` 加可选 direction——rate **null 无论如何排末**、sessions 并列落稳定序、旧三参签名与 tool/model wire 键行为逐字保留（mockFixtures 仍用）；UI 排序三项 组合名/速度/会话数 + 升降序按钮（`aria-pressed`、↑/↓+升序/降序文字），切键归位默认方向；**列表恒客户端排序**（`sortSeries(raw, key, 显示口径, direction)`），查询载荷 `sort` 恒发 `"tool"` 作服务端稳定基序（Task 12「服务端直出」例外扩大为恒客户端排序，两处头注释申报；`model` 选项从 UI 移除、枚举保留） | `speed-data.test.tsx` 新 4 例：defaultDirectionFor、name asc/desc（desc 翻转整个比较器含 null 渠道规则）、rate 双向 null 恒末、sessions 双向稳定并列、缺省方向=旧语义；`speed-dashboard.test.tsx` 重写排序用例：恰三项、载荷恒 `sort:"tool"`、方向翻转行序可逆、切键归位默认方向、行名 `title`=全名、左列轨 `minmax(280px,340px)` 类锁、fallback 用例改「恒按显示口径重排」（期望行序不变）；`speed-chart.test.tsx` 类锁 props 更新 | spec §8.1 改「按组合名/当前模式速度/有依据的会话数排序，可升序或降序；默认组合名升序（工具→模型→渠道稳定序），速度与会话数默认降序，无数值一律排末」（用户指示的措辞）；CHANGELOG zh/en 排序措辞同步；手册 11-usage zh/en 排序措辞同步；`feedback/speed-wide-sort-sessions-desc-1419x800.png`（1419×800：会话数降序 + 加宽列轨；**行名悬浮全名靠 `title` 属性**——如 `Claude · claude-sonnet-4-5 · console-proxy`，静态截图无法呈现 hover 浮层，特此申报） |

门禁实录（全部实际执行）：定向 vitest（tests/speed + tests/workspace）7 文件 101 passed；
全量 `pnpm exec vitest run tests/` 193 文件 2333 passed / 5 skipped（既有 skip）；
`pnpm check` 退出码 0（i18n 键集 + 错误码棘轮 + build 含内）。单笔提交：
`feat(speed): ranked list on narrow windows, 5h default, metric hints and client-side sorting`。

**顺手修复（非本批条目）**：HEAD `b94abb9a` 的 `src/components/sessions/SessionCard.tsx`
过不了 `prettier --check`（Batch 1 提交时漏 format，本轮 `pnpm check` 首跑即红）——仅
`prettier --write` 格式化（4 行排版），无语义改动，随本笔提交带上。

**环境申报**：截图用自起 Vite（端口 5199，用完即停）；期间实机 dev 应用并未在跑
（brief 所列 PID 79498/79634/79684 均已退出，全程未启动第二个 tauri dev、未跑 cargo、
未触碰 `src-tauri/src/**`）。浏览器 mock 页签切换沿用 Batch 1 的 store `setState` 直切法。

Batch 3 落地 #3a（导出图模块可选，同日实机反馈轮续批）：

| # | 现象 | 根因 | 修复 | 测试 | 文档与截图 |
|---|---|---|---|---|---|
| 3a | 导出图版式固定：单一分组 Top6 + 工具格 + 立绘，无 API 费用段，无法选择画哪些段 | `exportLayout.ts` 固定段序；`UsageExportSettings` 仅评语/姿态且走 Rust 持久化 | 新增 `lib/usage/exportModules.ts`（`ExportModules = { distribution: UsageGroupBy[]; cost: boolean; work: boolean }`；localStorage key `tuvis.usage.exportModules`，先例 `lib/toolFilter`；坏值/缺值回落默认、distribution 只收四个合法 groupBy；**默认值 = 今日出图逐字一致**：`["tool"] + cost:false + work:true`）。`exportLayout.ts` 改动态段序：分组段变**数组**（每选中维度一段，锚点 id 仍 `groups`、附 `key` 维度；可选 `cost` 段插在分组后：标题+合计行+≤6 行（名称左/金额右/ACCENT_B 占比条），行语法与分组段同一套）；`work:false` → 工具格与立绘/气泡**整段缺席**（一个开关管两段），图在最后数据段后正常收尾；没有段就不出标题不出空盒。`useExportShare.ts` 装配：导出时刻读模块偏好；当前看板 groupBy 命中即**复用 `dash`（零 IPC）**，其余维度走既有只读命令 `usageDashboard`（**不新增命令**）；费用段与费用卡同源（`usageCosts`，行=Token 降序 Top6、金额按显示币种取后端原生值、总额=原始十进制串 BigInt 缩放求和——用户裁决「总额 + Top 行」，费用卡「无全局总额」契约只约束卡片本身）；任一段取数失败该段缺席不阻断；`work:false` 不取图集、评语/姿态不进图。`UsageExportSettings.tsx` 加「出图模块」块：总览+趋势**勾死禁用**（带说明行）、四维度子项（复用 `usage.group.*`）、费用与工作小结两开关，**勾选即存 localStorage（无 Rust 往返）**+ 行内「已保存」；评语/姿态的 Rust 保存通路一字未动 | 新 `tests/usage/export-modules.test.tsx` 12 例：偏好纯层 4（默认/坏值/合法值过滤去重保序/往返+防共享引用）+ 布局动态段 5（**改版前锁定**：默认形状 anchors/步骤数 54/内容底 1466/总高 1560，取自改版前实现实跑冻结；两维度两段有序；费用段插位+ACCENT_B 条；work=false 无工具格无 sprite 无气泡底且内容底/总高收缩；空分组无锚点无标题）+ 弹层 3（总览勾死禁用、费用勾选即写 localStorage 且零 `usage_set_settings`、重开回读、分组切换）；既有 `usage-export-layout/image/csv(-actions)/settings` 按新输入形状适配（断言意图不变，如「0 值分布不画行」改断段在场但 `rows` 空） | 手册 11-usage zh/en 导出段各补「导出设置 → 出图模块」2 句（必选/可选/记在本机）；CHANGELOG zh/en `[Unreleased]` Added 各一条（手工排版，不入 prettier）；`feedback/export-settings-modules-dialog.png`（弹层模块块：总览勾死、四维度、两开关）、`feedback/export-image-all-modules.png`（四段分组 + 费用段合计 $0.001929 + 工具格 + 立绘，720×2454）、`feedback/export-image-overview-only.png`（仅总览+趋势+页脚，贴 `EXPORT_MIN_H`=1560 竖版下限——钉死版式常量未动，下方为空白卡面，如实申报） |

门禁实录（全部实际执行）：定向 vitest（tests/usage + tests/speed + tests/workspace + tests/pet）
85 文件 786 passed；全量 `pnpm exec vitest run tests/` **194 文件 2345 passed / 5 skipped**（既有 skip）；
`pnpm check` 退出码 0（prettier + eslint + i18n 键集 2606 键 + 错误码棘轮 + build 含内）。单笔提交：
`feat(usage): choose which modules the exported image includes`。

**申报三条**：① **文本导出一字未动**——`exportText.ts` / 复制文本 / CSV 全部保持原样，模块开关只影响分享图；② **模块偏好只存 localStorage**（`tuvis.usage.exportModules`），`usage_get_settings` / `usage_set_settings` 不接新字段、无 Rust 改动（本轮零 `src-tauri` 触碰、零 cargo）；③ **截图抓到并顺手修了一处真缺陷**：费用总额初版把 `formatNativeMoney` 的产物（带 `$` 符号与千分位）喂给求和正则 → 恒出「—」，已改为对原始 `estimatedUsd`/`estimatedCny` 串求和后格式化，截图复验 `$0.001929`。

**环境申报**：截图用自起 Vite（端口 5199，用完即停、PID 已杀）；浏览器 mock 里 `export_save_bytes` 不写真文件，故三张导出图经拦截 `HTMLCanvasElement.toBlob` 取 dataURL 落盘（与真机同一 `renderShareImage` 渲染路径）；期间未启动第二个 tauri dev、未跑 cargo、未触碰 `src-tauri/src/**`，保留清单（5 份 TPS 评审报告、`tuvis-setup-preview.html`、`.superpowers/`）原样未动。

Batch 4 落地 #7（深色配色改版，同日实机反馈轮续批）：

**用户裁决（原话）**：「黑夜模式太丑；黑白配色看起来不吉利；不喜欢纯黑白——就算黑白为主，也得加 2~3 种卡片或者标签色作为点缀。」雾银纯灰强调就此退役。

**主流深色配色参照（通识引述，非实时核实——未联网逐一开主题文件取证，仅凭一般知识概括）**：
VS Code Dark Modern（近黑 #1F1F1F/#181818 底 + 蓝 #0078D4 单强调）、GitHub Dark（#0D1117/#161B22 微冷底 + #58A6FF/#3FB950/#D29922/#BC8CFF 多语义点缀）、Linear（#191A23/#232433 带靛味底 + 靛蓝 #5E6AD2 强调 + 彩色标签）、Claude 桌面端（暖炭底 + 珊瑚 #D97757 强调）。共同模式 = **近黑微冷（或微暖）底 + 面板提亮一档 + 一个有彩主强调 + 语义色 + 彩色标签**——无一例用纯黑白或纯灰强调。新深色即按此模式重写；状态色/工具品牌色语义原样保留（它们本就是「点缀」的另一半）。

**旧→新 token（`.dark`）**：

| 旧→新 token | 旧值 | 新值 | 说明 |
| --- | --- | --- | --- |
| `--background` | `#1d1d1d` | `#1b1c20` | 页面/窗口背景（微冷石墨，不再纯灰） |
| `--card` / `--popover` | `#292929` | `#242630` | 内容面板（一步提亮、带靛味） |
| `--secondary` / `--input` / `--muted` / `--surface-secondary` | `#222222` | `#1f2128` | 输入/次级面 |
| `--border` | `#454545` | `#3a3d4a` | 边界 |
| `--primary` / `--ring` | `#c7c7c7` | `#8b95ff` | 强调/主图线 = 靛蓝（有彩强调，替代亮银） |
| `--primary-foreground` | `#1d1d1d` | `#10121a` | 强调色上的文字 |
| `--muted-foreground` | `#b8b8b8` | `#b8bcc8` | 次文字（冷灰，微提对比） |
| `--accent` / `--selected` | `#363636` | `#2c2f45` | 选中/悬停背景（靛蓝味） |
| `--usage-accent-a` | `#c7c7c7` | `#8b95ff` | 主图线 = 靛蓝 |
| `--usage-accent-b` | `#969696` | `#b49cff` | 辅图/渐变端 = 紫罗兰 |
| `--usage-track` | `#363636` | `#2c2f45` | 图表轨道对齐选中背景 |
| `--foreground` / 文字前景组 | `#f0f0f0` | `#f0f0f0` | 不变 |

**改动面**：`src/index.css` `.dark` 块逐 token 更新（注释同步改「2026-10-27 实机反馈：冷石墨 + 靛蓝/紫罗兰」，「状态色不抹平」备注保留）；destructive/oklch 状态色、玻璃键（派生自 `--card` 自动跟随）、浅色 `:root` 全部零改动。spec §3.1 深色列逐格更新 + 表后新增修订句（用户指示的设计稿修订，在此申报）；文档头状态行与 §3.1 引言的「雾银」措辞同步改为冷石墨，§3.2/§3.3 经核对无雾银措辞无需改。`tests/pet/usage-theme-tokens.test.ts` 深色取值表同步（accent-a `#8b95ff` / accent-b `#b49cff` / track `#2c2f45`）+ 注释更新。全库 grep 旧深色 hex（`#c7c7c7/#969696/#454545/#292929/#1d1d1d/#363636/#222222/#b8b8b8`）：除上述两文件外 `src/`、`tests/` 无其他钉值（浅色档无牵连，未触无关面）。

**WCAG 对比度重算**（同 Task 8 方法：sRGB 相对亮度 + `(L1+0.05)/(L2+0.05)`，node 脚本实算）：

| 配对 | 比率 | 判定 |
| --- | --- | --- |
| foreground `#F0F0F0` / background `#1B1C20` | 14.94:1 | ✅ ≥4.5 |
| muted-foreground `#B8BCC8` / background | 8.97:1 | ✅ ≥4.5 |
| muted-foreground / card `#242630` | 7.93:1 | ✅ ≥4.5（无需上调 `#BFC3D0`） |
| muted-foreground / selected `#2C2F45` | 6.92:1 | ✅ ≥4.5（无需上调 `#BFC3D0`） |
| primary `#8B95FF` / background | 6.36:1 | ✅ ≥4.5 |
| primary-foreground `#10121A` / primary | 6.98:1 | ✅ ≥4.5 |
| selected-foreground `#F0F0F0` / selected | 11.52:1 | ✅ ≥4.5 |
| border `#3A3D4A` / background（仅参考） | 1.58:1 | 边界线无 4.5 门槛 |

**截图**（browser-mock，非真机；深色经 `tauri-ui-theme=dark` 首帧缓存 + 页签经 workspaceStore `setState` 直切——mock 页签点击导航事件链不生效的既有环境问题，同 Batch 1 申报）：`feedback/dark-messages-1280.png`、`dark-usage-1280.png`、`dark-speed-1280.png`、`dark-resources-1280.png`（1280×800 四页）+ `dark-speed-narrow-800x600.png`（窄窗排名表 + 2×2 指标）。目验：冷石墨底 + 靛蓝主图线/选中底/激活档位在位，无纯灰死气；实机渲染未复核（未启动 tauri dev，属 §8 既有平台限制）。

门禁实录（全部实际执行）：定向 vitest（tests/pet tests/workspace tests/speed tests/usage）
85 文件 786 passed / 5 skipped（既有 skip）；全量 `pnpm exec vitest run tests/`
**194 文件 2345 passed / 5 skipped**（既有 skip）；`pnpm check` 退出码 0（prettier + eslint +
i18n 键集 + 错误码棘轮 + build 含内）。单笔提交：
`feat(theme): cool-graphite dark surfaces with indigo and violet accents`
（仅 stage 本题 9 路径：index.css、spec §3.1、theme-tokens 测试、本报告、5 张深色截图）。

Batch 3 补充（评审轮 Ready：1 Important + 2 nit + 1 文档顺带，全部落实）：

| 项 | 评审意见 | 修复 | 测试 |
|---|---|---|---|
| Important | 费用段 Top 6 之外**静默丢行**（X2 缺陷类）：分组段每段有 `groupsMore` 披露，费用段 `slice(0, 6)` 后什么都不说——可见行之和与 `total`（全部行合计）对不上且看不出来 | `ExportCostSection` 增 `more: string | null`（装配层 = `sorted.length − GROUPS_MAX`，为 0 ⇒ null，i18n 复用 `usage.moreN` 与分组段同一措辞）；`exportLayout.ts` 把「等 N」绘制提为 `paintMoreNote` 共用闭包（分组段 / 费用段同一款：FONT_TICK 小一档、左对齐、内容底 +22），费用段末行下照画；`useExportShare.ts::buildCostSection` 计算 `more` 并**导出给测试**（hook 本体的「只允许 `UsageExportActions` import」纪律不变，注释申报） | `tests/usage/export-modules.test.tsx`：布局级（`more: "等 2"` 逐字画出、位置在费用段内/工具格前、内容底恰 +22、总高贴公式；null ⇒ 不画）+ 装配级（`buildCostSection` 8 行 ⇒ `more` = `usage.moreN`(2) 且 `rows` 6、7 行 ⇒ +1、6 行 ⇒ null；`total` 恒覆盖全部行——8 行 × $0.01 = $0.08 不随 Top6 截断，正是披露的存在理由） |
| Nit A | `tests/usage/export-modules.test.tsx` 死代码 `const texts = textsOf(baseInput()); void texts;` | 删除（该用例重写为 `costOf(more)` 夹具，死代码自然消失） | 既有用例全绿 |
| Nit B | `SpeedSeriesList.tsx` 方向按钮 `aria-label` 代码内手拼全角「：」，en locale 读出全角冒号 | 新增 i18n 键 `speed.list.sortDirectionLabel`（zh「排序方向：{{direction}}」/ en "Sort direction: {{direction}}"），方向走 `{{direction}}` 插值，冒号归各 locale | speed 套件既有断言不锁该 aria-label，全绿；键集门禁通过（zh/en 同批） |
| 文档顺带 | 补记证据重拍提交 `3f054ce2` | 该提交 = 纯 evidence 重拍（速度页窄窗截图带上模式切换与口径提示入画），**无代码改动** | — |

门禁实录（全部实际执行）：定向 vitest（tests/usage + tests/speed）23 文件 355 passed；
全量 `pnpm exec vitest run tests/` **194 文件 2347 passed / 5 skipped**（既有 skip）；
`pnpm check` 退出码 0。单笔提交：`fix(usage): disclose rows beyond the export cost top-6`。

### Batch 5（第二轮实机反馈：行距/工具栏并行/导出口径与柱色/速度布局回横/口径三行）

用户对 Batch 1–4 成品的第二轮 4 条实机反馈（计划经用户批准后执行）；仍只改前端 + 文档 +
一行 tauri.conf（`src-tauri/src/**` 未触碰、未跑 cargo）。

| # | 反馈 | 修复 | 测试 | 文档与截图 |
|---|---|---|---|---|
| ① | 会话卡三行行距松、会话名不突出 | `SessionCard.tsx` 第二行会话名加 `font-semibold` + `mb-2`→`mb-1`（加粗拉近）；消息预览 `mb-2`→`mb-1.5`（line-clamp-2 / min-h / 文案不动）；首行 `mb-1` 保持 | `sessionCardHeader.test.tsx` 新增用例：标题行含 `font-semibold` + `mb-1` 无 `mb-2`、预览行 `mb-1.5`（结构锁照旧） | `feedback/session-card-two-row-tight-1419.png`（1419×800）+ `feedback/session-card-two-row-tight-800.png`（800×600），浅色 |
| ②a | 用量页工具栏占两行、页头吸顶压内容 | `UsageDashboardBody.tsx`：范围条行与看板/记录页签行合并为一行 `flex flex-wrap justify-between gap-2`（左=页签对、右=UsageRangeBar）；页头块去 `sticky top-0 z-10`（表头随内容滚动；**申报**：导出动作随滚动离开视口，用户裁决接受），保留 `bg-background border-b -mx-4 px-4 pt-4 pb-2`；合并行取 `items-start`（自选并申报：自定义日期展开时范围条向下长出三行竖排，页签对与 presets 行顶对齐比垂直居中稳定） | `v2-page.test.tsx` 新用例 17：页签对与范围条同处一个 `justify-between` 行、页签在前范围条在后（compareDocumentPosition）、页头块 `-mx-4` 在场且无 `sticky`/`z-10` | `feedback/usage-toolbar-single-row-1280.png`（1280×800：页签左 / 档位右同一行） |
| ②b | 导出图 token 单位与看板不一致（千分位 vs 万/亿）；分组柱全蓝无维度区分；费用柱实心与分组柱难辨 | `useExportShare.ts` hero / 指标行 token 位 / 分组行值 `fmtInt` → `fmtTokens(…, getLang())`（`format.ts` 「导出保留千分位」 carve-out 注释改为「2026-10-27 用户指示：导出图与看板同口径」，**文本导出（复制文本/CSV）千分位契约不动**）；`exportLayout.ts` 新增 `GROUP_ACCENTS = { tool:#3b82f6, project:#10b981, provider:#f59e0b, model:#8b5cf6 }`（`paintGroupSection` 按 `section.groupBy` 取色，同族同样式实心圆角条）+ 费用柱改**描边样式**（`COST_BAR_FILL` rgba(139,92,246,0.18) 淡紫填充 + `COST_BAR_STROKE` #8b5cf6 描边；`ExportStep` bar 变体加可选 `stroke`，执行层沿填充条同一圆角路径 `lineWidth 1.5` 描一圈，几何/总高不变）；**导出图保持固定浅色调色板**（不读 CSS 变量，theme-tokens 门禁照过） | `export-modules.test.tsx` 用例 3 改 pin（分组条 #3b82f6；费用条淡紫填充 + 紫描边）+ 新用例 3b（四分组维度各色、同段同色）；`tests/pet/usage-export-csv.test.tsx` hero/指标值 pin 改万/亿口径（203.70万 / 8.27万 / 63.61万）；`usage-export-layout.test.ts` 几何断言原样通过（描边不改几何） | `feedback/export-settings-modules-dialog.png`（重拍：全模块勾选态）；`feedback/export-image-all-modules.png` + `feedback/export-image-overview-only.png`（toBlob 拦截重拍，同 Batch 3 方法：四维度四色柱、费用段描边柱、指标行万/亿在画） |
| ③ | 窗口 860×900 竖版过高，窄窗速度页列表在详情上方不合理（回横 800×600） | `tauri.conf.json` main `width:860,height:900` → `800,600`（min 800×600 不变）；CHANGELOG.md / CHANGELOG.en.md 撤下从未发布的 860×900 升级须知两条；spec §2.8 「置于详情上方」→「置于详情下方」；`SpeedDashboard.tsx` 栅格两轨加 `order`：列表 pane `order-2 lg:order-1`（经 `SpeedSeriesList` 新可选 `className` prop 透传）、详情 section `order-1 lg:order-2`——窄窗（<lg）详情在上、列表在下，宽窗恢复左列表/右详情，**DOM 顺序不动** | `speed-chart.test.tsx` 类锁补 order 断言（pane 收 `className` 透传）；`speed-dashboard.test.tsx` 类锁补双轨 order 断言（pane order-2/lg:order-1 + 详情 section order-1/lg:order-2），max-h-56 / lg:max-h-none / 无 hidden / 左列轨 minmax 断言全保留 | spec §2.8 就地改词；CHANGELOG zh/en 删两条（未发布条目，CHANGELOG 手工排版不入 prettier）；`feedback/speed-narrow-detail-first-800x600.png`（800×600：详情在上、排名表限高在下）+ `feedback/speed-wide-1419.png`（1419×800：左列表右详情不变） |
| ④ | 口径说明挤在详情区按钮下方，应贴图例且分三行 | `SpeedChart.tsx` 加可选 `footnote?: ReactNode` 插槽（渲染在 `speed-chart-legend` 之后、卡底之前）；`SpeedDashboard.tsx` 删 `speed-mode-hint` 独立段落，改传 footnote 三行（新 i18n 键 `speed.modeHint.line1/line2/line3`，文案逐字用用户三行，zh/en 各自单语；`explainer` 单键随之删除）；模式按钮口径 title 保留 | `speed-dashboard.test.tsx` 口径说明用例改写：`speed-chart-footnote` 在场、三行 `<p>`、含三行口径文案、**DOM 顺序在图例之后**（compareDocumentPosition）、`speed-mode-hint` 不存在、两按钮 title 保留 | `feedback/dark-speed-1280.png`（深色重拍存档：三行口径脚注在图例下方入画） |

**如实申报**：① 评语气泡的 `{tokens}` 占位（`QuoteVars.tokens`）仍为 `fmtInt` 千分位——计划只列 hero/指标行/分组行三处，气泡不在枚举内，未越权改动；与 hero 万/亿同图并存如需统一待用户裁决。② 文本导出（复制文本 / Token CSV / 费用 CSV）完全未触碰。③ 导出图保持固定浅色调色板（深色主题不出深色导出图，既有契约）。④ 去吸顶后导出动作会随滚动离开视口（用户指示接受）。⑤ 截图证据目录沿用本计划既有根 `docs/superpowers/evidence/2026-10-26-desktop-workspace-v2/feedback/`（任务简报写的 `…-speed-v2/` 路径在该仓不存在）。⑥ 截图经自起 Vite（端口 5199，用完即停）+ 浏览器 mock，页签经 workspaceStore `applyNavigation` 直切（mock 页签点击事件链不生效的既有环境问题，同 Batch 1 申报）；导出图经 `toBlob` 拦截取 PNG（同 Batch 3 方法）。

门禁实录（全部实际执行）：RED 先行（7 条新/改断言对旧代码全红）→ 定向 vitest
（tests/speed tests/usage tests/session tests/pet tests/workspace）89 文件 812 passed / 5 skipped
（既有 skip）；全量 `pnpm exec vitest run tests/` **194 文件 2350 passed / 5 skipped**（既有 skip）；
`pnpm check` 退出码 0（prettier + eslint + i18n 键集 + 错误码棘轮 + build 含内）。

Batch 5 补充（评审轮 Ready：1 Minor + 1 Nit + 1 控制器裁决扩大 #2b，全部落实）：

| 项 | 评审意见 | 修复 | 测试 |
|---|---|---|---|
| Minor | 执行层 stroke 分支（`exportImage.ts` bar 用例）**零测试覆盖**：删掉整个 `if (step.stroke …)` 块没有任何测试会红——export-modules 只锁步骤数据，export-image 的 stroke 从未在 bar 用例断言 | `usage-export-image.test.ts` 新用例 6：假 ctx 的 `stroke` 升级为记录**调用瞬间 lineWidth + 路径外接框**（晚帧会被后续步骤覆盖，必须现读现记）；断言 恰好一次 `lineWidth === 1.5` 的描边且外接框 = **填充条**（`w × share`，非整条轨道）、无 stroke 的分组条填充几何上没有任何 stroke（**删掉 stroke 分支此用例即红**） | `tests/pet/usage-export-image.test.ts` 用例 6（假 ctx 补 `save/restore` 真实状态栈建模） |
| Nit | stroke 分支 `ctx.lineJoin = "round"` 未复位，随同一 ctx 流入后续步骤 | 描边分支包 `ctx.save() / ctx.restore()`（executor；假 ctx 的 save/restore 按状态栈实现） | 用例 6 追加：1.5 那次 stroke 之前有 `save`、之后有 `restore`，整图 save/restore 计数配平 |
| 控制器裁决 | 「导出图片的 token 单位写法改成和主看板一致」覆盖**整张导出图**——hero 已是 `203.70万` 而评语气泡插值仍是 `2,036,981`（Batch 5 申报的遗留不一致点） | `useExportShare.ts` 的 `QuoteVars.tokens` 改 `fmtTokens(dash.hero, getLang())`（与 hero 同函数同 lang）；`export-image-all-modules.png` 重拍覆盖（气泡实测 `203.70万`） | `usage-export-csv.test.tsx` 用例 5：覆写 `usage_get_settings` 给固定评语模板 `T{tokens}T`（默认评语池随机选条、无法逐字 pin），断言装配产物 `quote === "T203.70万T"`（RED 时实测 `T2,036,981T`）；无量到窗口的 `—` 行为不变（既有用例照过） |

门禁实录（全部实际执行）：RED 先行（2 条新断言对旧代码红：`T2,036,981T` ≠ `T203.70万T`、save/restore 缺席）→
定向 vitest（tests/usage + tests/pet + tests/speed）83 文件 775 passed / 5 skipped（既有 skip）；
全量 `pnpm exec vitest run tests/` **194 文件 2351 passed / 5 skipped**；`pnpm check` 退出码 0。
单笔提交：`fix(usage): unify quote tokens with the board scale and cover the cost-bar stroke`。
截图重拍经自起 Vite（端口 5199，用完即停）+ `toBlob` 拦截（同 Batch 3/5 方法）。

### Batch 6（第三轮实机反馈：窗口 900×750 / 会话卡段距 / 速度表头排序与去内滚 / 边缘可见性 / 仓库看板统一）

用户第三轮实机反馈 8 条（其中 (4) 为数据问答，无代码）。**本轮用户豁免测试与构建验证：下述 vitest/pnpm check 门禁未运行**；受影响测试文件已按新实现同步改写（未运行）。

| # | 发现/诉求 | 处置 |
| --- | --- | --- |
| 1 | 默认窗口改 900×750（最小 800×600 不变） | `tauri.conf.json` main `width:900/height:750`；CHANGELOG zh/en `[Unreleased]` Changed 区各加一条升级须知 |
| 2 | 会话卡三行段距进一步缩小/不设 | 标题行 `mb-1→mb-0.5`；预览 `mb-1.5→mb-1`；新增空标题守卫（`(title‖id).trim()` 非空才渲染标题行，空白标题不再留空行占位）；`font-semibold` 保留 |
| 3a | 删「当前口径：…」注释行 | `WorkspacePageHeader` 不再传 description；`speed.mode.description` 键 zh/en 删除（零消费已核）；无用的 `unitLabel` 一并删除 |
| 3b | 图表区块不要内层下拉、整 sheet 只滚一处 | 根容器 `overflow-hidden→overflow-y-auto`；详情 section 去 `overflow-y-auto`；栅格去 `min-h-0 flex-1`（内容驱动）；宽窗左列 `lg:sticky lg:top-0 lg:max-h-[calc(100vh-190px)]` 吸顶自滚，图表 240px 高随页完整可见 |
| 3c | 删「搜索工具或模型」整行 | `SpeedSeriesList` 搜索输入与 `search/onSearchChange` props 删除；查询载荷 `search` 恒 `""`（契约字段保留）；`speed-list-empty-search` 分支与 `speed.list.searchPlaceholder/emptySearch` 键删除 |
| 3d | 速度/有效会话表头直接点击升降序 | 表头三格改可点按钮（`speed-sort-header-name/rate/sessions`，`aria-sort` + ↑/↓ 指示；同键翻转、异键归默认方向）；排序 select 与方向按钮整体删除；查询载荷仍恒 `sort:"tool"`；组合名悬浮 `title` 保留 |
| 5 | 删「纯 token · 含子代理 · 本地聚合 · 环比基准…」注释行 | `UsageSummary` footer 整行删除、`retentionDays` prop 链拆除；`usage.footer.baseline/retention` 键删（**`usage.footer.caliber` 保留**——分享图 `exportLayout` 仍在消费） |
| 6+9 | 白/黑 UI 按钮卡片边缘不明显 | 根因=Task 8 把 `--input` 映射成次级面色，而 outline 按钮/输入框描边用 `border-input`：浅 `--input #F8F9FC→#CDD3E0`、深 `#1F2128→#4A4E5C`；浅 `--border #E3E6ED→#D9DEE9`、深 `#3A3D4A→#454854`；深色按钮同时获 `dark:bg-input/30` 深度差 |
| 7 | 口径第二行加分号 | `speed.modeHint.line2` 末尾加「；」（en 不动） |
| 8 | Skill 仓库看板灰蒙蒙/白色区块/搜索框不明显 | 根因=`ResourceKindDialog` 的 `.workspace-glass` 86% 透底叠遮罩 + `bg-muted` 表头 + `bg-background` 白块名字格：DialogContent 加 `bg-card`、两处 sticky 表头与名字格统一 `bg-card`、搜索框描边 `border-input`（随 --input 提亮显形）；零逻辑改动 |
| 4 | 「总请求次数有细分明细吗？按模型…」（消息截断） | **答复无代码**：账本按请求逐条采集、聚合行带 `requests` 列（`dao/usage.rs:40,58`），`v2/cost_query.rs:103,180` 已按供应商/模型 `SUM(requests)`——按模型出请求数的数据基础已具备；当前分布卡只展示 token 维度。是否把「按模型的请求数」加进分布/导出，待用户裁决后排期 |

文档同步：spec §3.1 边界行值 + 二次修订注；spec §8.1 布局行（去搜索、点表头排序、吸顶自滚/整页只滚）；CHANGELOG zh/en 900×750 条目。测试同步（未运行）：`sessionCardHeader`（mb/trim 用例改写）、`speed-dashboard`（排序用例改表头点击、搜索用例删除、类锁改写、工具筛选替代搜索的回退用例）、`speed-chart`（mount props 与列表类 pin）、`v2-summary`（去 retentionDays prop）。

门禁：**用户豁免，本轮未运行**（vitest / pnpm check / cargo 均未执行）；下次跑套件时上述同步后的测试为首次验证。

#### Batch 6 补充（同日追加减：行距 8px 裁决）
用户目检 Batch 6 后指出卡片中部仍有空行。根因（Playwright 实测）：会话卡根是 shadcn `Card`，基类自带 `flex flex-col gap-6`（24px 行距）——每两个直接子元素之间被垫 24px，此前改的行内 mb 只是在 24px 之上微调。先改 `gap-0` 验证，再按用户裁决定为 **`gap-2`（8px 统一行距）**、行内 mb 全部移除；Playwright 复测 header→title→preview→meta 三段间距均为 8.0px，截图 `feedback/session-card-gap8.png`。测试 `sessionCardHeader` 的 mb 引脚断言改为「行内无 mb + 卡根 gap-2 无 gap-6」。

### Batch 7（第四轮实机反馈：记录统计三列+请求合计 / 速度标识可选框+即时悬浮 / 工具格口径说明）

**本轮用户豁免测试与构建验证**（vitest / pnpm check 未运行）；`node scripts/check-i18n.mjs` 作为纯脚本核对已运行（2608 键对齐）。受影响测试已同步改写（未运行）。

| # | 诉求 | 处置 |
| --- | --- | --- |
| 1 | 记录里的模型统计带请求次数：供应商显示名 / token 数右侧请求次数；三项用表头；请求次数带合计 | `UsageRecordsPanel` RecordCard 行列表改三列结构：**表头行**（供应商显示名 / token 数 / 请求次数，`usage-record-cols-{toolId}`）；每行 token 定宽列右侧新增**请求次数**列（`row.metrics.requests`，前端本已携带、此前未渲染，零后端改动，`usage-record-requests-{toolId}:{i}`）；卡底新增**合计行**（token 合计沿用卡头口径、请求合计 = Σ 全部行，`usage-record-total-{toolId}`）；来源/归属徽标与操作按钮保留在行内。新增键 `usage.records.colModel/colTokens/colRequests/totalRow` |
| 2 | 输出速度组合名改可选框；悬浮全名缩短等待 | 标识块改原生 `<select data-testid="speed-detail-title">`（options = 快照全量组合名、文本 = `seriesRowLabel`（新导出于 view.ts，工具别名→模型[·渠道]），onChange 同列表点击走 `selectSeries`；keepPreviousData 尾窗补守卫选项防悬空）；「日志近似」徽标留框外。列表行名改**纯 CSS 即时提示**（`group-hover:block`，无原生 title 的 1~2s 延迟；`role=tooltip` 全名，`aria-label` 保可访问名）；共享拼法收敛到 `seriesRowLabel` 防两处漂移。新键 `speed.detail.seriesPicker` |
| 3 | 工具累计用时为啥都没时间 | **答复**：七个源都无法把工具执行时长可靠归属到单次模型调用（各源 `tool_ms: None` 的「不填 0」注释），spec §5.3 明文禁止拿全工具工作活动时长冒充——是口径不是缺陷。改动：工具格 `Tile` 加 `title`（`speed.tile.toolMsHint`：说明 + 指向「工作活动」），`—` 渲染不变 |
| — | 测试同步（未运行） | `speed-dashboard`（`detailTitle()` 改读 select 选中项；新增可选框切换/即时悬浮/工具格 title 用例）、`speed-acceptance`（四处断言改 `detailTitleText()` helper）、`usage-records`（新增表头/请求列/合计行用例） |
| — | 文档 | CHANGELOG zh/en Unreleased 两条（记录统计三列+请求合计；速度可选框/即时悬浮/工具格说明） |

门禁：**用户豁免，本轮未运行**（vitest / pnpm check / cargo 均未执行）；check-i18n 纯脚本已核对通过。

### Batch 8（第五轮实机反馈：仓库弹窗发糊 → 字号一档上调 + 弹窗去玻璃改实心卡）

用户反馈仓库弹窗整体发糊、「分辨率比别处低」。两层根因叠加：
1. **字号密度**：弹窗内 12 处 `text-[10px]`、10 处 `text-xs`(12px)——全库最小字号最密集的面，高分屏下小字笔画发虚（用户自诊）。
2. **合成层栅格化**：DialogContent 基类（Task 8）挂 `.workspace-glass`（`backdrop-filter: blur(12px) saturate(1.4)`）+ 50% 居中平移——三轮给弹窗加实心 `bg-card` 后玻璃已无视觉收益，但 backdrop-filter 仍把整个弹窗压进 GPU 合成层，高分屏 + 平移下整层栅格化发软（WKWebView 上尤甚）。

处置（单提交）：
- **字号一档上调**：`ResourceKindDialog`（10px→11px ×12、xs→13px ×10）与 `ResourceByKindView` 卡片侧（10px→11px ×3、xs→13px ×4）；`text-sm` 行名不动。横向空间：表格本就是内容定宽 + 横向滚动 + 名列 sticky，字号变宽由滚动区吸收，弹窗视口不变。
- **弹窗去玻璃**：`ui/dialog.tsx` DialogContent 基类 `.workspace-glass` → `bg-card`（全部弹窗实心卡面）；`ResourceKindDialog` 三轮加的实例级 `bg-card` 变冗余一并移除。spec §3.2 加修订注；玻璃保留在导航/下拉等小面浮层。
- 门禁：**用户节奏豁免，未运行**（vitest / pnpm check）；touched 文件 prettier 通过；测试无 dialog-glass/字号引脚（grep 核实），无需同步。

### Batch 7 回归修复（第四轮回归，用户肉眼验收通过 → `1768bd00`）

四轮改「即时悬浮」时埋的回归：列表名内层 span 变 inline——`truncate`（overflow/ellipsis/min-width）对 inline 盒不生效，长组合名整串溢出包装盒、横穿「日志近似」徽标（实机截图坐实）。修复：内层补 `block`；同步四轮漏改的 stale 断言（`title` 属性 → `aria-label`）并加 block+truncate 回归锁。**用户肉眼实测验收通过**；Playwright 复测 `innerDisplay: block / overflow: hidden / 徽标无重叠`。

**分支终态（用户 2026-10-27 宣告「整个分支没有问题」）：HEAD `1768bd00`；未合并/未推送/未打 tag；6 个未跟踪原样。**
