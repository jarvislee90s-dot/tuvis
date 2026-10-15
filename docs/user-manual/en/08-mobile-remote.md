# Mobile Dashboard & Remote Injection

Step away from the desk and the dashboard comes with you — this chapter covers watching your eight-tool sessions in a phone browser and replying remotely.

## What remote access is

The mobile board is the desktop dashboard served as-is to your phone's browser: the same traffic-light cards for eight tools, session details, approvals, and Q&A. Same WiFi uses direct LAN connection; across networks use a tunnel — one-click guided setup for Tailscale (Funnel, permanent address) or a Cloudflare quick tunnel / your own domain.

The board is served directly by the desktop process (local-first): data never leaves your machine and nothing is hosted on a third-party platform — the tunnel only forwards the connection.

## Enabling remote access

1. Check "Remote access" in the tray right-click menu, or open **Settings → Remote Access**;
2. Inside **Settings → Remote Access**, enable channels as needed (LAN / quick tunnel / own domain / external domain (no domain needed) — independent switches, can run together);
3. An address is shown only after it has been verified reachable from the public side; enabled channels auto-restore after a reboot;
4. The tray menu shows the address directly, like `http://192.168.x.x:9420/m`.

![Settings → Remote Access (channels / access password / devices)](../img/remote-settings.png)

## Opening the board on your phone

1. Open the given address in your phone's browser (**it must end in `/m`**); scanning the QR code is recommended — the link already contains the access password and scanning fills it in;
2. Enter the access password on first visit, then password-free for 180 days; no app install needed;
3. First-time pairing, password changes, and device management (rename / kick, up to 10 devices) all live in **Settings → Remote Access**.

![Mobile board (interactive demo)](../img/mobile-board.png)

The screenshot above shows the site's interactive demo.

## What your phone can do

- **See the cards**: traffic-light session cards for all eight tools; state changes push in about 2 seconds (banner + sound + vibration), degrading to 3-second polling if the stream drops;
- **See the files**: the file panel aggregates session files with preview (markdown / code / images) and split-screen layouts; four sources — beyond "uploaded by me", "tool reads", and "tool reads & writes", there is "**Session artifacts**": generated files that appeared in the conversation as a link or backtick path and still exist on disk (reports, charts, data produced by scripts/commands; only previewable types like md / images / html / json make the list). Files the tool actually rewrote still show as "tool reads & writes". Artifacts with relative paths need the session working directory — **currently Codex sessions only** (sessions inside the Codex app and the other seven tools list absolute-path artifacts only); paths inside triple-backtick code blocks do not count as artifacts (`~~~` fences and indented blocks are not recognized yet);
- **Approvals**: tool-call approvals (allow / deny) and plan confirmations (body synced, number shortcuts);
- **Q&A**: single / multiple choice, free text (overwrite write-in / clear), multi-question flow and question switching;
- **Mode & permission tiers**: pick mode and permission chips directly (kimi uses the two-step, screen-read-confirmed flow);
- **Send messages & create sessions** (experimental, with queueing / jump-the-queue / raising sessions): currently covers Claude Code / Codex CLI / Kimi Code / OpenCode; the rest say so honestly.

## Cards trust the terminal's screen-read

Remote injection is best-effort and drift is inevitable: **the card's source of truth is the terminal screen-read** — every injection receipt carries a post-action screen-read snapshot for verification, and mismatches fall back to terminal truth automatically; refreshing / reopening the page re-syncs from the snapshot; when the screen-read is unavailable (e.g. the process exited) the snapshot is empty and the card conservatively keeps the current state.

> 🔒 The address + access password is the key — do not share it; too many wrong entries locks access for about 10 minutes.

> 💡 Nine out of ten connection problems are one of these: router "AP isolation", an address missing the `/m` suffix, or a firewall blocking inbound — see the troubleshooting chapter.
