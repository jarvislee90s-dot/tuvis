# 文档导航

本目录面向使用者、开发者和维护者。分类按用途，不按日期。

## 产品

- [手动手册（中文）](product/manual/zh/01-overview.md) / [English](product/manual/en/01-overview.md)：完整使用说明。
- [操作指南（中文）](product/guides/zh/getting-started.md) / [English](product/guides/en/getting-started.md)：入门、资源、预设、监控、故障排查。
- [全局快捷键](product/guides/zh/global-shortcut.md) / [English](product/guides/en/global-shortcut.md)。

## 工程

- [项目宪法](MASTER-PLAN.md)：最高设计依据；未经用户明确同意不得修改。
- [规格与设计](engineering/specs/)：需求、设计、行为与验收口径。
- [Codex Desktop 原会话远程操控总体设计](engineering/specs/2026-11-25-Codex-Desktop连接诊断与能力边界.md)：完整能力愿景、原窗口与原生通道的分阶段路线、默认安装证据边界和完成台账。
- [Codex Desktop macOS 原会话纯文本热修](engineering/specs/codex-desktop-macos-text-hotfix.md)：逐通道的纯文本实现、保护边界及实测范围。
- [OpenCode 原生功能总表](engineering/specs/2026-11-01-OpenCode原生功能总表.md)：存量、增量与完整产品验收分开；逐启动形态的证据范围与未闭合项。
- [OpenCode 远程写入原生化收口与复查标准](engineering/specs/2026-11-23-OpenCode远程写入原生化收口与复查标准.md)：写入、队列、恢复契约与逐操作通道矩阵；旧路径退役与双向复查口径。
- [CLI 整组答题与双端一致性顶层设计](engineering/specs/2026-10-29-CLI整组答题与双端一致性顶层设计.md)：跨 CLI 的答题卡语义与双端一致性公共规格（公共规格，非 OpenCode 专用）。
- [DSH 代际路径缓存](engineering/specs/2026-11-01-f9-dsh-generation-path-cache.md) / [dsh 监控适配器计划](engineering/plans/2026-09-14-m1-dsh-monitoring-adapter.md)：dsh 会话扫描缓存预算与适配器实施口径。
- [WorkBuddy 5.9.3 子代理与看板 Spec](engineering/specs/2026-10-30-workbuddy-5.9.3-subagents-and-board-spec.md) / [设计](engineering/specs/2026-10-30-workbuddy-5.9.3-subagents-and-board-design.md)：5.9.3 子代理漏采与心跳冻结掉线。
- [实施计划](engineering/plans/)：当前/历史执行计划；executor prompt 不入库。
- [Codex Desktop Windows 联调交接](engineering/acceptance/codex-desktop-windows-hotfix-handoff.md)：从 Draft PR 构建，验证最新版 IPC 与旧版回归，并按证据回填。
- [Codex 答题卡修复与升级兼容执行手册](engineering/plans/2026-11-05-codex-答题卡修复与升级兼容执行手册.md)：PR #202 三项缺陷的修法、回归与验收，以及后续持久化、历史核对和版本兼容工作单。
- [WorkBuddy 5.9.3 TDD 执行计划](engineering/plans/2026-10-30-workbuddy-5.9.3-subagents-and-board-tdd.md)：R1/R2 任务、停止点与回滚。
- [人工裁决台账](engineering/decisions/)：用户裁决、推翻与边界；只记决定，不记 agent 返工过程。
- [最终审查索引](engineering/reviews/)：审查范围、最终结论、未闭合风险。
- [Codex B1 持久事实设计审查](engineering/reviews/2026-11-05-codex-b1-durability-design-review.md)：六项风险的证据等级、五处方案缺口与实施计划修订清单。
- [Codex B1.2 进度与剩余项](engineering/reviews/2026-11-05-codex-b1-2-progress-review.md)：多次撤销重放的真实 Rust 失败证据、B1.2 收口与 B1.3 未实施范围。
- [Codex B1.2 验收](engineering/acceptance/2026-11-05-codex-b1-2-acceptance.md)：历史答复证据、重放防护、测试结果及后续边界。
- [探测与取证](engineering/evidence/)：脱敏的 probe/复探证据。
- [OpenCode 归属回声 challenge（H02 摘要）](engineering/evidence/opencode-native/2026-11-14-h02-echo-challenge-attribution.md)：写前归属判据、窗口参数与升级复验要求；代码登记表直接引用此路径。
- [OpenCode 旧 A 存活 / 新 B 可写（M6 摘要）](engineering/evidence/opencode-native/2026-11-16-m6-old-a-alive-new-b-writable.md)：双服务拓扑下的写归属与窗口绑定边界。
- [OpenCode 实名普通启动写正例（C5 摘要）](engineering/evidence/opencode-native/2026-11-17-c5-realname-normal-startup-write-positive.md)：托管服务单实例正例与三方对账。
- [OpenCode macOS 原生复审核验交接（R9 摘要）](engineering/evidence/opencode-native/2026-11-20-macos-native-closure-handoff.md)：五项工程验收、实际 LAN 产品活验与遗留边界。
- [WorkBuddy 5.9.3 证据](engineering/evidence/2026-10-30-workbuddy-5.9.3/evidence.md)：真机核算、心跳/DB/TPS 探针。
- [验收](engineering/acceptance/)：最终验收；`history/` 为历史验收材料。
- [Codex B1.1 验收收口](engineering/acceptance/2026-11-05-codex-b1-1-acceptance.md)：生产 Store 窗口证据、自动检查结果与后续阶段边界。
- [WorkBuddy 5.9.3 验收清单](engineering/acceptance/2026-10-30-workbuddy-5.9.3-acceptance.md)。
- [架构](engineering/architecture/)：ADR、设计、线稿。
- [参考](engineering/reference/)：实体对齐、并发评估等。
- [归档](engineering/archive/)：仅明确被取代或有矛盾的文档；当前可为空。

## 发布与维护

- [发布指南](operations/RELEASE.md)
- [自动更新](operations/AUTO_UPDATE.md) / [中文](operations/AUTO_UPDATE.zh-CN.md)
- [依赖更新](operations/DEPENDENCY-UPDATES.md)
- [国际化](operations/I18N.md) / [中文](operations/I18N.zh-CN.md)
- [构建签名](operations/tauri-build-signing-key.md)
- [版本说明](release-notes/)

## 研究

- [调研与参考](research/)

## 约定

- 新文档写到 `docs/engineering/<category>/`；不要创建 `docs/engineering/`。
- 不按日期归档；只有明确被新版本取代或含矛盾结论的文档才进 `engineering/archive/`。
- 人工裁决进 `engineering/decisions/`；逐轮 agent 返工、测试流水、executor prompt 不入库。
- 测试夹具留在 `src-tauri/tests/fixtures/`，索引见 [Codex Desktop IPC 夹具](../src-tauri/tests/fixtures/codex-desktop-ipc/README.md)。
