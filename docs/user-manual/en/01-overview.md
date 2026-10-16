# What is Tuvis?

Tuvis is a desktop board for seeing and steering all your AI agents: open source (MIT), local-first, telemetry-free, available for macOS and Windows.

![Tuvis](../img/hero.png)

## Core idea: monitor → alert → act remotely

1. **Monitor**: running sessions of your AI coding tools are discovered automatically and shown on a status-light board with state, project, branch, message preview and more — click a card to focus the matching terminal directly;
2. **Alert**: state changes fire desktop notifications and sounds while the tray icon aggregates and recolors in sync — important states no longer drown in a pile of terminals;
3. **Act remotely**: with remote access on, your phone's browser opens the very same board as the desktop, so away from the desk you can still follow session progress, receive push alerts, send messages, approve plans, and even create sessions (sending messages and creating sessions are experimental).

## Eight tools, one board

Tuvis supports eight AI coding tools today. Each tool differs in how open it is, and the matrix below reports capabilities honestly (✅ supported / ◐ partial / 🧪 experimental / ❌ not supported):

| Capability            | Claude Code | Codex CLI | OpenCode | OpenClaw | Kimi Code | WorkBuddy | ZCode        | dsh   |
| --------------------- | ----------- | --------- | -------- | -------- | --------- | --------- | ------------ | ----- |
| Session monitoring (status lights) | ✅          | ✅        | ✅       | ✅       | ✅        | ✅        | ✅           | ✅    |
| Desktop notifications | ✅          | ✅        | ✅       | ✅       | ✅        | ✅        | ✅           | ✅    |
| Skill management      | ✅          | ✅        | ✅       | ✅       | ✅        | ✅        | ✅           | ◐ read-only |
| MCP management        | ✅ JSON     | ✅ TOML   | ✅ JSONC | ✅ JSON  | ✅ JSON   | ✅ JSON   | ✅ JSON subtree | ❌    |
| Mobile · message viewing | ✅          | ✅        | ✅       | ❌       | ✅        | ✅        | ✅           | ✅    |
| Mobile · file preview | ✅          | ✅        | ✅       | ❌       | ✅        | ✅        | ✅           | ✅    |
| Mobile · message injection | 🧪          | 🧪        | 🧪       | ❌       | 🧪        | ❌        | ❌           | ❌    |
| Token usage collection | ✅          | ✅        | ✅       | ❌       | ✅        | ✅        | ✅           | ✅    |

> 💡 🧪 in the table marks an experimental capability: phone injection (sending messages / creating sessions) is currently open to four tools only — Claude Code, Codex CLI, OpenCode and Kimi Code. OpenClaw / WorkBuddy are black-box forms with no external write channel, so they can be watched but not written to (OpenClaw cannot be viewed remotely from the phone, only on the desktop board) — a real difference in how open each tool is, not a malfunction; OpenClaw does not support token-usage collection yet.

Want to try the interactions first? Open the [interactive demo](/demo) — no install needed to get a feel for the board and the phone-side flows.
