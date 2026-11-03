# FAQ

One-sentence answers first; follow the links when you want depth. Groups match the manual's chapter order (product & general → session board → remote access → usage stats).

## Product & general

### Does it cost anything?

No. Tuvis is open source (MIT license) and free — the code is public; feel free to adapt and self-host. See [Chapter 1](./01-overview.md).

### Does it upload my code or conversations?

No. Tuvis is a local-first desktop app: all data stays in the local `~/.tuvis/` directory — no cloud servers, no telemetry; the mobile board rides channels you enable yourself (LAN direct, or your own Cloudflare / Tailscale tunnel). See [Chapter 1](./01-overview.md).

### Which tools are supported?

Eight: Claude Code, Codex CLI, OpenCode, OpenClaw, Kimi Code, WorkBuddy, ZCode, and dsh. Openness varies per tool — see the capability matrix in [Chapter 1](./01-overview.md).

### Why do some tools have fewer features (e.g. no message sending)?

Each tool exposes a different surface: black-box forms like OpenClaw / WorkBuddy have no external write channel, so they can only be monitored, not injected; phone injection is currently experimental and open to Claude Code / Codex CLI / OpenCode / Kimi Code only. Tuvis discloses the differences honestly instead of pretending uniformity — per-tool capabilities in the [Chapter 1](./01-overview.md) matrix.

### Where do updates come from?

The in-app updater prompts for new versions (the update source is GitHub Releases); you can always download installers manually from the Releases page and install over the top. See [Chapter 2](./02-installation.md).

### Does uninstalling clean everything?

On Windows the uninstaller also removes the autostart registry entry; the user data directory `~/.tuvis/` stays in place — delete it manually for a full wipe. For failed upgrades or leftover autostart entries see [Chapter 12](./12-troubleshooting.md).

## Session board

### What do the red / yellow / green lights mean?

🔴 red = waiting for your input / approval, 🟡 yellow = running, 🟢 green = finished / idle; cards sort by "waiting → running → idle". See [Chapter 3](./03-quick-start.md).

## Remote access

### Does the phone side need an app?

No app at all. Enable "Settings → Remote Access", open the `/m`-suffixed address in your phone's browser, and the web board is ready — entry and configuration in [Chapter 8](./08-mobile-remote.md).

## Usage stats

### Why do some metrics show `—` on the usage board?

Metrics whose data cannot be obtained show an empty state rather than 0: "user input (est.)" only exists for claude / codex / kimi; WorkBuddy has no turn concept; dsh's error count and longest single turn depend on raw session logs (retention varies with cleanup policy). Errors are also three per-layer columns plus a separate user-interrupt column — mutually exclusive, so they do not add up. Definitions in [Chapter 11](./11-usage.md).

### Usage stats are off — why can I still export CSV?

Export is not gated by the master usage switch: when off, nothing is collected or written, but the export covers the "already-collected historical ledger" for backup. This asymmetry is a registered open decision, not a malfunction. See [Chapter 11](./11-usage.md).
