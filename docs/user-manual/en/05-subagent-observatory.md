# Sub-agent Observatory

Background sub-agents dispatched by the main session are no longer a black box — names, elapsed time, tokens and the full execution process are visible in real time.

![Sub-agent list on the mobile board](../img/subagent-mobile.png)

Above: the "Sub-agents" panel of a Claude session on the mobile board (`/m`) — the list is ordered by dispatch, and each sub-agent carries a name, a task title, elapsed time and token usage.

## Running chip: a live strip on the detail page

Open a session's detail page and any running sub-agent shows up as a chip below the message area, reading **name + elapsed time + token usage**. An active sub-agent's clock keeps ticking; once it finishes, the chip freezes with its final numbers. Click a chip to jump straight to that sub-agent's execution details.

## The "Sub-agents" board

In the session detail, the "Sub-agents" board sits level with the file panel:

- one single list, ordered by **dispatch order** — no mixing, no collapsing;
- green dot = active (time / tokens ticking), gray dot = finished (frozen);
- click a list card to open that sub-agent's execution details.

## Execution details

Each sub-agent's detail dialog presents four sections:

1. **Original task** — the task exactly as the main session dispatched it;
2. **Thinking** — collapsed by default, expand when needed;
3. **Tool calls & results** — collapsed to a single line, expandable item by item;
4. **Output** — the sub-agent's final product.

While active, it auto-refreshes every 5 seconds and stays pinned to the bottom; once finished it freezes into a snapshot — reviewable any time, never changing again.

> 💡 The surest sign that a sub-agent has quietly hung: the clock still ticks but the output no longer updates — go look at the main session's terminal.

## Report alert switch

"Sub-agent report alerts" are on by default: when a sub-agent finishes and reports back — waking the main session — you get the alert too. Rather not be nudged? Turn the switch off under "Settings → Notifications & Pet" to silence it independently — the main session's own alerts are unaffected.

> 💡 In the current version (v1), full execution details cover Claude only (both mechanisms are recognized: classic task notifications and teammate idle announcements); other tools still show the sub-agent list and running chips as usual, but execution details are not supported yet.
