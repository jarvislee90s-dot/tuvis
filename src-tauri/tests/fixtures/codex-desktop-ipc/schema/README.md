# 官方 schema 夹具（审批 decision 枚举）

来源：随包 CLI 的 `app-server generate-json-schema --out <temp> --experimental`
（`probe/schema_probe.rs` 跑同一条命令）——**官方公开 schema 文档**，不含任何用户数据/
会话内容。

采集记录（2026-10-21，Windows 主机）：

| 项 | 值 |
| --- | --- |
| Desktop 包版本 | `26.1013.4894.0`（MSIX `Identity/@Version`） |
| 随包 CLI | `C:\Users\<user>\AppData\Local\OpenAI\Codex\bin\<hash>\codex.exe`（`codex-cli 0.163.0-alpha.10.2`） |
| 产出文件数 | 452（平铺 45 + `v1/` 2 + `v2/` 405；审批响应只在平铺 v1 集里） |
| command 审批 decision | `oneOf` **6 分支**：`accept` / `acceptForSession` / 对象 `acceptWithExecpolicyAmendment` / 对象 `applyNetworkPolicyAmendment` / `decline` / `cancel` |
| file 审批 decision | `oneOf` **4 分支全是字符串**：`accept` / `acceptForSession` / `decline` / `cancel` |
| permissions 响应 | `permissions` / `scope` / `strictAutoReview` —— **不是** accept/deny（不开放） |
| 问答响应 | `answers`（map<qid, {answers: string[]}>）—— 结构化，不走决策词（不开放） |

两个 JSON 是生成结果里的**原样副本**（未裁剪），只用于解析回归：真实形态是
`properties.decision` = `{"$ref": "#/definitions/<X>ApprovalDecision"}`，被引用定义是
`oneOf` 分支集合——**每个词一个分支**。只看 `decision.enum` 的解析器在这里会只认出
第一个词（`fix(codex-ipc): parse oneOf decision enums` 修的正是它）。

对象变体（`acceptWithExecpolicyAmendment` / `applyNetworkPolicyAmendment`）**不是字符串
词**：本仓只发字符串决策（`accept` / `cancel`），不构造这两个对象分支；解析器只摊平
`enum` 词、跳过对象分支（T7 若将来要支持 execpolicy/network 修正，另立任务）。

语义要点（schema 自带的 description，逐字）：
- `decline`：拒绝，**回合继续**（command：agent 继续本轮；file：agent 继续本轮）；
- `cancel`：拒绝，**本轮立即中断**——#172 的「终止本轮」用它。

重新采集：`probe::schema_probe::probe_schema()`（或手工跑上面那条命令），然后替换本目录
两个文件即可（回归用例 `parses_real_4894_approval_schema_fixtures` 会立刻反映形态变化）。
