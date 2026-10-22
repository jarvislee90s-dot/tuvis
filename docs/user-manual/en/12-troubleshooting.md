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

## Installation & data

| Symptom | Possible cause | Fix |
| -------------------------------- | ------------------------------------------------------------ | ------------------------------------------------------------------------ |
| In-app update check fails | GitHub Releases is hard to reach; or the app is still on 0.5.2 or older (its update channel has expired) | Download the latest installer from GitHub Releases manually and install over the top |
| Autostart still happens after uninstalling | Leftover autostart registry entry | Disable or delete the Tuvis entry in Task Manager → "Startup apps"; or delete the Tuvis value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` |
| Want to back up or migrate data | All Tuvis data lives under `~/.tuvis/` | Copy that directory as a whole (database, settings, logs, skill repo); since 0.5.5 the data directory moved once from `~/.mam` → `~/.tuvis` — on failure the old data stays in place and the move is logged |

> 💡 When nothing above matches, compare against the logs: Tuvis writes them to `~/.tuvis/logs/mam.log` (auto-rolled to `mam.log.old` when full — check both). Attach the log, the tools involved, and their versions when filing an issue; since v0.5.8 the error **code** shown in the UI pinpoints the failure precisely.
