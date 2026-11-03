# ADR 001: Adapter 模式实现多工具支持

**状态**: 已接受 | **日期**: 2026-07-05

## 背景
需要支持多个 AI 编程工具（Claude Code、Codex CLI、OpenCode、OpenClaw），每个工具的进程发现、配置格式、会话解析方式不同。

## 决策
采用 Adapter 模式：每个工具实现 `AgentAdapter` trait，核心模块通过 trait 接口与工具交互。

## 后果
- 正面：新增工具只需实现 trait，不改核心代码
- 负面：trait 设计需足够抽象

## 替代方案
- 硬编码 if-else：代码膨胀快，拒绝
- 插件系统：过于复杂，MVP 阶段过度设计，拒绝
