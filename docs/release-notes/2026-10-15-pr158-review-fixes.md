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
