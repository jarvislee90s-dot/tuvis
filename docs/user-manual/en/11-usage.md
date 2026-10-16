# Usage Statistics

One ledger shows at a glance how many tokens each tool burns — collected automatically, read locally, zero telemetry.

## The usage ledger

Tuvis keeps one ledger for seven tools — Claude Code / Codex / Kimi Code / OpenCode / ZCode / WorkBuddy / dsh (OpenClaw is not collected yet). It only reads and aggregates each tool's own session records on this machine and reports no telemetry of any kind; the ledger lives in `~/.tuvis/tuvis.db`, hourly-bucket details are kept for 90 days by default (configurable), and only daily aggregates remain after that.

Collection cadence: one automatic catch-up shortly after app launch, then on the configured interval (single-flight mutex plus a minimum-interval floor — it never rides on the 3-second session polling). **Settings → Usage statistics** offers "collect now" at any time plus a master switch — when off, nothing is collected or written (settings and export keep working, and the export covers the existing ledger).

## Opening the big board

The usage button in the main window's **title bar** opens the standalone usage dashboard window; "Details »" on the pet's minibar gets you there too.

![Usage dashboard](../img/usage-dashboard.png)

The big board contains:

- **Metrics grid**: core figures such as token totals (four buckets) and hit rate, each with a period-over-period comparison against the previous equal-length window;
- **Trend card**: a line + gradient area chart whose x-axis granularity follows the time range — peak marked top-right, hover for exact values;
- **Breakdown card**: share bars ranked across three switchable dimensions — **tool / project / provider · model**;
- **Work summary**: session count, turns, errors, longest single turn, tool calls — figures that cannot be obtained show an empty state rather than 0;
- **Export**: copy as text / export image (a share image including the pet's portrait) / export CSV (UTF-8 BOM, Excel-friendly, structured ledger columns only).

Five time ranges are switchable, with custom spans up to 31 days. The "Records" tab in the same window shows one card per tool, one row per "provider / model" spend, groupable by tool / project, with measured / inferred / unknown markers.

## Minibar

Hover the pet for about half a second and the popup that appears is the minibar (the pet's right-click "Today's usage" brings it back anytime): its 5 metric rows are pinned in order (requests + hit rate / cache hit + output / four-bucket breakdown / errors / longest single turn); the time range (today / last 5 hours / last 7 days) and the tool row count (1–7) follow the settings; the trailing "Details »" jumps straight to the big board. The "Today" figures in **Settings → Usage statistics** share the same source.

## Collection criteria at a glance

- **Four buckets** = uncached input / cache read / cache write / output; hit rate derives from cache semantics (mutually exclusive / inclusive / totals-only) — when in doubt it assumes mutually exclusive: better an under-reported hit rate than silent over-counting;
- **Sub-agents included**: token totals include sub-agent usage; turns are never summed across tools;
- **Unavailable shows `—`, not 0**: some metrics exist for some tools only (e.g. "user input (est.)" exists only for claude / codex / kimi);
- **Privacy**: the UI and the CSV contain structured numbers plus tool / model / project / session titles only — no raw prompts, no code content.

> 💡 A `—` in a column is not a malfunction — the true value is unobtainable under that criterion; when a hit rate defies common sense it is usually a cache-semantics misjudgment, so attach the tool version when reporting it.
