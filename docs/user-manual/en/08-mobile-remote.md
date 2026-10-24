# Mobile Dashboard & Remote Injection

**Fresh-install defaults**: with no historical database (including legacy-brand data), Remote Access starts enabled, the release app attempts to register launch at login once, and remote-message device signatures are off. Enable individual channels as needed; upgrades keep their previous state. Turning a switch off is respected on later launches. Launch-at-login status reflects the operating system; a failed registration can be retried in Settings.

**Tailscale installer and manual installation**: on macOS and Windows, packages are saved in the system Downloads folder, including redirected Windows folders. The wizard follows a vertical timeline, with actions on the right and completed steps in green. The actual path and “Open downloads folder” appear only after download and verification; the pinned official “Download manually” link appears only if automatic download fails. If the installer cannot open, double-click the verified package from its downloads folder. On macOS, clicking “Install Tailscale” opens the system installer automatically; follow its instructions and authorization prompts, then return to Tuvis and click “Refresh status”. Cancelling does not mark installation complete.

**Both fixed-address wizards**: Own domain assumes your domain is already on Cloudflare and guides you through tunnel creation, Token entry and a confirmed connection, browser-side hostname binding, and board-response verification. External domain (Tailscale) follows its installation and enablement steps. Once required steps and public board verification pass, the timeline collapses and the address, copy/open actions, and QR code stay in the channel card; “View setup steps” reopens it. Later failures keep the summary with the previous address clearly marked unavailable; open and QR actions pause without restarting installation.

**Waiting for public DNS**: when the local board and Funnel are ready but public records are absent, the wizard shows elapsed time and an estimate of several minutes, potentially around ten minutes, while checking read-only about once per minute. No repeated retry is needed; overdue waits provide troubleshooting. Refresh and ordinary checks never reset Funnel; the explicit “Re-enable” repair retains configuration ownership safeguards. An enabled macOS extension needs no further action; a stopped backend first calls for checking the Tailscale client.

Step away from the desk and the dashboard comes with you — this chapter covers watching your eight-tool sessions in a phone browser and replying remotely.

## What remote access is

The mobile board is the desktop dashboard served as-is to your phone's browser: the same traffic-light cards for eight tools, session details, approvals, and Q&A. Same WiFi uses direct LAN connection; across networks use a tunnel — one-click guided setup for Tailscale (Funnel, permanent address) or a Cloudflare quick tunnel / your own domain.

The board is served directly by the desktop process (local-first): data never leaves your machine and nothing is hosted on a third-party platform — the tunnel only forwards the connection.

## Four channel cards

**Settings → Remote Access** offers **four channel cards, each with its own switch and usable simultaneously**:

| Channel card                          | When to use                                                   | Notes                                                                                                                                                                                                                                            |
| ------------------------------------- | ------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| 📶 LAN                                | Phone and computer on the same WiFi / cable (**recommended**) | Fastest; requires the access PIN; **your own browser on this computer uses this card's address too** (its switch must be on)                                                                                                                     |
| 🔀 Quick tunnel                       | You're away and your phone is on cellular                     | Public, no signup (Cloudflare); **address changes every time it's enabled**                                                                                                                                                                      |
| 🌐 Own domain                         | You want a stable entry point                                 | Needs a Cloudflare account (free plan works) + a Tunnel Token; the address is **permanent** once configured — Tuvis downloads and supervises cloudflared for you, with a step-by-step guide in Settings                                           |
| 🛰 External domain (no domain needed) | You don't have a domain but still want a permanent address    | **No domain required**: the card's wizard walks you through installing Tailscale and enabling Funnel to get a **permanent** address like `https://<machine>.<tailnet>.ts.net/m`; you only install and sign in to Tailscale once on this computer |

**The access PIN applies to this computer too**: loopback access is **no longer PIN-free** — opening the board from this machine's own browser is treated exactly like a remote device (enter the PIN once, then remembered for 180 days). "This machine" is **no longer a channel card**: it is just one way to connect, using the address shown on the "LAN" card.

The PIN applies to **all four channels** (**including access from this computer**): enter it once per device for **180 days**, and after a PIN change every device must re-enter it.

## Connecting a channel

How to connect: confirm "Enable Remote Access" is on → enable the channel you want → click its card to reveal the address / QR code (**the link already contains the PIN, so scanning fills it in automatically**) → open it on your phone (**the URL must end with `/m`**). **The first run of "External domain (no domain needed)" goes through the card's wizard**: click "One-click setup" and it walks through detect → download from the official source with checksum verification → install (the system installer opens on macOS; an MSI wizard opens on Windows) → sign in to Tailscale (Tuvis turns the authorization link into a button; you finish in the browser) → turn off "block incoming connections" → enable Funnel → **reachability verification** — no command line at any point; on macOS you also approve Tailscale's system extension once in System Settings, and the first Funnel enablement may additionally need one browser approval (or none at all).

**The address is only shown after it passes a real check from outside the tailnet**: while verifying or not yet effective, the card honestly reports "taking effect" and gives the expected wait for the situation (several minutes, potentially around ten minutes, for public DNS; about 1–2 minutes when recovering after boot) — it never puts a dead link in front of you. Once verified it is re-checked periodically (about once a minute, less often when idle) and continues read-only checks on failure; an explicit repair requires your confirmation. The address never changes, paired devices never need re-pairing, and **Tuvis restores the channel automatically on every boot** (it waits for the Tailscale backend to be ready before re-enabling, so it never wipes a config that is still coming back).

> **Stated honestly**: the platform notes inside the wizard match the implementation — on Windows only the second half has been tested on a real machine (detect, enable, revoke and restart recovery after "installed / signed in"), while the **install and sign-in flow has not been run end to end**; on macOS the **write paths** (enable / revoke / self-heal) are **not yet verified on a real machine** either (read-only probing has been). The wizard says so on the spot per platform and does not pretend the whole flow has been verified.

## Enabling remote access

1. Confirm Remote Access is enabled in the tray right-click menu, or open **Settings → Remote Access**;
2. Inside **Settings → Remote Access**, enable channels as needed (LAN / quick tunnel / own domain / external domain (no domain needed) — independent switches, can run together);
3. An address is shown only after it has been verified reachable from the public side; enabled channels auto-restore after a reboot;
4. The tray menu shows the address directly, like `http://192.168.x.x:9420/m`.

![Settings → Remote Access (channels / access password / devices)](../img/remote-settings.png)

## The remote settings page: six-section order, PIN masking, and tool configuration

The remote block under **Settings → Remote Access** is laid out in a fixed six-section order that survives upgrades:

1. **① General** — the existing items such as the remote master switch;
2. **② Channels** — the four channel cards (independent switches, can run together);
3. **③ Access & security** — the 4-digit access PIN, device reset, and the paired-device list (the device list always lives in this section);
4. **④ Tool-specific configuration** — collapsible per-tool cards; the first release ships a single ZCode card (see below);
5. **⑤ Headless injection** — the headless switch, timeout and concurrency caps, and compatibility send (experimental);
6. **Injection audit** — always last in this block.

### Access PIN: masked by default + eye button

The 4-digit PIN box under **③ Access & security** **starts masked**; tap the eye button next to the box to reveal it, tap again to mask it. Leaving the block and coming back, switching settings blocks, or the window losing focus all restore the mask automatically — well suited to screenshots and screen sharing. The eye changes **display only**: the PIN value, digit filtering, random generation, saving, and device pairing are all unaffected.

### Headless injection and compatibility send: on by default (upgrade note)

The **headless injection** and **compatibility send (experimental)** switches under **⑤ Headless injection** now **default to on**:

| What you did before                                   | State after upgrading / on a fresh install    |
| ----------------------------------------------------- | --------------------------------------------- |
| Never touched either switch (no choice stored)        | **On** (the new default)                      |
| Explicitly turned a switch **off**                    | **Stays off** — Tuvis never overrides your explicit choice |
| The stored value is invalid (e.g. empty)              | Treated as **off** — never guessed into "on"  |

The new default only changes what counts when no choice of yours is stored: there is no table-wide migration and no fabricated toggle audit; saving still writes an explicit on/off. Defaulting to on loosens nothing — compatibility send still applies **only to Codex desktop builds whose write capability is unverified**, and the existing guards (read-chain verification, build / device / pending-queue) are all preserved; you can turn either switch off explicitly at any time.

### ZCode official remote link (tool-specific configuration)

Section **④ Tool-specific configuration** ships a single **ZCode remote link** card, collapsed by default; tap the card header to expand / collapse (no separate settings page):

- **What it is for**: storing the official remote-control link produced by the ZCode app's "mobile remote control" on this computer; paired phones read the same value;
- **How to get the link**: open the ZCode app on this computer → open mobile remote control → turn it on → copy the link shown on screen (or scan the on-screen QR code with your phone) → come back here, paste it into the box and save. Tuvis **never signs you in** and never opens or closes your ZCode; "Open the official page" is an explicit action for checking the link yourself — opening it does not change the card's status;
- **Saved ≠ connected**: the card's states are only "Not configured / Saved · connection pending / Link needs updating" — "Saved" only means the link passed format checks and is stored; the official-side connection and identity confirmation are **still pending**. A saved link **never displays as "connected"**. A malformed link (not https, not the official host, broken link, …) is rejected in place with the reason, and an already-stored valid value is untouched;
- **On the phone**: the ZCode session's board, messages and file previews keep working regardless of the link state; the composer explains the state in place ("Configure the ZCode remote link on your computer", "Connection pending — sending from here is not available yet (your draft is kept)", …) and never fakes a send;
- The link is stored on this machine only and the full link never enters logs; deleting clears only this machine's copy, and the board and message / file reading keep working.

> **Stated honestly (capability boundary)**: what this round ships for ZCode is **reading and configuration** — the board, messages, files, and the remote-link configuration all work; **unified control (sending / approvals / Q&A / interrupts / model switching from the phone) still awaits official authorization and the bridge (pending)**. It stays unavailable until then, and no fake "connected" state will ever be shown.

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
- **ZCode session discovery & reading**: newer and older ZCode builds write different session ID shapes; both are accepted and shown as-is (IDs are never rewritten), so no sessions are missed. File reference numbers in message bodies are aligned with the final rendered message order, so "jump to latest reference" no longer lands off-target (file-only messages keep their files and honestly carry no message sequence number).
- **Stop the desktop turn** (Codex APP): while a desktop turn keeps running, the outgoing-queue panel offers "Stop the desktop turn & continue" — it interrupts the turn currently running in desktop Codex, and **work that turn has not finished is lost** (a confirmation says so first); once the turn stops, the messages waiting in the queue go out on their own. Available from macOS `26.1013.40934`; other Desktop builds are not verified yet and do not offer it.
- **Approvals**: tool-call approvals (allow / deny) and plan confirmations (body synced, number shortcuts);
- **Q&A**: single / multiple choice, free text (overwrite write-in / clear), multi-question flow and question switching;
- **Mode & permission tiers**: pick mode and permission chips directly (kimi uses the two-step, screen-read-confirmed flow);
- **Send messages & create sessions** (experimental, with queueing / jump-the-queue / raising sessions): currently covers Claude Code / Codex CLI / Kimi Code / OpenCode; the rest say so honestly.

Push uses **SSE as the primary channel** — a tap on the notification takes you straight to that session, and transition deduplication uses the same server-side logic as desktop notifications.

## Session detail page

The session detail page (ZCode-style conversation view, unified across all eight tools):

- Expand details while running (thinking / tool calls are collapsible), auto-collapse to the final summary once a turn completes
- Markdown rendering (headings / lists / tables / code highlighting); project file paths in the text become clickable links
- **File panel**: aggregates the files a session touched, newest first, with document/image filters and a **200 / 500 / 1000 message** look-back range; **modified** files are highlighted and **read-only** ones are neutral (both previewable), and tapping the secondary line reveals the full path. The four sources — including "**Session artifacts**" — are described under "What your phone can do" above
- **File preview**: markdown / syntax highlighting / inline images; three switchable layouts — side-by-side (chat left, file right), stacked, and fullscreen overlay — with a draggable splitter; readable scope is the session project directory plus your home directory (sensitive directories such as keys are always refused)
- **Transfer progress and honest bandwidth disclosure**: uploads and downloads both show a **progress bar** (bytes transferred / percentage / instantaneous rate / ETA); **when the rate or the total size is unknown it shows bytes transferred only and never invents a percentage**. On a bandwidth-limited tunnel channel the file panel and the attachment area say **in place** that this is a channel limit, not a malfunction, with time estimates based on this channel's measured rate (about 1.5 minutes to download and 3.3 minutes to upload a 20 MB attachment) and a **full-speed upgrade path** (install Tailscale on your phone and join the tailnet to connect directly, 1–2 orders of magnitude faster)
- **Message bookmarks**: mark a spot to revisit with a colored dot (up to 10, one per color), jump back by tapping it, delete individually or clear all; stored in the browser — refresh-proof, cleared when Tuvis restarts or the tab is closed
- **Font size**: 50% / 75% / 100% / 125% for message and file text only
- **Archived history**: finished or unreachable sessions stop cluttering the board — registry-based archiving (history page lazy-loads 1/3/7 days), tap a card for a read-only detail view with one-tap reactivation; board cards can be closed/archived (CLI sessions stop their process, APP sessions get a true "close")

## Remote control · send messages & approve from your phone (v0.5.0, experimental)

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
| ![Session detail](../../images/mobile-detail-v0.5.0.png) |  ![Send](../../images/mobile-send-v0.5.0.png)   |
|                  **Approvals · plan**                   |             **Q&A · multi-select**             |
|   ![Approvals](../../images/mobile-approve-v0.5.0.png)   | ![Q&A](../../images/mobile-question-v0.5.0.png) |

**Mobile remote control status (v0.5.0, experimental)**: message viewing and file preview cover 7 of the 8 tools (OpenClaw's black-box build is not remotely readable yet); **sending messages to CLI sessions and creating sessions from your phone are experimental**, currently supporting Claude Code / Codex CLI / OpenCode / Kimi Code. Messages are injected as keystrokes into the terminal and verified by screen-reading receipts (no false "delivered"); queueing, interrupt-and-jump, remote approvals, question answering, permission-mode switching, and session creation are included. Illustrated walkthroughs live in the Chinese manual; per-tool statuses and limitations are listed below.

**Codex APP remote control status (v0.6.0, experimental)**: real-phone PRODUCT acceptance is not fully executed yet — see `docs/superpowers/reviews/2026-10-20-codex-app-product-acceptance.md` (builds, capability scope and preview behavior are as described above).

**Three-state capability badge for Codex APP remote control (mobile)**: capabilities are shown as three honest states instead of claiming theoretical support — **verified** (that Desktop build has real-machine evidence: controllable / sendable), **compatibility send (experimental)** (unknown build: send/status default to on per the 2026-10-22 ruling, while destructive capabilities — interrupt, approvals — stay evidence-gated: Tuvis first runs a **read-only probe** on that build (15-second hard cap server-side) and takes schema evidence from the bundled CLI, unlocking them only after evidence; while a probe is in flight the UI shows only a "probing" skeleton and **does not prematurely declare the build locked**), **locked** (the server states it is unavailable).

**Evidence level of Codex APP approvals (reported as-is)**: **command approvals** have real-machine evidence (accept/decline closure on `26.1013.4894.0` + the pending shape on `26.1016.6573.0`); **file approvals** currently have **official method-table / call-site** evidence only — **not yet closed end-to-end on a real machine**.

### The four injectable terminals — current status and limitations (reported honestly)

- **Claude Code**: busy sessions queue messages without loss; "send now" = Esc interrupt + taking the whole queue out at once to open a new turn; retract-window protection (delivery is refused and reported honestly when the input line still has residue); **plan approval card** (approving the waiting red light + digit-key direct selection + a plan feedback bar after approval); multi-question Q&A with ←/→ two-way question switching + multi-select free-form answers (once written, they can be **edited / overwritten / cleared**); the multi-select question panel is no longer misread as an approval card (checkbox guard + approval keys disabled)
- **Codex CLI**: Tab enters the native queue (without interrupting the current turn), Esc jumps the queue with an interrupt, Ctrl+C retracts queued messages; Q&A supports a note field (Tab expands/clears it, and **the terminal's note line is screen-read back into the input box**) + multi-select free-form answers; **remote mode/permission switching** — mode Plan↔Operate chips selected directly (current tier highlighted) and four permission tiers selected directly (including a Full Access double confirmation), with the switch result verified by terminal screen reading. Limitation: the multi-question page-turn keys are not field-tested yet (multi-question navigation guides the terminal to do it)
- **Kimi Code**: busy sessions auto-send the queued message when the turn ends; three permission tiers switched via the **two-step** flow (open the menu, screen-read the highlighted row to confirm) + a bottom-bar mode/permission dual read-back; the **full 2.1.1 questionnaire chain** — Other free-form answers (edit / overwrite / clear), multi-select checkboxes, single-choice auto-advance + landing verification (after all n questions are answered it always lands on n+1), ←/→ question switching, and confirmation-card summaries taken from the terminal's Review page via screen reading. Limitations: approval digit selection is disabled (it conflicts with the menu key, so navigation is used instead); the `Ctrl+S` instant handoff is not productized yet
- **OpenCode**: Q&A with Enter / digit-key selection + single/multi-select **free-form answers** (overwrite / clear; single-choice delivery self-heals), ◀/▶ two-way question switching, Esc interrupt to jump the queue, and 2.x dual-schema session discovery. Limitation: Ctrl+C is blacklisted globally (it would quit the opencode app outright)

## Cards trust the terminal's screen-read

Remote injection is best-effort and drift is inevitable: **the card's source of truth is the terminal screen-read** — every injection receipt carries a post-action screen-read snapshot for verification, and mismatches fall back to terminal truth automatically; refreshing / reopening the page re-syncs from the snapshot; when the screen-read is unavailable (e.g. the process exited) the snapshot is empty and the card conservatively keeps the current state.

> 🔒 The address + access password is the key — do not share it; too many wrong entries locks access for about 10 minutes.

> 💡 Nine out of ten connection problems are one of these: router "AP isolation", an address missing the `/m` suffix, or a firewall blocking inbound — see the troubleshooting chapter.
