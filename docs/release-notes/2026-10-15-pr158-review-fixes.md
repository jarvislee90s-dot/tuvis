# PR #158 四轮复审修复记录

基准：`2f9c30ee`；复审提交时间：2026-10-15 13:19（北京时间）。
本轮只修改错误文案、扫描门禁、CI 与测试约束；不改变注入动作、屏读判据、用量采集与账本归属。
`docs/MASTER-PLAN.md` 未修改。

## 逐项进度

| 项目 | 本轮处理 |
| --- | --- |
| I-1 三处半迁移 | `question.rs` 两处、`api.rs` 一处补既有码。裸串按生产字面量整段收集，const/helper 与多行调用均纳入跨文件对账；日志豁免只作用于调用自身实参。 |
| I-2 词法剔除器 | Rust 共用一套注释/字符串骨架处理 cfg；支持任意 #、br/cr 原始串、嵌套注释、生命周期与属性间注释；修正 enum 变体与 struct-pattern/if-else match arm 边界。 |
| I-3 CI | frontend job 显式执行 `pnpm check:i18n` 与错误码门禁，自测由已有 `pnpm test` 执行。paths-filter 纳入 Rust 生产源码、scripts 与 eslint 配置，防止后端独改/门禁独改时跳过。 |
| I-4 用量 detail | 延续保留 W-28 的方案，英文化固定诊断并保留定位参数；另补查 `UsageError::internal` 的任务/账本包装文案。源码字面量门禁覆盖 new/internal，动态参数保留原值。 |
| I-5 块注释误报 | Rust 词法态剥除注释，不把字符串里的 URL 或行注释里的路径通配符当块注释边界。 |
| M-1 断言失明 | server/content/tailscale 相关断言检查带码原文，含 busy 分支与 unknown-tool 邻接断言。 |
| M-2 申报口径 | 本记录明确区分探针、全树检查、运行时与候选计数，不沿用前几轮的错误零中文/双源/比例表述。 |
| M-3 前端对称收集 | 带码与裸串两侧均纳入 TS/TSX；通过 TypeScript AST 处理 regex/JSX/模板，不再使用 Rust 字符串词法规则扫描前端。 |
| M-4 lint 覆盖 | tests 与 scripts 纳入 ESLint；清理既有未用变量、改用 const，并说明 mock 的合法共享返回/实例持有，不改变测试断言。 |
| M-5 迁移棘轮 | 固定新口径按文件预算，当前合计 **452 个候选**。这是 ±3 行上下文统计，包含尚未判定为上屏错误的锚点等，不能与复审旧逐行口径的 172 相比。新增文件预算为零，基线缺失/无效报红。 |
| M-6 mobile 边界 | UI 全量翻译与七处历史无码 sink 夹具仍由 #157 跟踪；不宣称 mobile 只剩七处夹具，也不宣称英文界面零中文。 |

## 验证证据与更正

- 新增隔离 CLI 探针先复现 9 个失败；第一次只读复查又复现 TS regex 错位、struct pattern 与
  if/else arm 三个失败，分别补回归后通过。现有 **33 条**门禁用例覆盖正/反向注码、缺键、
  重复前缀后继续诊断、跨文件归一化、块注释、用量 detail 与按文件棘轮。
- 真实 `MessageComposer.tsx` 曾因附件 regex 的引号漏收后续文案。AST 改造后，复制完整文件
  至临时树并为「设备已失效，请重新配对」提供 Rust 带码孪生，门禁能正确报半迁移。
- 初始工作区 `query.rs` 的未提交差异误删约 3,800 行，已先备份现场，再从基准完整恢复该文件，
  只重新应用三处 detail 英文化；代码、测试与既有约束恢复完整。
- 用量源码尺寸锁实测 **7241 行 / 156998 字符**：原 **7238/156598** 基础上修改固定诊断；
  dsh 的 `format!` 经 rustfmt 展开增加 3 行，其余变化为文案字符差异，排除区标记未移动。
  原先注释引用的 `KNOWN-PLAN-DEFECTS.md` / `FINAL-FIX-report.md` 本工作区不存在，本记录承接本轮追记。
- 用量域定向测试：292 通过、2 忽略；W-28 模板测试保持通过，zh/en locale 模板未改。
- 前端全量：141 文件、1472 通过、5 跳过；`pnpm check` 通过，1348 个 i18n 键、249 码/250 errors 键。
- 后端并行全量首轮：2371 通过、43 忽略、1 失败，失败为既有
  `probe_version_via_cmd_shim` 超时（此前复审已记录）；单独运行 0.03 秒通过。
- Git Bash 串行第二轮：2371 通过、43 忽略、1 失败，失败为
  `resolve_tool_path_finds_cmd_on_windows` 的 PATH 解析；原生 PowerShell 单跑 0.11 秒通过。
  本批未修改 `approve.rs` / `resume.rs`，不据此将脚本整体记作全绿。
- 原生 PowerShell 最终串行 `cargo test --features marker-helper`：**19 个 target 全绿，
  2512 通过 / 0 失败 / 55 忽略**。`cargo fmt --check`、`cargo check`、
  `cargo clippy -- -D warnings` 均通过。Windows 宿主未执行 GNU 交叉三步，也未进行真实终端交互验收。

## 保留边界

错误码门禁不证明所有运行时展示链已翻译。UsageError detail 的常量/变量间接传参与底层库
动态错误不做全程序追踪；保留参数意味着中文路径/外部错误也可能出现在英文界面。
Rust cfg 剔除是词法扫描，不承诺完整宏展开或任意 Rust 语法的数据流分析。
后续 mobile 全量翻译、历史夹具生产化与阶段文案常量提取继续单独推进。

## 五轮复审收口（`431f8da` 之后）

基准：`431f8da`。五轮复审结论为「可合，但建议先修 1 处本轮新引入的扫描器缺陷 + 补 2 条自测」，
本批按该结论收口，**只动扫描器/自测/基线/文档，不动任何生产行为**。

| 项目 | 本批处理 |
| --- | --- |
| Important-1：`cfg(test)` 项类型位顶层逗号 | `error-code-source.mjs` 新增 `matchingAngle`：条目扫描在类型位按 `<…>` 配对跳过（`->`/`=>`/`>=` 的 `>` 不算收口；找不到配对或先遇 `;{}` 则退回按普通字符处理，比较运算符不受影响）。`-> Result<(String, u32), String>` 这类条目的逗号不再提前结束条目。 |
| 基线重算 | 修复后 `remote/attachment_fixtures.rs` 的 6 条测试夹具候选归零：50 文件/452 → **49 文件/446**；其余 49 个文件逐字不变（已与 `431f8da` 的基线逐项 diff，仅夹具一项变化），即**没有生产代码被误删出扫描面**。 |
| Important-2：两条历史回归类别补自测 | 新增 6 条用例（33 → 39）：① 三例类型位逗号（`Result<(String,u32),String>`、`Result<String,String>`、多级嵌套泛型）；② `#[cfg(test)] mod x;` 无体声明之后的生产码必须留在面内；③ struct pattern arm 的后缀表达式（`Fixture {}.run("…")`）不结束条目；④ 真实夹具文件 `attachment_fixtures.rs` 内注码不得进生产面。 |
| 自测判别力（变异验证） | 三个变异体逐个注入后立即还原（文件哈希校验一致）：禁用尖括号跳转 → **3 例红**；删除「`;` 先到」规则 → **1 例红**；删除 struct-pattern 后缀规则 → **1 例红**。三者此前均为存活变异体。 |
| 文档 | `docs/I18N.zh-CN.md` 补：类型位尖括号/无体声明已入隔离用例；棘轮表述改为「**±3 行启发式命中时**请添加码」；边界段补 `concat!("码: ", "正文")` 与 JSX 文本节点两个未收集形态；注明仅入日志的中文诊断也会计入候选（口径偏保守）。 |

验证：`node scripts/check-error-codes.mjs` 绿（249 码 / 250 键 / 候选 446）；`check:i18n` 1348 键；
`vitest run tests/lib/errorCodesGate.test.ts` **39 通过**；前端全量与 `tsc`/`lint`/`format:check` 结果见 PR 复评。
Rust 侧本批零生产改动（diff 仅 `scripts/`、`tests/`、`docs/`），由 CI backend 与本地复跑覆盖。

