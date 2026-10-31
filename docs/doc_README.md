# 文档导航

本目录面向使用者、开发者和维护者。分类按用途，不按日期。

## 产品

- [手动手册（中文）](product/manual/zh/01-overview.md) / [English](product/manual/en/01-overview.md)：完整使用说明。
- [操作指南（中文）](product/guides/zh/getting-started.md) / [English](product/guides/en/getting-started.md)：入门、资源、预设、监控、故障排查。
- [全局快捷键](product/guides/zh/global-shortcut.md) / [English](product/guides/en/global-shortcut.md)。

## 工程

- [项目宪法](MASTER-PLAN.md)：最高设计依据；未经用户明确同意不得修改。
- [规格与设计](engineering/specs/)：需求、设计、行为与验收口径。
- [DSH Desktop 远程连接统一 Spec](engineering/specs/2026-10-29-dsh-desktop-remote-design.md)：八类功能、支撑证据、逐操作版本兼容与验收条件。
- [实施计划](engineering/plans/)：当前/历史执行计划；executor prompt 不入库。
- [人工裁决台账](engineering/decisions/)：用户裁决、推翻与边界；只记决定，不记 agent 返工过程。
- [最终审查索引](engineering/reviews/)：审查范围、最终结论、未闭合风险。
- [探测与取证](engineering/evidence/)：脱敏的 probe/复探证据。
- [验收](engineering/acceptance/)：最终验收；`history/` 为历史验收材料。
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
