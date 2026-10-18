# 实机金样夹具（canonical 真实形态）

来源：独立只读探针直连 `~/.codex/ipc/ipc.sock`（initialize → thread-owner-discovery →
`thread-stream-following-changed {following:true}` → 收 targeted snapshot → 只读
`load-complete-history` → `following:false` → 关闭），未发送 Turn、未审批、未改设置。
采集与脱敏由 2026-10-18 复审完成（复审报告与原始证据：
主 checkout `docs/superpowers/reviews/evidence/2026-10-18-codex-ipc/`）。

采集时安装包 `CFBundleShortVersionString=26.1013.40934`（运行主进程 2026-10-18 启动）。
**与原缺陷报告实测的 `26.1013.31256` 不是同一 build**；两代 build 的实体级形态一致
（原报告 2026-10-18 凌晨抓包：实体合成键 `tail:0:local:<uuid>`、turn 实体无 `type`、
`islands.entries` 为 key/value 对象、`isComplete` 在 history 层）。

| 文件 | 等级 | 说明 |
| --- | --- | --- |
| `canonical-before-load.sanitized.json` | OBSERVED 形态 / SYNTHETIC 文本、ID | `thread-stream-state-changed` v11 snapshot（revision 3）：3 个 turn 实体（`turn:<uuid>` 与 `tail:0:local:<uuid>` 两种键并存；tail 键尾 UUID ≠ 实体 turnId）、11 个**内联 items**（`userMessage`/`agentMessage`/`reasoning`/`error`，camelCase）、`history.isComplete` 在 history 层、`islands.entries` 为 `{key,value}` 对象、`threadRuntimeStatus:{type:"idle"}`、`turns:[]`、`requests:[]` |
| `canonical-after-load.sanitized.json` | OBSERVED 形态 / SYNTHETIC 文本、ID | 只读 `load-complete-history` 成功返回后的 snapshot（revision 4）；实体与正文与加载前一致。证明该 RPC 成功返回带目标 revision，**不**证明所有历史一开始就完整 |

## 金样钉死的事实（读者必须满足）

1. 实体按 `turnId` 字段认领，map key（`turn:`/`tail:` 前缀）只是引用——tail 键尾 UUID 与
   实体 `turnId` 可以不同，绝不能从 key 解析业务 ID，也不能要求 `type=="turn"`。
2. canonical 顺序 = `islands[].entries` 数组序，引用键取 **`entry.value`**（Desktop 自身
   读法；`entry.key` 是条目自身标识，可与 value 不同）。
3. 消息正文在 `turn.items[]` **内联对象**上：`userMessage.content[].type=="text"` 的
   `text` 块、`agentMessage.text`；`error` 无正文（安全摘要）；`reasoning` 按产品语义过滤。
4. 完整性分层：Turn 列表完整性 = `turnHistory.history.isComplete`（缺字段 ≠ 完整）；
   单 Turn 的 items 完整性 = `itemsPagination`（`hasLoadedOldest==false`/evicted 即不完整；
   字段缺省不作不完整论）。
5. 原缺陷绑定的目标 Turn 在样本中为 `00000000-0000-4000-8000-000000000006`
   （实体键 `turn:<该ID>`，`status:"completed"`）。

脱敏约定与旧夹具相同：ID 为占位 UUID、文本为 Synthetic 占位，不含真实提示词/会话内容。
