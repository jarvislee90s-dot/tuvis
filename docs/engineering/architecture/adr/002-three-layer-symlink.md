# ADR 002: 三层符号链接映射架构

**状态**: 已接受 | **日期**: 2026-07-05

## 背景
Skill/MCP/Plugin 需要在统一仓库中管理，同时映射到各工具配置目录。需要支持工具级和子 Agent 级两层分配。

## 决策
三层符号链接：Layer 1 (SSOT) -> Layer 2 (工具级) -> Layer 3 (子 Agent 级)。Layer 3 指向 Layer 2，工具级禁用自动断开所有子 Agent。

## 后果
- 正面：工具级禁用自动传播到子 Agent
- 负面：Windows 不支持符号链接，需 Junction/copy 降级
