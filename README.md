<div align="center">

# Tuvis

**Your Desktop Multi-Agent Command Deck**

A desktop app to monitor, notify, jump to, and manage Claude Code / Codex CLI / OpenCode / OpenClaw / Kimi Code / WorkBuddy / ZCode / dsh sessions

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Tauri v2](https://img.shields.io/badge/Tauri-v2-blue?logo=tauri)](https://v2.tauri.app/)
[![React 19](https://img.shields.io/badge/React-19-61DAFB?logo=react)](https://react.dev/)

English · [中文](README.zh-CN.md)

<img src="docs/images/tuvis-hero.png" width="400" alt="Tuvis — AI Agent for Developers" />

</div>

---

## Features

> Since v0.5.8 every in-app error carries a stable **error code** with localized (EN/中文) wording — include the code when reporting an issue for precise diagnosis.

### Session Monitoring Dashboard

Real-time traffic-light status board for all active AI coding tool sessions.

| Status    | Meaning                |
| --------- | ---------------------- |
| 🔴 Red    | Waiting for user input |
| 🟡 Yellow | Processing / Thinking  |
| 🟢 Green  | Idle / Finished        |

- Auto-discovers running **Claude Code**, **Codex CLI/APP**, **OpenCode**, **OpenClaw**, **Kimi Code**, **WorkBuddy**, **ZCode**, and **dsh** sessions
- Distinguishes CLI vs. desktop APP form: APP sessions support session-level deep-link jumps (`workbuddy://chat/<id>`, `codex://threads/<id>`, with APP-foreground fallback) and persistent unread cards (kept across restarts, cleared when the host exits); dsh (web-hosted) jumps focus/open the dsh tab in your browser (macOS)
- Shows project name, git branch, last message preview, CPU usage, runtime
- Sorts by priority: waiting → running → idle
- **Tool filter chips** ("All" + 7 tools) below the tab bar — icons/colors share one source with the card badges; the choice is remembered locally, the counter stays global, and an empty result names the tool instead of showing a blank board. OpenClaw is not filterable (no message alerts) though its badge remains; a stale OpenClaw filter falls back to "All" automatically
- System tray icon reflects aggregate status (🔴/🟡/🟢)

### Remote Access & Mobile Board (LAN / Quick Tunnel / Own Domain / External Domain)

Open the same eight-tool session board from your phone browser. Settings → Remote Access offers **four channel cards, each with its own switch and usable simultaneously**:

| Channel card                         | When to use                                                   | Notes                                                                                                                                                                                                                                            |
| ------------------------------------ | ------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| 📶 LAN                               | Phone and computer on the same WiFi / cable (**recommended**) | Fastest; requires the access PIN; **your own browser on this computer uses this card's address too** (its switch must be on)                                                                                                                     |
| 🔀 Quick tunnel                      | You're away and your phone is on cellular                     | Public, no signup (Cloudflare); **address changes every time it's enabled**                                                                                                                                                                      |
| 🌐 Own domain                        | You want a stable entry point                                 | Needs a Cloudflare account (free plan works) + a Tunnel Token; the address is **permanent** once configured — Tuvis downloads and supervises cloudflared for you, with a step-by-step guide in Settings                                          |
| 🛰 External domain (no domain needed) | You don't have a domain but still want a permanent address    | **No domain required**: the card's wizard walks you through installing Tailscale and enabling Funnel to get a **permanent** address like `https://<machine>.<tailnet>.ts.net/m`; you only install and sign in to Tailscale once on this computer |

**The access PIN applies to this computer too**: loopback access is **no longer PIN-free** — opening the board from this machine's own browser is treated exactly like a remote device (enter the PIN once, then remembered for 180 days). "This machine" is **no longer a channel card**: it is just one way to connect, using the address shown on the "LAN" card.

![Remote Access · desktop settings page](docs/images/remote-settings-v0.6.0.png)

**Fresh-install defaults**: with no historical database (including legacy-brand data), Remote Access starts enabled, the release app attempts to register launch at login once, and remote-message device signatures are off. Enable individual channels as needed; upgrades keep their previous state. Turning a switch off is respected on later launches. Launch-at-login status reflects the operating system; a failed registration can be retried in Settings.

How to connect: confirm "Enable Remote Access" is on → enable the channel you want → click its card to reveal the address / QR code (**the link already contains the PIN, so scanning fills it in automatically**) → open it on your phone (**the URL must end with `/m`**). **The first run of "External domain (no domain needed)" goes through the card's wizard**: click "One-click setup" and it walks through detect → download from the official source with checksum verification → install (the system installer opens on macOS; an MSI wizard opens on Windows) → sign in to Tailscale (Tuvis turns the authorization link into a button; you finish in the browser) → turn off "block incoming connections" → enable Funnel → **reachability verification** — no command line at any point; on macOS you also approve Tailscale's system extension once in System Settings, and the first Funnel enablement may additionally need one browser approval (or none at all).

**Tailscale installer and manual installation**: on macOS and Windows, packages are saved in the system Downloads folder, including redirected Windows folders. The wizard follows a vertical timeline, with actions on the right and completed steps in green. The actual path and “Open downloads folder” appear only after download and verification; the pinned official “Download manually” link appears only if automatic download fails. If the installer cannot open, double-click the verified package from its downloads folder. On macOS, clicking “Install Tailscale” opens the system installer automatically; follow its instructions and authorization prompts, then return to Tuvis and click “Refresh status”. Cancelling does not mark installation complete.

**Both fixed-address wizards**: Own domain assumes your domain is already on Cloudflare and guides you through tunnel creation, Token entry and a confirmed connection, browser-side hostname binding, and board-response verification. External domain (Tailscale) follows its installation and enablement steps. Once required steps and public board verification pass, the timeline collapses and the address, copy/open actions, and QR code stay in the channel card; “View setup steps” reopens it. Later failures keep the summary with the previous address clearly marked unavailable; open and QR actions pause without restarting installation.

**Waiting for public DNS**: when the local board and Funnel are ready but public records are absent, the wizard shows elapsed time and an estimate of several minutes, potentially around ten minutes, while checking read-only about once per minute. No repeated retry is needed; overdue waits provide troubleshooting. Refresh and ordinary checks never reset Funnel; the explicit “Re-enable” repair retains configuration ownership safeguards. An enabled macOS extension needs no further action; a stopped backend first calls for checking the Tailscale client.

**The address is only shown after it passes a real check from outside the tailnet**: while verifying or not yet effective, the card honestly reports "taking effect" and gives the expected wait for the situation (several minutes, potentially around ten minutes, for public DNS; about 1–2 minutes when recovering after boot) — it never puts a dead link in front of you. Once verified it is re-checked periodically (about once a minute, less often when idle) and continues read-only checks on failure; an explicit repair requires your confirmation. The address never changes, paired devices never need re-pairing, and **Tuvis restores the channel automatically on every boot** (it waits for the Tailscale backend to be ready before re-enabling, so it never wipes a config that is still coming back).

The PIN applies to **all four channels** (**including access from this computer**): enter it once per device for **180 days**, and after a PIN change every device must re-enter it. Paired devices can be renamed or kicked from the "Paired Devices" list (up to 10).

> **Stated honestly**: the platform notes inside the wizard match the implementation — on Windows only the second half has been tested on a real machine (detect, enable, revoke and restart recovery after "installed / signed in"), while the **install and sign-in flow has not been run end to end**; on macOS the **write paths** (enable / revoke / self-heal) are **not yet verified on a real machine** either (read-only probing has been). The wizard says so on the spot per platform and does not pretend the whole flow has been verified.

**Live board**: session status changes are pushed to your phone within ~2 seconds (SSE as the primary channel) — banner, chime, and vibration, with a tap taking you straight to that session; if the stream drops it degrades to 3-second polling automatically. Transition deduplication uses the same server-side logic as desktop notifications.

**Session detail page** (ZCode-style conversation view, unified across all eight tools):

- Expand details while running (thinking / tool calls are collapsible), auto-collapse to the final summary once a turn completes
- Markdown rendering (headings / lists / tables / code highlighting); project file paths in the text become clickable links
- **File panel**: aggregates the files a session touched, newest first, with document/image filters and a **200 / 500 / 1000 message** look-back range; **modified** files are highlighted and **read-only** ones are neutral (both previewable), and tapping the secondary line reveals the full path. Since v0.5.8 a fourth source, **Session artifacts**, scans ordinary message text for artifact links and inline file references (paths in code blocks are excluded); whitelisted formats preview on tap (desktop and mobile alike). **Since v0.6.0, recognized file paths in Codex APP messages open the preview, and uploaded/referenced images, generated images, and files changed or viewed appear as structured artifacts with source labels. File entries can jump back to the latest message reference within the current tail and up to four earlier pages of 50 messages each.**
- **File preview**: markdown / syntax highlighting / inline images; three switchable layouts — side-by-side (chat left, file right), stacked, and fullscreen overlay — with a draggable splitter; readable scope is the session project directory plus your home directory (sensitive directories such as keys are always refused)
- **Transfer progress and honest bandwidth disclosure**: uploads and downloads both show a **progress bar** (bytes transferred / percentage / instantaneous rate / ETA); **when the rate or the total size is unknown it shows bytes transferred only and never invents a percentage**. On a bandwidth-limited tunnel channel the file panel and the attachment area say **in place** that this is a channel limit, not a malfunction, with time estimates based on this channel's measured rate (about 1.5 minutes to download and 3.3 minutes to upload a 20 MB attachment) and a **full-speed upgrade path** (install Tailscale on your phone and join the tailnet to connect directly, 1–2 orders of magnitude faster)
- **Message bookmarks**: mark a spot to revisit with a colored dot (up to 10, one per color), jump back by tapping it, delete individually or clear all; stored in the browser — refresh-proof, cleared when Tuvis restarts or the tab is closed
- **Font size**: 50% / 75% / 100% / 125% for message and file text only
- **Archived history**: finished or unreachable sessions stop cluttering the board — registry-based archiving (history page lazy-loads 1/3/7 days), tap a card for a read-only detail view with one-tap reactivation; board cards can be closed/archived (CLI sessions stop their process, APP sessions get a true "close")

### Remote Control · send messages & approve from your phone (v0.5.0, experimental)

Not just looking — you can **act**: send messages, approve tool calls, answer questions, switch permission modes, and even **create new sessions** from your phone (currently **Claude Code / Codex CLI / Kimi Code / OpenCode**, experimental):

- **Create sessions**: start a brand-new CLI session straight from the phone — pick a known project or type a path (checked against a blocklist), optionally with an opening message injected automatically; creation progress streams back and the board jumps to the new session when it is ready. Per-tool popup shapes (trust prompts, update notices, …) are handled automatically, verified by on-machine E2E
- **Send & queue**: messages queue automatically while a session is busy, "send now" interrupts and jumps the queue (per-tool key sequences measured on real machines), and queued messages can be retracted. Every message uses **key-sequence injection with a screen-read receipt** (character-by-character tail verification) — a non-delivery is reported honestly, never faked as success
- **Remote approvals**: tool-call approval (allow/deny), plan confirmation (plan body plus terminal dialog options synced live from the screen, direct digit selection), Claude plan-approval digit shortcuts plus a **plan feedback bar** (send written feedback on an approved plan, or clear it in one tap); the approval card dismisses itself once its job is done
- **Remote Q&A**: single-choice, multi-select, and free-form answers (including **Other — overwrite & clear**: enter edit, backspace to empty, then type; an empty text is rejected honestly), per-question progression with a confirmation card; multi-select taps only toggle, "next question" advances explicitly (opencode/kimi also support ◀/▶ back and forth) — the phone and the terminal always stay on the same question. **Confirmation-card answers are sourced from the terminal's Review page via screen reading**: after a refresh or page reopen they match the terminal summary exactly; local memory is only a fallback
- **Permission modes**: kimi's three tiers (`/permission` / `/yolo` / `/auto`) use a **two-step flow** — open the menu, screen-read the highlighted row, only then press Enter — plus a **mode/permission dual read-back** in the status bar (both the switch receipt and the current tier carry screen-read evidence); codex switches via the terminal menu single-select
- **Guards**: while a dialog is pending, ordinary message injection is blocked (to avoid answering the default by accident); input-line residue is detected to prevent double sends; dangerous keys (such as Ctrl+C, which quits OpenCode) are blacklisted
- **Codex desktop APP remote control (experimental, unlocked per exact Desktop build)**: Windows `26.1013.4894.0` supports message projection / sending / status / command approval / Plan blocking Q&A; macOS `26.1013.40934` supports sending / status. `permissions` stays disabled; other builds or unsupported capabilities are refused honestly as "not unlocked / work in progress" — no fake approvals or empty sessions. Recognized file references in message bodies open the existing preview; returning from a preview searches the current tail plus at most four earlier pages for the latest reference. Preview always reads the file's current contents and does not retain historical versions. See the capability matrix below.

|                Session detail · messages                |              Send · queue & jump               |
| :-----------------------------------------------------: | :--------------------------------------------: |
| ![Session detail](docs/images/mobile-detail-v0.5.0.png) |  ![Send](docs/images/mobile-send-v0.5.0.png)   |
|                  **Approvals · plan**                   |             **Q&A · multi-select**             |
|   ![Approvals](docs/images/mobile-approve-v0.5.0.png)   | ![Q&A](docs/images/mobile-question-v0.5.0.png) |

**Can't connect? Check these first (every item below is a pit we actually hit):**

1. **The router has "client isolation / AP isolation / guest network" enabled** — the most elusive one: your phone and computer are on the same WiFi, yet the router forbids wireless clients from talking to each other, so nothing connects. Turn that setting off in the router's admin page (field note: everything else checked out; this turned out to be it).
2. **The quick tunnel address changes every time it is toggled** — each time you enable the quick tunnel a new public address is generated (the old link / QR code dies immediately), so don't treat it as a long-term entry point; for a fixed address use "Own domain" or "External domain (no domain needed)". Toggling other settings does not affect this channel.
3. **The phone URL must end with `/m`** — a full address looks like `http://192.168.x.x:9420/m`; scanning the QR code is recommended (**the link already contains the PIN, filled in automatically**), and typing it without `/m` gives you an empty 404 page.
4. **Access from this computer needs the PIN too** — "This machine" is no longer a channel card: opening the board in this machine's own browser is treated exactly like a remote device (enter the PIN once), using the address shown on the "LAN" card, and **that switch must be on** (the address is unreachable when the server only listens on loopback); on your phone use the "LAN" card or a tunnel address.
5. **Firewall** — on Windows the first listen triggers an "allow access" prompt; if it was dismissed, inbound traffic is blocked: Windows Security → Firewall & network protection → Allow an app through firewall → tick Tuvis (both Private and Public).
6. **The TLS confirmation dialog** — a direct LAN connection is plain HTTP; ticking the box means "I understand / I have a TLS reverse proxy in front"; for a home LAN just tick it — you don't need to actually set up a reverse proxy.
7. **Own domain is connected but the domain won't open** — follow “Bind public hostname” in the channel timeline: in Cloudflare, go to Routes → Add route → Published application, configure the subdomain, choose HTTP for Type, and use the current origin address shown by the wizard for Service. Save the route and return to Tuvis to verify; setup completes only after the public board response passes verification.
8. **The “External domain (no domain needed)” card keeps waiting for public DNS** — publishing records or refreshing a negative cache can take several minutes, potentially around ten. Keep the channel enabled; read-only checks run every minute. Retry is hidden during this wait, and overdue waits provide troubleshooting. Ordinary checks never reset Funnel; only an explicitly confirmed re-enable performs repair. An enabled extension needs no further approval; recovery after boot normally means the Tailscale backend is reconnecting.

### Built-in Pets (tuvis-3D / Foxbell)

Built-in companions live in a corner of your screen and watch every session in real time: **tuvis-3D** (a 3D tech rabbit, the default since v0.5.8) and **Foxbell** (a fox with 31 voice clips).

![Built-in desktop pet](docs/images/foxbell-pet.png)

- Status cards above the pet mirror the dashboard: 🔴 waiting / 🟡 running / 🟢 finished — click a card to jump to its terminal
- Voice alerts (each pet ships its own clips: tuvis-3D 16 / Foxbell 31): playful nudges on waiting approvals, cheers on completion, small talk on double-click, subtitles synced to audio length
- Drag physics: pinned-to-cursor dragging, gravity fall on release, throw inertia, squash-and-bounce landing (optional; **defaults to off after every app restart** since v0.5.8 — re-enable per session)
- Single-click waves, double-click talks, right-click menu: sound / subtitles / physics / always-on-top / size / per-scene action binding / hide
- Dashboard integration: takes over completion chimes, suppresses toast popups while always-on-top; toggle from the dashboard 🦊 button, system tray, or settings
- **"Launch with the app" switch** (Settings → "Notifications & Pet" block, on by default): decides whether the pet follows Tuvis at startup; once turned off it stays off and is no longer overridden by the startup fallback logic
- **Switch built-in pets**: Settings → "Notifications & Pet" → "Switch pet" lists tuvis-3D first and Foxbell second; hot swap, no restart. Choices from earlier versions carry over automatically (legacy `foxbell` activation records are recognized)

#### External Pets

Since v0.3.0 the pet format is open — beyond the two built-ins:

- **Import custom pets**: from a local zip / directory, or download from the Petdex online repository; manifest structure, frame rate, dimensions and voice manifests are fully validated
- **Manage panel**: import / edit description / rename / delete / one-click hot swap — no app restart needed; the active pet is auto-restored after deletion or switching
- **Capability gating**: pets without voices gracefully degrade to animation-only (transient actions kept); voice capabilities stay in two-way sync

### Token Usage Dashboard (Usage Ledger)

Token usage from seven tools (Claude Code / Codex / Kimi Code / OpenCode / ZCode / WorkBuddy / dsh) is accounted
for in one ledger: the app collects once shortly after startup, and `usage_collect` lets the frontend trigger a
collection on demand (single-flight mutex + a default minimum interval of 10 minutes);
**collection runs on its own on-demand path and never enters the 3-second session polling loop**. The ledger
lives in 4 tables in `~/.tuvis/tuvis.db` (hourly detail, permanently kept daily aggregates, collection cursors,
session dimension); detail is kept for 90 days by default with a 90-day maximum (configurable), and after expiry only the daily
aggregates remain.

**Dashboard V2**: A Token overview stays expanded (new input / cache hit / cache miss / output /
hit rate / requests; the former "cache write (explained)" block is removed and meter labels are unified as
new input / cache hit / output / total); B Usage trend
filters locally inside the card by **All / By tool / By channel**, with peak and period comparison using the
same scope and no effect on the global overview; **"channel" means the provider alias** (the provider display
name; unedited = raw name;
different raw strings can map to the same display name and merge across tools; an explicit billing binding wins;
tools are not a channel dimension), and non-empty raw provider strings are used when no alias exists; C Usage
distribution switches between **By channel / By tool / By project / By model**, shows Top 5 by default with "show
more", and lists tokens, cache hit rate and a relative bar per row; the same display name merges across tools,
identically named models no longer produce indistinguishable duplicate rows, an empty model shows "unknown
model"; when an empty provider has no assignment, the channel view merges it into one "unassigned channel" row
and the model view splits by tool; the channel view also has a "channel may be mixed" row for log identities
that may mix multiple accounts and cannot be split, and neither "unassigned channel" nor "channel may be mixed"
is offered as a trend filter option. Work activity is collapsed by default, with p50 / longest turn / tool
duration inside its advanced stats. C and Work activity are collapsible; A/B are fixed. Records-page row names
now prefer the **provider alias**: they show "alias / raw model" and fall back to the raw "provider / model" when
no alias exists, while the raw identity stays in the title and the row data; **the channel / model dimensions of C and the D
cost-row tooltips** show raw provider evidence — when multiple raw providers merge, the complete list is shown,
and groups that contain a non-empty provider together with an unlabeled source are marked
"includes unlabeled source" (the tool / project dimensions show no provider evidence). Ordinary rows no longer
show "Assign channel / Edit assignment"; only ambiguous rows offer "Bind billing attribution", and the legacy
billing attribution / reference rules live behind the "Advanced" entry in the Records footer; every row still
has a "Provider display name" entry — a non-empty provider can map to a display name, and an empty provider's
"tool + model" entry can be given an alias individually (applies retroactively once saved); the dialog also
previews the rule's matching tools, models and total tokens over the last 30 days before saving. Known limitation:
when two subscriptions log the exact same raw provider string, aliases cannot split them and the channels stay
merged; only the ambiguous row's "Bind billing attribution" or the footer's advanced entry (explicit billing
binding) can help, and tokens are never split proportionally. Settings → "Display & pricing config" (the same entry as the cost card's "Manage config") and
"Custom price rules" now live in **one shared dialog**: the settings page and the D card open the same dialog, and
the D card's "Manage config" opens it directly; the upper part holds the provider/model entries and the lower part
is a collapsible custom price rules section (collapsed by default). Settings and the D card are separate WebViews;
changes in Settings do not guarantee an immediate refresh of an already-open usage dashboard, and the in-dashboard config is the primary path. Entries default to the top 10 by usage in the last
7 days, with "show all" and search at the bottom, plus three sort keys (usage / raw provider / raw model, click again
to reverse). Each entry has three rows — raw identity (read-only) / provider
name (editable, applied globally to the same raw provider; empty-provider entries write a toolModel alias and stay
"unassigned channel" when unnamed) + model name (editable, applied per (display provider, raw model)) / pricing
identity (dropdown / search selection only). "Custom price rules" supports **fixed** and **time-of-day** pricing:
time-of-day uses peak rates inside multiple Monday–Friday peak windows (cross-midnight allowed) and off-peak rates
elsewhere; a blank off-peak component falls back to the catalog rate; the time zone can be China Standard Time, UTC
or system local and only affects API billing peak/off-peak calculation; a "peak × %" helper fills the off-peak rates.
Legacy billing-attribution binding / reference rules moved to the Records page footer's "Advanced" entry. Every advanced
option has an Info icon with a hover/focus explanation; each model entry also has a "Set custom price /
Edit custom price" shortcut that pre-fills the raw model name (normalized on save) and shows a "Custom price applied" badge when
present. The rule applies globally by **normalized model name**: no provider needed, custom display names do
not participate, and it is not selected in pricing identities or reference rules. Pricing is matched automatically from the public
models.dev catalog, with curated sources ranked **direct official providers (including China-site variants) >
OpenRouter**; all-zero quotes are skipped in favour of the next non-zero candidate. Model IDs are mechanically normalized then matched exactly; within a tier, the
latest `release_date` wins and the normalized model shares one price per currency. Prefix fallback is used only
when the candidate is unique, or when multiple candidates match but all share exactly the same price; cross-family
substitution still requires an explicitly chosen reference model. Models missing from the curated set stay
unpriced by default — pick a catalog entry explicitly via the pricing identity (the catalog search is curated-only
by default, with a "show all N sources" switch). Conditional quotes (multimodal input, context tiers, etc.) are no
longer discarded: when base text rates exist, the estimate uses them and the caveat line says "estimated at base
text rates"; missing prices still render `—`, never 0. **The D card is expanded by default** and renders one card
per model (five columns: channel / model | estimate | total tokens | cache hit rate | billing detail), with the
detail expanded inside the card as four rows (new input / cache hit / output / total) × four columns (item /
tokens / amount / unit price). Main and detail headers use solid foreground text; the total row leaves the unit
price blank, and cache write appears only when data exists. A "Sync prices" button in the card header force-refreshes
models.dev + native RMB sources + the daily FX rate (loading / inline status), while daily/stale background refresh
and first-enable refresh remain. The footnote keeps only the normalized model and snapshot fetch time — **a model
release date is not a price date**; caveats join into one dot-separated line. **CNY prefers native RMB prices
explicitly written in the models.dev source TOMLs and converts at the daily rate when missing; USD prefers the
catalog's native USD price. Native-currency prices render normally; amounts/unit prices converted by FX carry a
dashed underline (legend in the card header)**. The header keeps the USD / CNY quick switch; the ledger and cost
CSV stay in USD. Cost granularity: the last 7 days / 30 days / custom spans (≤31 days) are priced from **hourly
details aggregated per day**, so dsh is fully priced across input / cache hit / output; when lowered detail
retention or cleanup leaves days without details, the whole span falls back to daily aggregation (dsh stays
output-only, disclosed in the caveats); the last 5 hours / today are unchanged. "Pricing catalog status" stays in
Settings and enables/refreshes the catalog (falling back to the last valid snapshot on failure); the override list
also shows the catalog source provider/model, release date and snapshot date, and later syncs never overwrite user
values.

API-equivalent costs are estimated per channel / model row, with **no global cost total or channel subtotal**; the
export bar has a separate **Cost CSV** (all cost rows for the current window, no totals, no BOM added by the
frontend; seven columns — channel key / channel kind / display name / normalized model / catalog release date /
override flag / price-rule kind — are appended after the existing 17, and the single-value `observed_provider` column is left empty
when a row merges multiple raw providers or includes an unlabeled source, with the full evidence available in the
UI tooltip).

**How the numbers are defined (read this first)**

- **Four buckets**: uncached input / cache read / cache write / output; **new input** (the ledger's request
  total) and the
  **cache hit rate** are derived from the three cache-semantics modes (exclusive / subset / total-only).
  Sources disagree on semantics, and **records of the same tool can differ too** — Codex decides per record
  by arithmetic on `total_tokens` (measured on this machine: 92.69% with the correct rule; treating
  everything as "subset" yields absurd values above 100%). When in doubt we treat it as "exclusive"
  (a wrong call shows a lower hit rate instead of silently over-counting).
- **Total processed tokens = new input + output**; **cache-miss input = new input − cache read**
  (same formula for the current and previous windows). V2 removes "user input (est.)" from the dashboard and
  shared summaries (it is not API tokens) and unifies meter labels as new input / cache hit / output / total;
  the ledger field and the raw Token CSV column remain for compatibility.
- **Sub-agents included**: token totals include sub-agent / sidechain usage; **session and turn counts are
  layered by parent/child** (one conversation is not counted as several).
- **The sub-agent flag is only meaningful in the hourly tier**: `isSubagent` is `true` / `false` for hourly
  grouped rows, and is the empty state `null` in the daily tier and for record-page card rows (neither tier
  has a session dimension, so it cannot be computed) — `null` must **not** be read as `false` ("no sub-agents").
- **Turn counts are never summed across tools** (each source's rule is not comparable); **the longest single
  turn is reported per tool as p50 + max**, and **it includes idle time (idle is not excluded)**.
- **Errors are split into three layers, plus a separate "user interrupted" column** (the three layers are
  mutually exclusive; adding them up is meaningless).
- **Unavailable metrics show an empty state, never 0**: WorkBuddy has no turn concept, no duration field and
  no provider; OpenCode has no tool-level errors; kimi has no user-interrupt rule; dsh errors and longest
  turns can only come from raw session logs (only a subset of sessions still have raw logs — **no ratio is
  hard-coded**, since coverage varies with the machine and cleanup policy; the earlier "19/99" is obsolete).
- **Master switch (on by default)**: when it is off, **nothing is collected and nothing is written to the
  ledger** (retention cleanup is skipped as well) and dashboard / record queries return the off-state early;
  **CSV export is currently not gated by the master switch**, so existing ledger history can still be exported
  while it is off (a registered decision pending review).
- **"By project" follows record ownership**: one session file can span several projects (only the cwd inside a
  given record counts as its project); project names come from the same source as the dashboard session cards
  (`projectName`); on Windows the same directory may split into two groups because path casing differs between
  sources; records without a cwd fall into `Unknown`.
- **Privacy**: ledger queries and the upcoming surfaces contain only structured numbers and static text
  (tool name, model name, project name, session title) — **no prompt text and no code content**;
  CSV export contains only structured ledger columns, and **session titles and raw paths never enter the CSV**.
  CSV export is UTF-8 with BOM (Excel-friendly).

### Desktop Notifications & Sound Alerts

- Color-change-based notifications (red↔yellow↔green) with deduplication
- Web Audio API chimes — no audio files needed
- Configurable on/off toggle in settings
- Clickable notifications with "View Session" action to jump to terminal

### Launch at Login

- A fresh release installation attempts to enable launch at login once; existing installations and legacy-brand migrations keep their state. Failed registration can be retried in Settings.
- Toggle it from the Settings → "Notifications & Pet" card, or from below the tray's "Show Window" item — both share one read/write path and keep the checkbox state in sync
- What you see is the **real OS-side state, read live** (Windows registry Run key, including the enable/disable flag Task Manager writes; macOS login items) — no local cache, and a failed toggle rolls back and reports honestly
- When launched by autostart, the main window **starts hidden in the tray** and the desktop pet still follows (no window popping up on boot)
- On Windows, uninstalling removes the startup entry for the user who uninstalls it; macOS has no uninstall hook, so remove the matching plist under `~/Library/LaunchAgents/` manually

### Quick Terminal Jump

Click a session card to instantly focus the corresponding terminal tab:

| Terminal     | Support                            |
| ------------ | ---------------------------------- |
| iTerm2       | ✅ AppleScript                     |
| Terminal.app | ✅ AppleScript                     |
| tmux         | ✅ pane selection + terminal focus |
| Wayland      | ❌ Graceful fallback message       |

Terminal tools (Claude Code / Codex CLI / OpenCode / Kimi Code) resolve through process-tree and window-content disambiguation; **same-project dual-open jumps land directly**: for Kimi / OpenCode, the window title is matched against the session title (kimi `state.json` title / OpenCode DB title) after normalization — a unique hit locks onto the window, so dual terminals no longer raise a picker. On Windows, resolution also stamps a one-shot identity marker onto the target terminal title (` — Tuvis:xxxxxxxxxxxx`, cleared automatically after focus) for positive locking; markers never stack, and when the card↔terminal pairing is uncertain (same-project multi-open) the app **raises a picker rather than risking the wrong window**, and focus refusals surface an explicit error instead of failing silently.

Desktop APP tools (Codex APP, WorkBuddy) support deep-link jumps: `codex://threads/<id>`, `workbuddy://chat/<id>` (session-level). The handler is verified before dispatch and foregrounding is verified after; on failure it falls back to APP-level focus (macOS AppleScript / Windows nearest-ancestor) without marking the session read. ZCode is a single-window multi-tab app — its jump simply focuses the unique window (cards carry the host pid, zero ambiguity). dsh's jump focuses (or opens) the dsh web tab in your browser.

### Extension Resource Management

Unified repository for Skills, MCP servers, and Plugins across tools:

- **Skills**: Symlink (Unix) / Junction (Windows) mapping to each tool's skill directory
- **MCP Servers**: Auto-format conversion — JSON (Claude / Kimi / WorkBuddy) / TOML (Codex) / JSONC (OpenCode) / nested JSON subtree (ZCode: `mcp.servers`; read-modify-write touches only that subtree, preserving unknown keys and original key order)
- **Plugins**: File/config hybrid management
- Auto-import existing skills on first launch (from per-tool directories such as `~/.claude/skills/`, `~/.codex/skills/`, `~/.config/opencode/skills/`, plus the shared directory `~/.agents/skills/`)
- `~/.agents/skills/` is a **read-only shared import source** (source label `agents-shared`): Tuvis only scans it into the repository (no tool attribution, no linking); tools that follow the open standard, such as codex / zcode, read this directory directly
- Rescan button for discovering newly installed skills

### Preset Groups

Bundle Skills + MCP servers + Plugins into named presets and apply/deactivate in one click:

- One-click apply to any tool — auto-adapts to each tool's config format
- Partial success handling: reports failures without rolling back successful items
- Conflict detection: skips already-existing resources
- System tray menu integration for quick switching

### Sub-Agent Resource Allocation

For multi-agent tools (Hermes, OpenCode, etc.), allocate resource subsets to sub-agents:

- Sub-agent allocation is constrained to the tool-level enabled range
- Tool-level disable cascades down to all sub-agents

### Tool Toggle Management

A dedicated settings section to decide which tools Tuvis monitors and manages:

- Row-style toggle list: icon + name + installed badge; changes are staged locally and batch-saved, with a confirmation dialog listing restore/rollback items and an unsaved-changes leave guard
- Unchecking = full restore: symlinks become real files, MCP entries are removed from tool configs, unread cards are cleared; the SSOT repository and DB assignments are kept, and re-checking rebuilds everything per the original assignments (partial failures auto-rollback — re-saving retries idempotently)
- Unchecked tools are fully hidden: session scanning skips them, notifications are muted, resource/preset UIs hide them, and guarded commands return structured, localized errors

### Signal Health (Hook trust-gate self-check)

A settings section under the "Tools & Health Check" block to verify each tool's Hook channel is actually working:

- Every Hook-capable tool shows registration status, last event time, and pending prompts (data comes from the existing Hook event directory and session-scan snapshots, fetched on open/manual refresh — no background polling)
- Criterion: registered + an active session exists for the tool + zero events within a 30-second window → shows "enter /hooks in the tool's terminal and trust the Tuvis entry (one-time)" with a one-click copy of the `/hooks` command
- Unregistered tools get an honest reason (e.g. kimi on macOS shows "the Hook channel is not distributed on this platform; nothing to do") instead of a bare "unregistered"
- codex has an extra trust gate: Tuvis being registered ≠ events firing — you must run `/hooks` inside the Codex TUI and trust it once (the trust hash lands in your user-level config). After registration Tuvis raises a **persistent in-app action card** instead of a one-shot desktop notification: raising the card ≠ dismissing it; only "Copy command" or "Don't remind me again" clears it, and the card carries the command text as a clipboard fallback

---

## Tool Support Matrix

Capabilities across the 13 capability rows × 8 terminal-class AI coding tools (✅ supported / ◐ partial / 🧪 experimental / ❌ not supported) — this is the master capability table, updated with every release (structured source: [`docs/capabilities.json`](docs/capabilities.json), shared with the website):

| Capability                         | Claude Code | Codex CLI | OpenCode | OpenClaw | Kimi Code    | WorkBuddy | ZCode           | dsh         |
| ---------------------------------- | ----------- | --------- | -------- | -------- | ------------ | --------- | --------------- | ----------- |
| Session monitoring                 | ✅          | ✅        | ✅       | ✅       | ✅           | ✅        | ✅              | ✅          |
| Desktop notifications              | ✅          | ✅        | ✅       | ✅       | ✅           | ✅        | ✅              | ✅          |
| Skill management                   | ✅          | ✅        | ✅       | ✅       | ✅           | ✅        | ✅              | ◐ read-only |
| MCP management                     | ✅ JSON     | ✅ TOML   | ✅ JSONC | ✅ JSON  | ✅ JSON      | ✅ JSON   | ✅ JSON subtree | ❌          |
| Plugin management                  | ✅          | ✅        | ✅       | ✅       | ◐ file-based | ❌        | ❌              | ❌          |
| Status hooks                       | ✅          | ✅        | ❌       | ❌       | ✅           | ❌        | ❌              | ❌          |
| Mobile · message viewing           | ✅          | ✅        | ✅       | ❌       | ✅           | ✅        | ✅              | ✅          |
| Mobile · file preview              | ✅          | ✅        | ✅       | ❌       | ✅           | ✅        | ✅              | ✅          |
| Mobile · send messages (injection) | 🧪          | 🧪        | 🧪       | ❌       | 🧪           | ❌        | ❌              | ❌          |
| Mobile · create sessions           | 🧪          | 🧪        | 🧪       | ❌       | 🧪           | ❌        | ❌              | ❌          |
| Token usage collection             | ✅          | ✅        | ✅       | ❌       | ✅           | ✅        | ✅              | ✅          |
| Mobile · sub-agent observatory     | ✅          | ❌        | ❌       | ❌       | ❌           | ❌        | ❌              | ❌          |
| Mobile · Codex APP control (Desktop IPC) | ❌    | 🧪        | ❌       | ❌       | ❌           | ❌        | ❌              | ❌          |

**Codex APP remote control (v0.6.0, experimental)**: Codex only, unlocked per exact Desktop build (Windows `26.1013.4894.0`: send/status/command approval/Plan blocking Q&A, `permissions` off; macOS `26.1013.40934`: send/status). Recognized file paths in message bodies open the preview; the file panel shows uploaded/referenced images, generated images, and files changed or viewed with source labels, and entries can jump to the latest body reference (current tail plus up to four earlier pages of 50 messages each). Unsupported capabilities are refused honestly — no fake approvals or empty sessions; previews read current file contents and retain no history. Real-phone PRODUCT acceptance is not fully executed yet; see `docs/superpowers/reviews/2026-10-20-codex-app-product-acceptance.md`.

**Three-state capability badge for Codex APP remote control (mobile)**: capabilities are shown as three honest states instead of claiming theoretical support — **verified** (that Desktop build has real-machine evidence: controllable / sendable), **compatibility send (experimental)** (unknown build: send/status default to on per the 2026-10-22 ruling, while destructive capabilities — interrupt, approvals — stay evidence-gated: Tuvis first runs a **read-only probe** on that build (15-second hard cap server-side) and takes schema evidence from the bundled CLI, unlocking them only after evidence; while a probe is in flight the UI shows only a "probing" skeleton and **does not prematurely declare the build locked**), **locked** (the server states it is unavailable).

**Evidence level of Codex APP approvals (reported as-is)**: **command approvals** have real-machine evidence (accept/decline closure on `26.1013.4894.0` + the pending shape on `26.1016.6573.0`); **file approvals** currently have **official method-table / call-site** evidence only — **not yet closed end-to-end on a real machine**.

**Mobile remote control (v0.5.0, experimental)**: message viewing and file preview cover 7 of the 8 tools (OpenClaw's black-box build is not remotely readable yet); **sending messages to CLI sessions and creating sessions from your phone are experimental**, currently supporting Claude Code / Codex CLI / OpenCode / Kimi Code. Messages are injected as keystrokes into the terminal and verified by screen-reading receipts (no false "delivered"); queueing, interrupt-and-jump, remote approvals, question answering, permission-mode switching, and session creation are included. See the Chinese README for illustrated walkthroughs and per-tool limitations.

---

## Tech Stack

| Layer              | Technology                                                                               |
| ------------------ | ---------------------------------------------------------------------------------------- |
| Desktop Framework  | [Tauri v2](https://v2.tauri.app/) (Rust)                                                 |
| Frontend           | [React 19](https://react.dev/) + [TypeScript](https://www.typescriptlang.org/)           |
| UI Components      | [shadcn/ui](https://ui.shadcn.com/) (Radix UI)                                           |
| Styling            | [Tailwind CSS v4](https://tailwindcss.com/)                                              |
| State Management   | [Zustand](https://zustand-demo.pmnd.rs/)                                                 |
| i18n               | [i18next](https://www.i18next.com/) (Chinese / English)                                  |
| Database           | [SQLite](https://www.sqlite.org/) (via [rusqlite](https://github.com/rusqlite/rusqlite)) |
| Process Monitoring | [sysinfo](https://github.com/GuillaumeGomez/sysinfo)                                     |

## Architecture

```
src-tauri/src/
├── adapter/           # Agent adapter trait + per-tool implementations
│   ├── claude.rs      #   Claude Code (JSONL + Hook)
│   ├── codex.rs       #   Codex CLI/APP (JSONL + Hook)
│   ├── opencode.rs    #   OpenCode (SQLite)
│   ├── openclaw.rs    #   OpenClaw (state.json)
│   ├── kimi.rs        #   Kimi Code (session_index + wire.jsonl)
│   ├── workbuddy.rs   #   WorkBuddy (heartbeat-driven + JSONL)
│   ├── zcode.rs       #   ZCode (host detection + SQLite session aggregation)
│   ├── dsh.rs         #   dsh (web host + zstd multi-frame logs)
│   └── mod.rs         #   AgentAdapter trait + tool registry + session discovery scheduler
├── monitor/
│   ├── process.rs     #   Process discovery (sysinfo scan)
│   ├── claude_parser.rs   # Claude parser (message.role protocol)
│   ├── codex_parser.rs    # Codex parser (rollout JSONL protocol)
│   ├── opencode_parser.rs # OpenCode SQLite parser
│   ├── openclaw_parser.rs # OpenClaw state.json parser
│   ├── kimi_parser.rs     # Kimi Code parser (session_index + wire.jsonl)
│   ├── workbuddy_parser.rs # WorkBuddy parser (heartbeat + JSONL tail)
│   ├── zcode_parser.rs    # ZCode parser (tasks-index + session/message/part dual SQLite)
│   ├── dsh/           #   dsh parser (projcache + zstd logs + status/preview, 5 modules)
│   ├── jsonl.rs       #   Shared JSONL reading (tail read, file enumeration)
│   ├── cwd.rs         #   cwd normalization (process ↔ session matching)
│   ├── git.rs         #   GitHub URL lookup (in-process cache)
│   ├── path_codec.rs  #   Claude projects dir-name codec
│   ├── project.rs     #   Project name extraction + cwd shape validation
│   ├── status.rs      #   Pure-message status determination
│   └── hooks.rs       #   Hook registration + event file reader
├── services/          #   Business services split by domain
│   ├── skill/         #   Skill install/enable/disable + auto-import
│   ├── resource/      #   Resource scan, SSOT import, link sync
│   ├── mcp/           #   MCP config writer (JSON/TOML/JSONC)
│   ├── preset/        #   Preset apply/deactivate + compatibility check
│   ├── plugin/        #   Plugin management
│   └── manifest/      #   Extension manifest validation + update check
├── linker/
│   ├── mod.rs         #   Symlink/Junction management + security checks
│   ├── detector.rs    #   Tool installation detection
│   ├── layer2.rs      #   Layer 2 tool-level active directory
│   └── layer3.rs      #   Layer 3 sub-agent-level active directory
├── commands/          #   Tauri IPC commands split by module
├── database/          #   SQLite data layer (schema/migration/dao)
├── session/           #   Session model + status enum
├── window/            #   Terminal focus (iTerm2 / Terminal.app / tmux)
├── plugins/
│   └── system_tray.rs #   System tray with status + preset menu
└── lib.rs             #   App entry + plugin registration

src/
├── pages/             #   Home / Settings / About
├── components/
│   ├── SessionCard.tsx #   Session card with status light
│   ├── SessionGrid.tsx #   Dashboard grid
│   ├── ExtensionList.tsx # Dual-view (byKind/byTool) resource management
│   ├── ResourceByKindView.tsx # Skills/MCP/Plugins three-section view
│   ├── ResourceByToolView.tsx # Four-tool card view
│   ├── ImportDialog.tsx  #   Native resource scan & import
│   ├── CompatibilityDialog.tsx # Preset compatibility check
│   ├── PresetList.tsx  #   Preset group CRUD
│   └── ui/            #   shadcn/ui primitives
├── hooks/             #   useSessions, useNotification, useUpdater
├── stores/            #   Zustand session store
├── lib/               #   Audio, shortcut, updater, window utils
├── i18n/              #   Chinese + English locales
└── types/             #   TypeScript type definitions
```

---

## Getting Started

### Prerequisites

- [Node.js](https://nodejs.org/) ≥ 18
- [pnpm](https://pnpm.io/) ≥ 8
- [Rust](https://www.rust-lang.org/tools/install) ≥ 1.77
- [Tauri v2 CLI](https://v2.tauri.app/start/prerequisites/)

### Install & Run

```bash
# Clone the repository
git clone https://github.com/jarvislee90s-dot/tuvis.git
cd tuvis

# Install frontend dependencies
pnpm install

# Start development mode
pnpm tauri:dev
```

### Build

```bash
# Build release binary (Windows NSIS installer)
pnpm tauri:build
```

### Lint & Format

```bash
pnpm check        # format:check + lint + build
pnpm format       # auto-format with Prettier
pnpm lint         # ESLint check
pnpm lint:fix     # ESLint auto-fix
```

---

## Configuration

The app stores its data in `~/.tuvis/`:

| Path                            | Purpose                                                        |
| ------------------------------- | -------------------------------------------------------------- |
| `~/.tuvis/tuvis.db`             | SQLite database (settings, extensions, presets, session cache) |
| `~/.tuvis/skills/`              | Global skill repository                                        |
| `~/.tuvis/mcp/`                 | Global MCP server configs                                      |
| `~/.tuvis/hooks/status-hook.sh` | Shared Hook script for status events                           |
| `~/.tuvis/events/`              | Hook event files (auto-cleaned, 30s TTL)                       |

### Supported Tool Configs

| Tool        | Skill Directory              | MCP Config                         | MCP Format                          | Hook Support                                             |
| ----------- | ---------------------------- | ---------------------------------- | ----------------------------------- | -------------------------------------------------------- |
| Claude Code | `~/.claude/skills/`          | `~/.claude.json`                   | JSON                                | ✅ (PascalCase)                                          |
| Codex CLI   | `~/.codex/skills/`           | `~/.codex/config.toml`             | TOML                                | ✅ (camelCase)                                           |
| OpenCode    | `~/.config/opencode/skills/` | `~/.config/opencode/opencode.json` | JSONC                               | ❌                                                       |
| OpenClaw    | `~/.openclaw/skills/`        | N/A                                | N/A                                 | ❌                                                       |
| Kimi Code   | `~/.kimi-code/skills/`       | `~/.kimi-code/mcp.json`            | JSON                                | ❌ (status parsed from wire)                             |
| WorkBuddy   | `~/.workbuddy/skills/`       | `~/.workbuddy/mcp.json`            | JSON                                | ❌ (status derived from heartbeat + JSONL)               |
| ZCode       | `~/.zcode/skills/`           | `~/.zcode/cli/config.json`         | JSON (nested `mcp.servers` subtree) | ❌ (status derived from SQLite message-stream tail)      |
| dsh         | `~/.dsh/skills/`             | N/A (probed unsupported)           | N/A                                 | ❌ (status derived from lock cross-check + event stream) |

> Note: `~/.agents/skills/` is the cross-tool shared directory of the Agent Skills open standard (read directly by codex / zcode and other compliant tools). Tuvis's skill activation directory for codex is the private `~/.codex/skills/`; `.agents` serves only as a read-only shared import source (source label `agents-shared`) — Tuvis scans it into the repository, with no tool attribution, no linking, and never writes to it (sole exception: one-time migration of Tuvis-created legacy links).

---

## Roadmap

- [x] US1 — Multi-tool session monitoring dashboard
- [x] US2 — Status change notifications & sound alerts
- [x] US3 — Quick terminal jump (iTerm2/Terminal.app/tmux)
- [x] US4 — Skill/MCP/Plugin unified repository management
- [x] US5 — Preset group one-click switching
- [x] US6 — Sub-agent level resource allocation
- [x] Resource dashboard redesign (dual-view + import + compatibility)
- [x] OpenClaw support (4th tool)
- [x] Kimi Code support (5th tool: session monitoring + MCP management + `KIMI_CODE_HOME` data directory redirection)
- [x] WorkBuddy support (6th tool: heartbeat-driven monitoring + deep-link jumps + resource management)
- [x] ZCode support (7th tool: SQLite session-aggregate monitoring + subagent-activity arbitration + workspace deep-link jumps + skill/MCP resource management)
- [x] dsh support (8th tool: web-host monitoring + zstd multi-frame log parsing + tri-color status + browser-tab jumps + read-only skill access)
- [x] Foxbell desktop pet (status cards + voice alerts + drag physics)
- [x] External pets (local/Petdex import + manage panel hot swap + capability gating)
- [x] Tool toggle management (batch save + restore/rebuild + full hiding)
- [x] APP-form session cards and deep-link jumps
- [x] Plugin management (file/config hybrid)
- [x] i18n (Chinese + English)
- [x] Auto-update via GitHub Releases (redone in v0.5.1: prerelease channel + ignore-this-version + title-bar upgrade badge)
- [x] Dark/light theme sync with system
- [x] Windows support (NSIS installer + deep links + nearest-ancestor window focus)
- [x] Remote control phase 2 (v0.5.0, experimental) — send messages / queue & jump the queue / remote approvals / Q&A / permission switching from your phone (Claude Code / Codex CLI / Kimi Code / OpenCode)
- [x] Mobile archived history (registry-based archiving + reactivation loop)
- [x] Remote access · External domain (no domain needed) fixed-address channel (one-click Tailscale onboarding + public-side reachability verification + automatic recovery on boot; access from this computer now requires the PIN)
- [x] External-domain wizard validated on Windows (v0.5.1) — custom Tailscale CLI path lookup fixed at the root + the login step hands you the authorization link (never logs in for you)
- [x] Token usage dashboard (phase 1 collection substrate & ledger + phase 2 settings section / pet mini-bar; see the "Token Usage Dashboard" section for the exact accounting rules)
- [x] Mobile sub-agent observatory (running-session chip + sub-agent board + execution detail)
- [x] Launch at login (Settings + tray entries; source of truth = live OS-side state, no local cache)
- [x] Board filtering by tool (desktop 7-tool chips + local memory, same semantics as mobile)
- [ ] Linux support
- [ ] Kitty & WezTerm terminal jump support

---

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

1. Fork the repository
2. Create your feature branch (`git checkout -b feature/amazing-feature`)
3. Commit your changes (`git commit -m 'feat: add amazing feature'`)
4. Push to the branch (`git push origin feature/amazing-feature`)
5. Open a Pull Request

Please read [AGENTS.md](AGENTS.md) for project architecture and development guidelines.

---

## License

This project is licensed under the MIT License — see the [LICENSE](LICENSE) file for details.

---

## Forking & Derivative Works

Forks and derivative projects are welcome. As required by the MIT License, please retain the original copyright notice and license text when redistributing. On top of that, you are welcome (but not obliged) to note "based on Tuvis" in your project description with a link back to this repository, so users can identify the upstream. See [TRADEMARK.md](TRADEMARK.md) for the boundaries on using the brand name and logo. Improvements are best contributed back upstream via PR, so your work evolves with the mainline.

---

## Trademarks & Non-Affiliation

Tuvis is an independent, open-source project. It is not affiliated with, endorsed by, or sponsored by Anthropic (Claude / Claude Code), OpenAI (Codex / ChatGPT), OpenCode, OpenClaw, Moonshot AI (Kimi Code), WorkBuddy, ZCode, dsh, or any other company or product mentioned in this repository. All product names, logos, and brands are the property of their respective owners; they are used here solely to describe compatibility (nominative fair use). Icons in this app are original designs; some color schemes are used only to help identify the corresponding tool and do not imply any official status.

## Official Channels

The only official distribution channel for this project is the [GitHub Releases](../../releases) page of this repository. Downloads offered anywhere else are third-party redistribution. See [TRADEMARK.md](TRADEMARK.md) for brand usage guidelines.
