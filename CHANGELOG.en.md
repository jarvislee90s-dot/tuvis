# Changelog

English · [中文](CHANGELOG.md)

<!-- 本文件是 CHANGELOG.md 的英文版（2026-10-17 拆分，用户裁决）。契约：
     ① 两份文件各自单语——本文件纯英文，CHANGELOG.md 纯中文，互不混排；
     ② 版本标题 `## [x.y.z] - YYYY-MM-DD`、分区顺序与条目顺序同中文版逐条对齐；
        分区名用英文（`Engineering` 对应中文版「工程」）；
     ③ 未译条目不留占位——直接不写（网站英文页按「仅列已译版本」展示）；
     ④ 新增版本时两份文件同批更新（发版文档纪律见 docs/RELEASE.md）；
     ⑤ 网站（tuvis-website）构建时按双源同步：本文件 → 英文站，CHANGELOG.md → 中文站。 -->

_Only versions translated so far are listed here; the full history lives in the
[Chinese changelog](CHANGELOG.md)._

## [Unreleased]

### Added

- **ZCode official remote link configuration with per-state phone hints**: the remote
  settings gained a "④ Tool-specific configuration" section whose first release is a
  single collapsed ZCode card — it stores the official remote-control link produced by
  the ZCode app's "mobile remote control" on this machine (strict format checks: only
  https on the official host with a versioned /remote/vN path and a non-empty sid;
  errors are shown in place, malformed values are never distributed, and the full link
  never enters logs); paired phones read the same value through a read-only endpoint.
  The card's "Saved · connection pending" state only means the link passed format
  checks — **saved ≠ connected**. On the phone, a ZCode session's board, messages and
  files keep working; the composer explains the link state in place (not configured /
  needs updating / connection pending), keeps the draft, and never fakes a send.
- **ZCode session read recovery**: main session IDs written by newer ZCode builds
  (`session_` + 32 hex chars) used to be rejected wholesale by the read gate; they are
  now accepted alongside the legacy shape (`sess_` + UUID) — original IDs are preserved
  verbatim, never rewritten or merged; all other read gates (interactive, 24h window,
  caps) are unchanged. ZCode attachment reference numbers are now aligned with the
  final rendered message sequence (numbered after multi-part expansion and tail
  trimming), so "jump to latest reference" no longer lands off-target; file-only
  messages keep their files and honestly carry no message sequence number.

### Changed

- **Headless injection and compatibility send default to on (upgrade note)**:
  `remote.headless_enabled` and `remote.compat_send_enabled` now resolve to **on** when
  the settings hold no choice of yours (key absent — fresh installs or upgrades without
  a stored value); users who explicitly turned a switch **off keep it off**, and invalid
  values (e.g. empty) still resolve to off — never guessed into on. No table-wide
  migration and no fabricated toggle audit; the existing timeout (600000 ms), concurrency
  cap (2) and the compatibility-send guards (read-chain verification / build / device /
  pending queue) are unchanged.
- **Remote settings page rearranged into a fixed six-section order**: ① General →
  ② Channels → ③ Access & security (including the device list) → ④ Tool-specific
  configuration → ⑤ Headless injection (with compatibility send) → Injection audit;
  the headless and compatibility-send controls were extracted into their own sub-section,
  and the compatibility-send probe hint now only appears while the switch is off.
- **Access PIN masked by default**: the 4-digit PIN box in settings starts masked, with
  an eye button toggling reveal / mask; leaving the block, switching settings blocks, or
  the window losing focus restores the mask automatically — display only; the PIN value,
  storage and pairing are unaffected.

### Fixed

- **Default README switched to English**: `README.md` is now the English version
  (content moved in from `README.en.md`); the Chinese source-of-truth moved to
  `README.zh-CN.md`, with language-switcher links at the top of both files. The
  `AGENTS.md` bilingual asset table, project tree, and the README pin in the
  `usage-partial-coverage-lock` test were updated accordingly.
- **README facade rewrite and dual community channels**: both READMEs were rebuilt as
  facade pages (hero trio, feature table linking into the manual, a 2×3 screenshot
  gallery, and a community section), with the former release-level details migrated
  into 8 chapters of the 13-chapter user manual (16 files across zh/en,
  `docs/user-manual/{zh,en}/`); two community channels went live — QQ group
  `1131682176` and Discord (`discord.gg/2PBUDYuD5`) — with stable links and QR images
  checked into `docs/images/` (expiring group-QR screenshots are never inlined);
  `AGENTS.md` gained a "documentation division of labor" discipline (facade / manual /
  ledger responsibilities are mutually exclusive).

### Fixed

- **Crash when toggling remote or hot-restarting a channel (Windows)**: turning the
  Remote Access master switch off or hot-restarting a channel could abort the whole
  process (0xc0000409 crashes, observed three times on 2026-10-23) — the stop path of
  the Codex APP desktop IPC spawned its shutdown from a synchronous command thread
  with no async runtime, and the panic crossed a no-unwind WebView2 callback boundary;
  the shutdown is now spawned via the app runtime, so toggling no longer crashes the
  app.
- **Updater install network fallback and honest errors**: the install layer had no
  network fallback (the discovery layer does) — when the system proxy broke
  mid-install, the download timed out reaching GitHub directly and only reported a
  manifest verification failure. Installation now runs two hops: the plugin's default
  pipeline first (10s connect timeout), then on transport-class failures a retry
  through a no-proxy client pinning DoH-resolved addresses for github.com and both
  release asset hosts. Errors are flattened through the full source chain, so the
  failure toast shows the actual cause.

### Engineering

- Gate cleanup for the A package: one closure in `tests/zcode_config_remote.rs` replaced
  with a function reference (new clippy `redundant_closure` warning; test-only, no
  behavior change).

## [0.6.2] - Unreleased

### Upgrade Notes

- **One-time UI language reset**: older versions cached the auto-detected system
  language in the same local key as manual choices, so the two could not be told
  apart. From this version on, only manual choices are remembered. After
  upgrading, desktop users who had manually switched the UI language will fall
  back to the system language once — pick your language again and it sticks
  (no longer overridden by system-language changes). Users who never switched
  manually will notice no difference.

## [0.6.1] - 2026-10-22

### Changed

- **Unified fixed-address setup completion**: Tailscale and Cloudflare Own domain use timelines that collapse only after real public board verification, retaining address, copy/open, QR and setup-details actions. Later failures keep the summary; historical completion never implies current availability.
- **Cloudflare Own-domain wizard**: for domains already on Cloudflare, guide tunnel creation, Token entry, confirmed connection, browser-side hostname binding and access verification. A live process alone is not a connected tunnel, and changed configurations require fresh verification.
- **First-install remote setup**: installations without a historical database enable Remote Access and launch at login by default, with device signatures off. Channels remain manual; upgrades retain their settings, and switches stay off after users disable them.
- **Tailscale installer experience**: packages are saved in the system Downloads folder. The wizard aligns actions on a vertical timeline, marks completion in green, shows file paths only after verification, reveals manual download only after automatic download fails, and reuses verified legacy packages; temporary downloads become clickable packages only after checksum verification.

### Fixed

- **Tailscale enable wrap-up**: enabling Funnel from the wizard now persists the channel switch and writes an audit entry, so the wizard completes after public verification and the channel restores after a relaunch (previously the flag was never written and the completion gate could not be met).
- **Approval link retention**: the browser approval link returned by first-time enable is no longer cleared by ordinary same-config status refreshes; config changes, channel off or master off still invalidate stale links and in-flight receipts.
- **Separate DNS waiting from repair**: confirmed missing public records show continuous elapsed time and an estimate of around ten minutes, with automatic read-only checks and no repeated retry action. Ordinary verification no longer resets Funnel; an explicitly confirmed re-enable performs repair. Enabled macOS extensions require no further action, and a stopped backend no longer implies missing approval.
- **macOS Tailscale installation**: the install action opens the system installer and rechecks status when Tuvis regains focus. Cancellation stays incomplete; installer failures retain exit codes and stdout/stderr instead of showing a blank error.
- **Re-verification process hint**: while the automatic re-check runs after a channel is re-enabled, the summary shows a spinner with the expected timing (usually within a minute); it clears itself once verification passes, letting users tell normal waiting from a fault.

### Added

- **Stop the Codex desktop turn from the phone**: while a desktop turn keeps running, the mobile outgoing-queue panel gains a "Stop the desktop turn & continue" action — it interrupts the turn currently running in desktop Codex, and **work that turn has not finished is lost** (the confirmation says so before anything is sent); once the turn stops, the queue continues on its own. Verified on macOS `26.1013.40934`; other Desktop builds are unverified and stay disabled.

### Changed

- **Default README switched to English**: `README.md` is now the English version
  (content moved in from `README.en.md`); the Chinese source-of-truth moved to
  `README.zh-CN.md`, with language-switcher links at the top of both files. The
  `AGENTS.md` bilingual asset table, project tree, and the README pin in the
  `usage-partial-coverage-lock` test were updated accordingly.

## [0.6.0] - 2026-10-21

### Added

- **Codex APP mobile control and message/file links (experimental)**: view message
  content and runtime status, send and queue messages, handle command approvals, and
  answer Plan-blocking questions from a phone. Capabilities are unlocked per exact
  Desktop build: Windows `26.1013.4894.0` supports send/status/command approval/Plan
  Q&A; macOS `26.1013.40934` supports send/status. `permissions` remains disabled.
  When Desktop is absent, the compatible exec path is used only if H3 is enabled;
  otherwise the request is refused honestly. Recognized file paths in message text
  open the preview; uploaded/referenced images, generated images under
  `~/.codex/generated_images/<threadId>/`, and files changed or viewed appear in the
  file panel with accurate source labels. A file entry jumps to its latest message
  reference within the current tail and at most four earlier pages (up to 50 messages
  per page). Previews read the current file on disk; historical file versions are not
  retained.
- **Codex session image extraction**: the file pool now reads `event_msg.local_images`
  so user-uploaded or referenced local images can appear in the panel (#69).
- **Token dashboard V2 and cost estimates**: refreshed usage summaries, trend and
  distribution views, provider aliases, model pricing from curated public sources,
  USD/CNY estimates, hourly cost calculation, and a separate cost CSV export.

### Changed

- **Sub-agent completion-report toggle extended to codex / kimi / opencode** (#152).
- **Codex APP message signature is off by default**: when the setting is off, Codex APP
  paths no longer carry the `[mobile name]` source marker by default; enable it in
  desktop Settings → Message signature. actionId idempotency is unchanged.
- **Usage formatting follows the locale**: English UI uses K/M/B and min/s/ms instead
  of Chinese number formats.

### Fixed

- **ZCode no longer reports completion while background work is unsettled**: synthetic
  notification messages no longer affect status or preview (#61 Task 3/4, PR #169).
- **Codex file-pool seq now aligns with message entries**, fixing ordering shifts
  (PR #168).
- **Sub-agent notifications / board / presets**: PR #167 cleanup; the parent card's
  sub-agent badge is recomputed from the same source (#134); the sub-agent-level preset
  entry is restored with distinct empty/unsupported states (#66).
- **Codex Q&A cancel now supports Esc** with explicit destructive-action wording
  (#80, PR #166).
- **Remote hardening**: escaped ingress hostnames parse correctly; saveToken gains a
  busy guard; named-parser config-line priority and charset handling are hardened.

### Engineering

- Split the bilingual changelog and backfilled v0.5.6–v0.5.8 English entries; added
  `nameEn` to the capability matrix; translated the remaining 7 English manual
  chapters; corrected the OpenClaw matrix cells.
- Added `CHANGELOG.en.md` to the website-notification trigger paths, so English-only
  changelog updates also trigger a website rebuild.

## [0.5.8] - 2026-10-16

### Added

- Added a built-in 3D tech-rabbit desktop pet as the new default, with a fresh 3D asset
  set and four voice groups of four original lines each; it is pre-selected on first use
  and sorted first in the built-in list. Built-in pet directories and descriptors were
  consolidated into a single registry (a uniform `public/pet/<dir>/` layout — adding a
  built-in pet now means creating a directory and appending one registry row). Legacy
  unprefixed activation pointers are normalized on read, upgraded installs keep their
  existing selection, and previously imported external pets with clashing names remain
  manageable.
- Errors now carry error codes across the board: failures in the Rust layer (remote,
  commands, services, inject, etc.) and the mobile network layer are unified as "error
  code + bilingual message", with a new CI gate that scans for hardcoded on-screen
  Chinese strings to prevent regressions (#123/#150/#157). Quote the error code in a bug
  report and the failure can be pinpointed.
- Added "session artifacts" as a fourth source for the file panel: artifact links and
  inline file references are scanned from session bodies (plain text and inline
  backtick paths, excluding triple-backtick code blocks) and wired into desktop and
  mobile from one shared implementation; preview honors an extension whitelist,
  existence filtering and rank-based fallback merging, and Codex sessions pass their
  working directory through as the base for resolving relative paths.
- The Codex trust gate became a persistent in-app action card instead of a one-shot
  system notification after registration (showing the card is not the same as clearing
  it — only "Copy command" or "Don't remind me again" dismisses it, and the card carries
  the `/hooks` command text as a fallback when the clipboard is unavailable). The reason
  a hook is not registered is stated as-is (on macOS Kimi shows "this platform does not
  ship a Hook channel yet — nothing to do"), and the signal-health section moved into
  the "Tools & Health" block in Settings.
- Tool badges refreshed to official artwork: DSH, Kimi, OpenClaw, WorkBuddy and others
  were redrawn from official sources, unifying the design language across all platforms
  (#149/#153/#154/#156).

### Changed

- Pet physics-based falling now defaults to off after every app restart (it can still
  be turned on within a session and cross-window sync is unchanged; per the user ruling
  of 2026-10-13).
- Dropped the OpenClaw option from board filters since message notifications are not
  supported for it; the card badge remains, and any leftover OpenClaw filter falls back
  to "All" automatically.

### Fixed

- Root-cause fix for false external-link reports on preset L4: with a symlinked HOME or the
  macOS `/var→/private/var` alias in play, self-created links are no longer misreported
  as external; ships with both universal and native defenses (#64).
- The injection key blacklist is now enforced at the Injector engine entry point, so
  all three legacy macOS channels go through the same check, closing the bypass gap
  (#79).
- Corrected the expected sha256 of the cloudflared macOS `.tgz`, fixing "External
  domain (no domain required): installer checksum verification failed".
- Remote "cannot connect" failures no longer auto-reset the Funnel: transient glitches
  now self-heal within a minute or two instead of wiping a perfectly good configuration;
  onboarding copy and diagnostic logs were updated to match.
- End-to-end fix for nested skills on Windows: skill id/name slash conventions are now
  uniform, and existing backslash ids are migrated automatically from the DB inventory —
  no manual action required.
- Cleared all pre-existing failures in the full Windows regression suite (11 + 1);
  win32 disambiguation logic was extracted and refactored, and identity markers now
  come from a single source of truth (#57).

### Engineering

- Checked the local gate script `ci-backend-local.sh` into the repository, mirroring
  the CI backend job (#127/#128).
- Switched the website-sync trigger to `peter-evans/repository-dispatch` (the
  previously referenced action does not exist under its official organization and failed
  silently).
- Annotated stale claims in historical docs (#131); added misuse guards around
  `tokens_used` and documented the invalidation conditions of `wait_child_bounded`
  (#97/#126); the settings layout lock now allowlists by value, with QR, copy and hint
  tiers put on record (#124/#125); removed the dormant API (#64).

## [0.5.7] - 2026-10-13

### Added

- Launch at login: a new "Launch at login" card in the Settings "Notifications & Pet"
  block, with a matching checkbox under the tray's "Show window" item — both share one
  read/write path (the `flip_and_sync` single point), so the checkbox state stays
  consistent in both places. The source of truth is a live read of the OS-side state
  (the Windows registry Run key across both HKLM/HKCU hives, plus the
  `Explorer\StartupApproved\Run` enable/disable markers written by Task Manager; macOS
  LaunchAgents) rather than local KV storage, so the UI cannot drift from reality;
  failures roll back and are reported honestly instead of claiming success. Autostart
  passes `--tuvis-autostart`, the main window slips quietly into the tray, and the pet
  still follows along. Uninstall cleanup has known limits: the NSIS hook only clears the
  HKCU entries of the user who ran the uninstall (leftover elevated HKLM entries are
  backstopped by write-after verification of the in-app switch); macOS has no uninstall
  hook, so the LaunchAgent plist must be removed by hand.
- Filter the desktop board by tool: an "All" chip plus eight single-select tool chips
  were added under the board tab bar; their icons and labels come from the same source
  as the card badges (chip background and selected state use the theme primary, which is
  not the same value as the badge background), and the filter semantics match the
  existing mobile `filterByAgent` (mobile already had filtering; this closes the gap on
  desktop). The choice is remembered in localStorage (dirty or invalid values fall back
  to "All"), the count bar always reflects all sessions, and an empty result names the
  tool honestly instead of showing a blank board.
- A "Show pet on startup" switch for the desktop pet, on by default and backward
  compatible via petConfig save sanitization (older saves without the key get the
  default true). This reverses the v0.5.6 behavior: startup visibility used to read
  `loadVisible()` (staying hidden if it was hidden last time) and is now decided by this
  switch, so users who had stashed the pet will find it showing up on every launch after
  upgrading.

### Changed

- Settings page reorganized: top-level section titles gained a primary color bar and
  larger type, and all 13 settings cards were unified with a gray header strip; the
  sections were reordered to Notifications & Pet → Appearance & Skins → Remote Access →
  Usage Stats → Tools & Health → Data Management, with Notifications & Pet as the
  default; on the remote end the appearance card is now always expanded, and on desktop
  its title was clarified to "Desktop appearance" to avoid ambiguity.

### Fixed

- Opening directories/files failed with `os error 6` in the installed build: a GUI
  process built with `windows_subsystem = "windows"` has no console, so child processes
  spawned for system actions such as "open directory / open MCP config file" reported
  an invalid handle. stdio is now explicitly nulled before spawning, and a
  `spawn_detached` single point consolidates every such spawn (directory/file opening,
  the deep-link `cmd /C start` fallback, and the window-marker helper exe). The root
  cause remains unconfirmed (the `detached` module docs list two candidate mechanisms
  and state plainly that "which one the explorer incident hit is still unexplained";
  real-machine handle-slot evidence is still needed); the one-click resume window path
  goes through the empty `ConsoleMode::OwnConsole` arm with stdio inheritance semantics
  and `CREATE_NEW_CONSOLE` unchanged verbatim (same-shape hardening, not re-verified on
  a real machine this round).
- Write actions on Codex APP sessions went down the wrong channel: the APP form (the
  ChatGPT desktop GUI) has no console, so the CLI keystroke-sequence channel that
  mode/permission switching and Q&A answering rely on simply does not work for it
  (confirmed on a real machine: `AttachConsole 0x80070006`, zero screen-read deliveries,
  Q&A `aborted:select`). Until the protocol arm lands, the four write endpoints
  `session-mode/switch`, `session-mode/menu`, `session-question/answer` and
  `session-approve` return 409 `app_form_pending` for sessions with
  `form=App ∧ tool=codex` before any terminal screen read or injection, and write an
  audit line (through the `audit_app_form_pending` single point); the mobile client then
  shows "APP sessions must take the protocol channel for this action — under
  development, please operate inside the APP" to guide users back to the APP. The CLI
  form is untouched and read-only endpoints are not gated. Note: `/session-send` and its
  queued insertion are a separate pre-existing path, already blocked for the APP form by
  `inject/routing.rs` with 403 `app_form`, and are not part of this new gating. A
  permanent hint was added under the Settings "Tool management" card
  (`settings.tools.boardRemoteNote`, bilingual): "Unchecked tools never appear on the
  board and cannot be messaged remotely".
- The launch-at-login registry entry silently failed when the path contained spaces:
  the Run value written by the upstream plugin is unquoted, so enabling failed on spaced
  paths while the status query still reported enabled. The value is now rewritten in
  place to a quoted form after enable (the pure `quoted_run_command` function plus
  boundary unit tests), rolling the entry back on failure; also added HKLM/HKCU
  dual-hive scanning, write-after verification on disable, and NSIS uninstall-hook
  cleanup of `Run\Tuvis` and `StartupApproved\Run\Tuvis`. (This item is hardening done
  while the same-release feature was being built; the feature does not exist in v0.5.6
  or earlier.)
- The desktop pet popped back up on its own a while after being stashed: v0.5.6
  created the window with `.visible(true)` right away, so once the UI had hidden it via
  `loadVisible()`, the 5-second startup fallback could not tell "not shown yet" from
  "the user wants it hidden" and called `show()` again, with nothing to hide it
  afterwards. Visibility is now governed by the authoritative `PET_PAGE_SPOKE` marker
  (`set_pet_visible` sets it, successful creation clears it, reuse keeps it, and the
  startup fallback checks it first), so a hide triggered by closing the follower is no
  longer forced back open, while the deadlock rescue path is preserved. Known
  limitation: the tray arm bypasses `set_pet_visible` (window-level direct show/hide),
  so within the roughly 5.8s narrow window the fallback may not respect a tray-initiated
  hide.

### Docs

- Truth-first rewrite of all 13 chapters of the user manual (full Chinese plus English
  skeletons — en/06-presets actually fell back from finished prose to a skeleton, while
  the other English chapters had been placeholder templates all along), with real-machine
  screenshots added, screenshot placeholders eliminated, and the manual mounted at the
  website's `/docs`.
- Added `docs/capabilities.json` as the structured source of the capability matrix
  synced with the website; in the README's multi-device capability matrix, OpenClaw's
  "view from phone" and "file preview" columns were corrected to ❌ based on real
  testing.

## [0.5.6] - 2026-10-11

### Added

- Tuvis branding applied across the board: app icons at every size on Windows and macOS
  were unified with the Tuvis visual identity in the rename batch, and the remote board's
  brand row and the PWA manifest were re-skinned to match.
- Remote Codex mode/permission switching with a screen-read loop: the Plan ↔ Act mode pair
  is a direct two-chip selector (a tap sends shift+tab; the current tier is highlighted and
  not clickable), and the four permission tiers are direct chips as well (read-only /
  default / auto-approve / full trust, including the second confirmation and proxy press
  for Full Access); the result is read back from the terminal screen, and the card
  highlight follows it.
- Full screen-read chain for the Codex Q&A panel: a two-phase machine covering
  multi-question, single-choice and Other-notes cases, guards against misreading the
  scroll-back region, plus a last-question zeroing gate and a two-condition submit gate;
  the Q&A card and the terminal always stay on the same question.
- Codex notes line synced by screen read: note text in the terminal is echoed live into the
  mobile input box (re-read after paging and after every action), and all three remote
  actions are in place — send, clear and overwrite (an input-line readiness gate plus a
  clear loop of at most two tabs, reading the screen after each press).
- Overwrite/clear lit up on the Claude multi-select card: once text has been written, the
  card offers Edit / Overwrite / Clear (the key sequences were settled on a real machine on
  2026-10-03; a previously missed flag had kept them dark).
- Graceful degradation for the approval card: when an approval is detected but the terminal
  dialog cannot be read, Allow / Deny are disabled and re-read automatically every 2.5s,
  becoming available only once options are visible (so a multi-select question panel can no
  longer be hit by approval keys).

### Fixed

- Codex mode read backwards: with the two-row status bar, Plan was read as Default (the
  bottom-up scan stopped at the shortcut row) — it is now judged from two sources (the
  status-bar row window plus the event row).
- Codex notes chain revived: the "notes are not persisted" behavior measured on 0.160.0 did
  not reproduce on 0.162.1, so remote free-form answering was restored and rewritten around
  the new semantics (clear = double tab; wait for the input line to mount before typing;
  verification failures now reconcile silently instead of flashing red).
- Silent reconciliation of cross-question races: paging to another question or submitting
  in the terminal while a verification window is open (legitimate terminal-side actions) no
  longer raises a red error — the page refreshes to realign the card and the input box.
- The Claude multi-select panel no longer masquerades as an approval dialog: options
  carrying checkbox marks make the dialog parser refuse to emit choices (a checkbox guard);
  the version drift where claude 2.1.287 stopped writing pending questions to the
  transcript is on record as issue #140.
- Self-healing migration for the renamed hooks: entries registered by pre-rename versions
  under the old `~/.mam` path are rewritten to the new `~/.tuvis` spec on startup (removing
  both the orphan-entry hook error and the silent failure of the question channel), and
  parsing now tolerates the BOM in codex's hooks.json that had broken registration.
- Notes starting with a digit are no longer invisible: notes such as "3 suggestions" were
  wrongly skipped as option lines — with the exclusion rule now sharing its source with the
  option-line parser, they are read as usual.

### Docs

- Added a secondary-development guide to both READMEs (forks must keep the MIT copyright
  notice; upstream PRs are welcome) along with a header hero image; the capability matrix
  moved to the end as a master table (12 capability rows × 8 terminals), with descriptions
  calibrated to this release's capabilities.
