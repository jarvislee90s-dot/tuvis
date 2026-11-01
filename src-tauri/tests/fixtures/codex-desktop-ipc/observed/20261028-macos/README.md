# 2026-10-28 Codex Desktop 26.1023.51421 契约结构片段（Mac）

本目录只有 `contract-fragments/`（等级 **CODE**：`app.asar` 主进程打包产物里与协议
相关的最小静态片段），不含运行日志、会话正文、命令、路径或凭据。

运行身份：Desktop `26.1023.51421`（bundle `/Applications/ChatGPT.app`，
`CFBundleIdentifier=com.openai.codex`，bundle version 22530），随包 CLI
`0.165.0-alpha.1.2`，探测键 `macos|26.1023.51421|0.165.0-alpha.1.2|1`。

包身份（取证时）：SHA-256
`1c8ef1bef66e5fc416ae66eff8e6529972aa3601e27a1c7b5f61d6a292b957d3`，
659,958,238 字节，mtime 2026-10-28 19:24:05 +0800。

## 片段与来源

| 片段                                  | 来源（包内）                                       | 相对 `20261026-macos` 的差异                                                                                                                            |
| ------------------------------------- | -------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `version-table.js`                    | `/.vite/build/src-tOjGKoT3.js`（`Oe`/`ke`/`Ae`/`je`） | 逐字相同（start-turn=2、interrupt=4、decision=1、submit-user-input=1）                                                                                   |
| `start-turn-handler.js`               | `/.vite/build/main-UOTY8NmV.js`                    | 逐字相同                                                                                                                                                |
| `decision-and-user-input-handlers.js` | 同上                                               | 逐字相同                                                                                                                                                |
| `interrupt-handler.js`                | 同上                                               | 逐字相同                                                                                                                                                |
| `async-envelope.js`                   | 同上                                               | **仅压缩器标识符改名**：`uxe/dxe/Ay/fxe/pxe/mxe` → `_Se/vSe/ey/ySe/bSe/xSe`；zod 访问器 `r.k/r.P/r.L/r.g` → `r.M/r.L/r.B/r.v`。标记字面、字段名、`request_user_input_async` 合成、`min(1)`、`slice(34,-35)`、守卫引用关系全部不变 |
| `start-turn-request.js`               | 同上                                               | 辅助函数 `_C` → `Yx`；`turnStart.context` 新增 `inheritedSelection` 字段（与规则无关）                                                                   |
| `question-id-callsite.js`             | 同上（`ry`）                                       | 新增取证（旧包不可得）：`map((t,n)=>bSe(e.id,n))`——题目标识第三元素来自 map index 形参                                                                     |
| `string-schema-witness.js`            | 同上（`vie`）                                      | 新增取证（旧包不可得）：`r.L().trim()`——三字段共用的构造链在包内被字符串专有组合子修饰（同类用法 130+ 处，取最短一处）                                       |

这组片段是 2026-10-28 `v0.6.3` 异步答题识别缺陷（`operation_no_evidence`）的一手
证据：旧规则把 `startsWith(uxe)` / `endsWith(dxe)` / `question:r.P(),answer:r.P()`
这三条**压缩器局部名**当成稳定契约，本版改名即失配；其余五项操作在本包上仍匹配。

## 取法（只读，不执行包内 JS）

读 ASAR 目录头（`[4][header_size][..][json_size]`）→ 只取 `/.vite/build/*.js` →
按锚点切出原文（未归一化，保留反引号与空白，让测试走真实 `normalize`）。
本机复现：

```bash
cargo test --lib f08p_t01_local_installed_package_smoke -- --ignored --nocapture
```

## 纪律

- 不提交 `raw/`、截图、真实 thread/owner/turn ID、真实题库文本或家目录路径。
- 片段是"结构匹配"规则的来源，**不是**"能力已验"的证据——放行仍由逐操作策略按
  当前包重新匹配决定；本版新版写入语义的隔离会话实机验收另记。
