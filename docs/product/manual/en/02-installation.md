# Download and Installation

**Fresh-install defaults**: with no historical database (including legacy-brand data), Remote Access starts enabled, the release app attempts to register launch at login once, and remote-message device signatures are off. Enable individual channels as needed; upgrades keep their previous state. Turning a switch off is respected on later launches. Launch-at-login status reflects the operating system; a failed registration can be retried in Settings.

Tuvis ships installers for Windows and macOS — grab the latest release from GitHub Releases; installation takes about a minute.

## Download

1. Open the releases page: `https://github.com/jarvislee90s-dot/tuvis/releases`;
2. In the latest release's Assets list, download the installer that matches your system:
   - **Windows**: `Tuvis-*-Windows-x64-setup.exe`
   - **macOS**: `Tuvis-*-macOS.dmg`

> 💡 Trust only this repository's Releases page. Downloads offered by any other site or channel are third-party distributions — vet the risk yourself.

## System requirements

- Windows 10 or later (64-bit);
- macOS 14 or later.

## Installation steps

**Windows**: double-click the installer and click "Next" through the wizard to the end.

![Install wizard](../img/install-wizard.png)

**macOS**: open the `.dmg`, drag Tuvis into the Applications folder, then launch it from Launchpad or Finder.

## Installation notes

- **Upgrading from 0.5.2 or earlier**: these old versions (when the app was still named MultiAgents Manager) have a broken in-app updater — download the new installer manually instead. Old and new are two separate app registrations, so **uninstall the old version before installing the new one** to avoid ending up with two tray icons. Your data migrates automatically from `~/.mam` to `~/.tuvis` on first launch of the new version — nothing to move by hand.
- **Upgrading from 0.5.5 or later**: just install over the top with the new installer; hook configuration is migrated automatically at app startup.
- **All data stays local**: everything Tuvis keeps (database, settings, logs, skill repository) lives under `~/.tuvis/` — back up that directory and you have backed up everything.
- **macOS says the app "is damaged" or "can't verify the developer"**: this is the system's standard block for unnotarized packages. In System Settings' Privacy & Security area, find the allow prompt about Tuvis and click Allow; or right-click the app (Ctrl+click) and choose "Open" once — after that double-clicking works normally.

## Data directory (`~/.tuvis/`)

The app stores its data in `~/.tuvis/`:

| Path                            | Purpose                                                        |
| ------------------------------- | -------------------------------------------------------------- |
| `~/.tuvis/tuvis.db`             | SQLite database (settings, extensions, presets, session cache) |
| `~/.tuvis/skills/`              | Global skill repository                                        |
| `~/.tuvis/mcp/`                 | Global MCP server configs                                      |
| `~/.tuvis/hooks/status-hook.sh` | Shared Hook script for status events                           |
| `~/.tuvis/events/`              | Hook event files (auto-cleaned, 30s TTL)                       |

## Development builds (running from source)

**macOS dev-build note**: dev builds (unsigned packages) need a one-time manual grant
before the first one-click session resume opens a window — allow Tuvis to control
Terminal / iTerm2 under **System Settings → Privacy & Security → Automation** (TCC does
not prompt automatically). Without the grant, osascript fails with -1743, and resume
replies with a "macOS automation permission missing" error pointing to these
instructions.

**Helper build gate (locked in by batch-C task T2)**: `tuvis-hook-listener` (the
hook-event listener helper) and `tuvis-marker` (the window-title marker) both sit
behind Cargo `required-features` gates — **without the features they are not built at
all**, so the app's startup `ensure_hook_script` step cannot find the same-directory
helper and skips installing it, leaving the copy under `~/.tuvis/bin/` stale forever
(field-observed failure: hook events missing the `tool_name` payload → Q&A-card
channel A never recognized).

- `pnpm tauri:dev` / `pnpm tauri:build` **already include the features** (`hook-listener`
  and `marker-helper`, the latter implying the former) — just use them as usual;
- Building a helper standalone (e.g. for on-device `#[ignore]` tests) must pass the
  feature explicitly: `cd src-tauri && cargo build --bin tuvis-hook-listener --features hook-listener`;
- Same for bin tests: `cargo test --bin tuvis-hook-listener --features hook-listener`;
- macOS packaging (`release:macos` / the macOS job in release.yml) runs bare
  `pnpm tauri build` with explicit arguments and is **not** affected by the npm scripts
  above — the extra bins would break the universal build, so the macOS side
  deliberately omits the features (the helpers serve the Windows channel).
