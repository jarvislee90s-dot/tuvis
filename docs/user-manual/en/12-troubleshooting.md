# Troubleshooting

A quick "symptom → possible cause → fix" reference. Entries are grouped by feature area, matching the chapter order of this manual (board & sessions → notifications & health → remote access → installation & data) — find your section first, then act.

> Since v0.5.8 every in-app error carries a stable **error code** with localized (EN/中文) wording — include the code when reporting an issue for precise diagnosis.

## Board & sessions

| Symptom | Possible cause | Fix |
| -------------------------------------- | ------------------------------------------------------------ | ------------------------------------------------------------------------------------ |
| A tool never shows any card on the board | The tool is unchecked, or no session of it is actually running | Confirm the tool is checked under "Settings → Tools & Health Check"; confirm a session is really open in that tool's terminal |
| A card's status light is stuck and disagrees with the terminal | The Hook channel is not active, or the tool has no Hook support and state is only estimated | Complete the Hook trust and watch again; perception lag on no-Hook tools is normal; restart Tuvis and check once more |

## Notifications & health

| Symptom | Possible cause | Fix |
| ------------------------------ | -------------------------------------- | ------------------------------------------------------------------ |
| No desktop notifications | The in-app notification switch is off, or the OS notification permission is denied | Turn on notifications in settings; check the OS notification center allows Tuvis |
| "Signal Health" reports zero events for a tool | The tool's Hook entry has not been trusted yet | Enter `/hooks` in that tool's terminal and trust the Tuvis entry (codex must be confirmed inside its TUI); refresh the self-check in settings afterwards |

### Signal Health (Hook trust-gate self-check)

A settings section under the "Tools & Health Check" block to verify each tool's Hook channel is actually working:

- Every Hook-capable tool shows registration status, last event time, and pending prompts (data comes from the existing Hook event directory and session-scan snapshots, fetched on open/manual refresh — no background polling)
- Criterion: registered + an active session exists for the tool + zero events within a 30-second window → shows "enter /hooks in the tool's terminal and trust the Tuvis entry (one-time)" with a one-click copy of the `/hooks` command
- Unregistered tools get an honest reason (e.g. kimi on macOS shows "the Hook channel is not distributed on this platform; nothing to do") instead of a bare "unregistered"
- codex has an extra trust gate: Tuvis being registered ≠ events firing — you must run `/hooks` inside the Codex TUI and trust it once (the trust hash lands in your user-level config). After registration Tuvis raises a **persistent in-app action card** instead of a one-shot desktop notification: raising the card ≠ dismissing it; only "Copy command" or "Don't remind me again" clears it, and the card carries the command text as a clipboard fallback

## Remote access

| Symptom | Possible cause | Fix |
| -------------------------------------------- | ------------------------------------------------------------ | ---------------------------------------------------------------------------------------- |
| Phone cannot connect over LAN | Router "client / AP isolation / guest network"; firewall blocks inbound; address missing the `/m` suffix | Turn off isolation settings in the router admin; on Windows allow Tuvis through the firewall (check both private + public); make sure the address ends in `/m` — scanning the QR code is easiest |
| An old link stopped working on the tunnel | The quick tunnel's public address is regenerated every time it starts | Re-enable the quick tunnel and scan the new code; for a fixed address switch to the "own domain" or "external domain (no domain needed)" channel |
| The phone page asks for an access password | The access password applies to all channels (including local browser access) | Enter the access password (once per device, then 180 days password-free); when pairing by QR code the password is already inside the link |
| A TLS confirmation dialog appears when opening the LAN address | LAN direct connection is plain HTTP | Checking the box means "I understand / I run a TLS reverse proxy"; on a home LAN just check it — no real proxy needed |
| Own-domain connector started, but the domain is unavailable | No registered connection yet, or a missing/misconfigured published application route | Confirm the real connection first; bind the hostname under Routes → Add route → Published application using the wizard’s current local service address, then verify the board response |
| Tailscale waits for public DNS | Public records are not published yet or a negative cache has not expired | Keep it enabled and allow several minutes, potentially around ten. Read-only checks run every minute; avoid repeated reopening. Use overdue troubleshooting; only explicitly confirmed re-enable resets configuration |
| A fixed address shows "not yet effective: address resolves but won't connect" | Transient jitter on the public path to Tailscale's overseas relays (two entries share a facility and often fail together); TUN / system-proxy accelerators on this machine can also interfere | Most recover within 1–2 minutes — Tuvis re-verifies every minute and the address returns on its own; turn off accelerator tools first and wait; only after 10+ minutes use "retry" in the wizard |
| A ZCode session is visible on the phone but cannot be sent to (reading works) | Unified control for ZCode still awaits official authorization (pending); the composer explains the link-state reason in place | Keep using messages and files; you can already store the official remote link under Settings → Remote Access → ④ Tool-specific configuration following the tutorial, ready for when control lands — saved ≠ connected, and the card never shows "connected" |
| The phone says "Configure / update the ZCode remote link on your computer" | No remote link stored on the computer yet, or the stored link no longer matches the expected format | Copy the remote link again in the ZCode app on this computer (or scan the QR code), then paste it into the ZCode card under ④ Tool-specific configuration and save; errors are explained in place and a stored valid value is untouched |
| The headless injection / compatibility send switches look different after an upgrade | Both switches now default to on: with no choice of yours stored they come up on; an explicit off stays off; invalid values are always treated as off | Turn them explicitly on/off under ⑤ Headless injection as you like — Tuvis remembers only your explicit choices; no bulk migration happens |

### Can't connect? Check these first

Every item below is a pit we actually hit:

1. **The router has "client isolation / AP isolation / guest network" enabled** — the most elusive one: your phone and computer are on the same WiFi, yet the router forbids wireless clients from talking to each other, so nothing connects. Turn that setting off in the router's admin page (field note: everything else checked out; this turned out to be it).
2. **The quick tunnel address changes every time it is toggled** — each time you enable the quick tunnel a new public address is generated (the old link / QR code dies immediately), so don't treat it as a long-term entry point; for a fixed address use "Own domain" or "External domain (no domain needed)". Toggling other settings does not affect this channel.
3. **The phone URL must end with `/m`** — a full address looks like `http://192.168.x.x:9420/m`; scanning the QR code is recommended (**the link already contains the PIN, filled in automatically**), and typing it without `/m` gives you an empty 404 page.
4. **Access from this computer needs the PIN too** — "This machine" is no longer a channel card: opening the board in this machine's own browser is treated exactly like a remote device (enter the PIN once), using the address shown on the "LAN" card, and **that switch must be on** (the address is unreachable when the server only listens on loopback); on your phone use the "LAN" card or a tunnel address.
5. **Firewall** — on Windows the first listen triggers an "allow access" prompt; if it was dismissed, inbound traffic is blocked: Windows Security → Firewall & network protection → Allow an app through firewall → tick Tuvis (both Private and Public).
6. **The TLS confirmation dialog** — a direct LAN connection is plain HTTP; ticking the box means "I understand / I have a TLS reverse proxy in front"; for a home LAN just tick it — you don't need to actually set up a reverse proxy.
7. **Own domain is connected but the domain won't open** — follow "Bind public hostname" in the channel timeline: in Cloudflare, go to Routes → Add route → Published application, configure the subdomain, choose HTTP for Type, and use the current origin address shown by the wizard for Service. Save the route and return to Tuvis to verify; setup completes only after the public board response passes verification.
8. **The “External domain (no domain needed)” card keeps waiting for public DNS** — publishing records or refreshing a negative cache can take several minutes, potentially around ten. Keep the channel enabled; read-only checks run every minute. Retry is hidden during this wait, and overdue waits provide troubleshooting. Ordinary checks never reset Funnel; only an explicitly confirmed re-enable performs repair. An enabled extension needs no further approval; recovery after boot normally means the Tailscale backend is reconnecting.

## Installation & data

| Symptom | Possible cause | Fix |
| -------------------------------- | ------------------------------------------------------------ | ------------------------------------------------------------------------ |
| In-app update check fails | GitHub Releases is hard to reach; or the app is still on 0.5.2 or older (its update channel has expired) | Download the latest installer from GitHub Releases manually and install over the top |
| Autostart still happens after uninstalling | Leftover autostart registry entry | Disable or delete the Tuvis entry in Task Manager → "Startup apps"; or delete the Tuvis value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` |
| Want to back up or migrate data | All Tuvis data lives under `~/.tuvis/` | Copy that directory as a whole (database, settings, logs, skill repo); since 0.5.5 the data directory moved once from `~/.mam` → `~/.tuvis` — on failure the old data stays in place and the move is logged |

> 💡 When nothing above matches, compare against the logs: Tuvis writes them to `~/.tuvis/logs/mam.log` (auto-rolled to `mam.log.old` when full — check both). Attach the log, the tools involved, and their versions when filing an issue; since v0.5.8 the error **code** shown in the UI pinpoints the failure precisely.
