# 人工裁决台账

本目录只记录**用户明确裁决或推翻**、且仍会影响产品行为/验收的结论。

不记录：

- agent 的返工步骤、测试流水、提交清单；
- 常规实现修复；
- 已被后续裁决取代但无行为影响的讨论。

每条裁决至少包含：日期、背景、决定、后果、状态（active / superseded）、来源。
被取代的条目保留，并在新条目写 `supersedes:`，旧条目写 `superseded_by:`。

当前主题：

- [codex-compat.md](codex-compat.md)：自动适配、未知 build、权限/Plan 边界、成功证据。
- [grouped-answering.md](grouped-answering.md)：同组多题、提交、恢复与答案模式。
- [card-ui.md](card-ui.md)：停靠、审批/问答配色、历史与选中态。
- [queue-pause.md](queue-pause.md)：打断后的持久暂停、恢复与提示。
