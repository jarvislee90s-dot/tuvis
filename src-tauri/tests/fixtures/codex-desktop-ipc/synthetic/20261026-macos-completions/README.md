# SYNTHETIC · 26.1021.31345 契约片段补全（仅供旧夹具规则回归）

**等级：SYNTHETIC。** 这里的两个 `.synthetic.js` 不是从 26.1021.31345 包里切出来的，
而是按 26.1023.51421 的**观测**形态、替换成旧包访问器名合成的；只用于让
`observed/20261026-macos/contract-fragments/` 这套旧夹具能跑通 2026-10-28 收紧后的
异步答题结构规则（旧包已随 Desktop 自动升级不可得，本机无存档）。

| 文件 | 合成依据（观测来源） | 旧包替换 |
| --- | --- | --- |
| `question-id-callsite.synthetic.js` | `observed/20261028-macos/contract-fragments/question-id-callsite.js`：`map((t,n)=>bSe(e.id,n))` | 生成函数名 `bSe` → 旧夹具的 `pxe` |
| `string-schema-witness.synthetic.js` | `observed/20261028-macos/contract-fragments/string-schema-witness.js`：`r.M({path:r.L().trim()…})` | 对象构造 `r.M` → `r.k`，字符串构造 `r.L` → `r.P`（与旧 `async-envelope.js` 一致） |

纪律：

- 不得把本目录内容当作 26.1021 的观测证据引用；它只证明"规则对一致改名不敏感"，
  不证明旧包真有这两行。
- 旧包若日后可得（如从 Time Machine 恢复），应以观测片段替换并删除本目录。
