# Session Dashboard

The dashboard is Tuvis's main screen: sessions from eight AI coding tools on one board, traffic-light states telling you at a glance who is waiting for you.

![Session dashboard](../img/dashboard.png)

## Board layout

- **Top-bar status summary**: "N sessions" on the left is the total card count; when items need your attention, a red "N pending" counter appears on the right.
- **"Board / Resources" dual tabs**: the Board tab holds session cards, the Resources tab manages skills, MCP, and other resource entries; switching to Resources hides the whole filter chip row.
- **Right of the tab bar**: a "clear finished cards" button lights up when finished cards exist — one click sweeps all green cards away; next to it sits the notification bell (Chapter 6).

1. Open the main window — it lands on the Board tab by default;
2. Check the "pending" counter first; if it is not 0, handle the red cards first;
3. Cards are read-and-cleared: hit "clear finished cards" to wipe the board.

## Session card: four things at a glance

1. **Tool badge**: the source tool as icon + name at the top of the card;
2. **Project & branch**: the project directory name (truncated when long, hover for the full name) and the git branch;
3. **Status light**: red = awaiting approval / your input, yellow = running, green = finished; unread sessions get a halo around the light — click the X at the top-right to mark as read, or let an away-card tuck itself away (it re-appears when state changes);
4. **Last message preview**: a two-line summary of the final message; fine print at the bottom shows **CPU usage, runtime**, and the number of running sub-agents.

> 💡 Cards sort by "waiting → running → idle" priority, so the sessions that need you most sit on top; the "Open on computer" button at a card's bottom-right resumes that session in the terminal with one click.

## Filter by tool

![Filter by tool](../img/dashboard-filter.png)

A row of chips above the cards: "All" on the far left, then seven tools (Claude Code / Codex / OpenCode / Kimi Code / WorkBuddy / ZCode / dsh) — single-select, click to filter, auto-wrapping when they overflow. Your choice is remembered locally — the app reopens on the tool you last picked.

> 💡 OpenClaw does not support message alerts, so it is not in the filter chips (its sessions still appear under "All").

> 💡 Chip icons and labels share one source with the card badges — recognize a face once and find it everywhere.

## Click a card: jump to the terminal

1. Click a session card and Tuvis brings the matching terminal window to the foreground;
2. When one project has multiple windows and several candidates match, a picker lets you choose the target;
3. Forms that do not support jumping say so in place — Tuvis never fakes a successful jump.
