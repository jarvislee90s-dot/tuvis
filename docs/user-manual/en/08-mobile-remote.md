# Mobile Dashboard & Remote Injection

**Fresh-install defaults**: with no historical database (including legacy-brand data), Remote Access starts enabled, the release app attempts to register launch at login once, and remote-message device signatures are off. Enable individual channels as needed; upgrades keep their previous state. Turning a switch off is respected on later launches. Launch-at-login status reflects the operating system; a failed registration can be retried in Settings.

**Tailscale installer and manual installation**: on macOS and Windows, packages are saved in the system Downloads folder, including redirected Windows folders. The wizard follows a vertical timeline, with actions on the right and completed steps in green. The actual path and “Open downloads folder” appear only after download and verification; the pinned official “Download manually” link appears only if automatic download fails. If the installer cannot open, double-click the verified package from its downloads folder. On macOS, clicking “Install Tailscale” opens the system installer automatically; follow its instructions and authorization prompts, then return to Tuvis and click “Refresh status”. Cancelling does not mark installation complete.

**Both fixed-address wizards**: Own domain assumes your domain is already on Cloudflare and guides you through tunnel creation, Token entry and a confirmed connection, browser-side hostname binding, and board-response verification. External domain (Tailscale) follows its installation and enablement steps. Once required steps and public board verification pass, the timeline collapses and the address, copy/open actions, and QR code stay in the channel card; “View setup steps” reopens it. Later failures keep the summary with the previous address clearly marked unavailable; open and QR actions pause without restarting installation.

**Waiting for public DNS**: when the local board and Funnel are ready but public records are absent, the wizard shows elapsed time and an estimate of several minutes, potentially around ten minutes, while checking read-only about once per minute. No repeated retry is needed; overdue waits provide troubleshooting. Refresh and ordinary checks never reset Funnel; the explicit “Re-enable” repair retains configuration ownership safeguards. An enabled macOS extension needs no further action; a stopped backend first calls for checking the Tailscale client.

Step away from the desk and the dashboard comes with you — this chapter covers watching your eight-tool sessions in a phone browser and replying remotely.

## What remote access is

The mobile board is the desktop dashboard served as-is to your phone's browser: the same traffic-light cards for eight tools, session details, approvals, and Q&A. Same WiFi uses direct LAN connection; across networks use a tunnel — one-click guided setup for Tailscale (Funnel, permanent address) or a Cloudflare quick tunnel / your own domain.

The board is served directly by the desktop process (local-first): data never leaves your machine and nothing is hosted on a third-party platform — the tunnel only forwards the connection.

## Enabling remote access

1. Confirm Remote Access is enabled in the tray right-click menu, or open **Settings → Remote Access**;
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
- **Codex APP message and file links**: recognized file paths in message bodies open the preview; uploaded/referenced images, generated images, and files changed or viewed also appear as structured artifacts with accurate source labels. A file entry can jump to its latest message reference by searching the current tail and at most four earlier pages (up to 50 messages per page); it does not keep loading beyond that range. If a path is overwritten, preview reads the current file on disk and does not retain historical versions.
- **Approvals**: tool-call approvals (allow / deny) and plan confirmations (body synced, number shortcuts);
- **Q&A**: single / multiple choice, free text (overwrite write-in / clear), multi-question flow and question switching;
- **Mode & permission tiers**: pick mode and permission chips directly (kimi uses the two-step, screen-read-confirmed flow);
- **Send messages & create sessions** (experimental, with queueing / jump-the-queue / raising sessions): currently covers Claude Code / Codex CLI / Kimi Code / OpenCode; the rest say so honestly.

## Cards trust the terminal's screen-read

Remote injection is best-effort and drift is inevitable: **the card's source of truth is the terminal screen-read** — every injection receipt carries a post-action screen-read snapshot for verification, and mismatches fall back to terminal truth automatically; refreshing / reopening the page re-syncs from the snapshot; when the screen-read is unavailable (e.g. the process exited) the snapshot is empty and the card conservatively keeps the current state.

> 🔒 The address + access password is the key — do not share it; too many wrong entries locks access for about 10 minutes.

> 💡 Nine out of ten connection problems are one of these: router "AP isolation", an address missing the `/m` suffix, or a firewall blocking inbound — see the troubleshooting chapter.
