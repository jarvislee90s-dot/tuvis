# @tuvis/dsh-remote-access

External plugin bundle for Tuvis remote access to DeepSeek Harness (DSH)
Desktop. One package, two faces:

- **Host half** (`lib/index.js`, `exports["."]`): a Cordis plugin running
  inside the DSH `desktop` profile. It injects the host services `connection`
  and `webServer`, mounts a health route at `/tuvis-remote/health`, and returns
  a disposer from `apply` (a Cordis effect), so disposal empties the route
  table with no dangling routes left behind.
- **Client half** (`lib/client.js`, `exports["./client"]`): the local
  authorization card, served to the browser as a **classic script** that
  self-registers through the official envelope
  `window.__ModuleLoader__.load({ id: "@tuvis/dsh-remote-access", factory })`.
  It has no top-level `import`/`export` (the host injects it as a `<script>`,
  where an ESM top level is a SyntaxError), takes React from the host seed
  table through `require("react")` instead of bundling its own copy, and mounts
  through the slot registry at `settings.section` (probing `slots.spec` first,
  because the seat may not exist until the settings page opens). It shows the
  binding state and the two-end compare code only — never a credential.

No `/remote` chat route, no full WebUI.

## Install form

The release artifact is a **prebuilt tgz** (`resources/dsh/remote-access.tgz`
plus `resources/dsh/manifest.json` in the repo), installed by the Tuvis
desktop app via `dsh plugin --profile desktop add <tgz>`. **The user machine
needs no Node, no network, and no source build**:

- the tgz contains no `node_modules` at all (zero runtime dependencies;
  Cordis / DSH / React are provided by the host as peers);
- the packed manifest strips `scripts`, so there are no install hooks ever;
- `manifest.json = { pluginVersion, sha256, bindingProtocol: 1 }`; the
  installer verifies the tgz SHA-256 (streaming) before writing anything.

## Package contract (the `dsh` field)

```jsonc
{
  "dsh": {
    "engines": { "dsh": ">=0.2.1-rc.3" }, // lower bound only, no exact-version exemption
    "bundle": { "patch": "./cordis.patch.yml" },
    "client": { "inject": ["@deepseek-ai/dsh-client-connection"], "platform": "web" },
  },
}
```

## Runtime behavior (host half)

- **Access boundary**: self-registered routes do **not** inherit the official
  `/api` auth or Host/Origin fence (a measured host fact), so every route
  carries its own gate: the Host hostname must be a loopback literal (the port
  is not part of the check), a present Origin must match Host verbatim, and
  `sec-fetch-site: cross-site` is rejected — with 403 plus a fixed short
  reason. Only two **read-only** routes exist (`/tuvis-remote/health`, 
  `/tuvis-remote/binding-state`); there is no approve/grant/revoke write
  endpoint on any plugin route — approval happens only in the Tuvis settings
  card (Tauri IPC).
- **Binding session**: at most **one** IPC connection over its lifetime. While
  awaiting approval the connection is retained and the compare code does not
  churn across reconnects (Rust idempotency per client key is the backstop);
  once approved the credential is exchanged and delivered over that same
  connection, after which a read loop consumes `granted`/`revoked`. Revocation
  is **permanent** and arrives either as a pushed `revoked` frame or as
  `DSH_PEER_REVOKED` on the next re-hello (a revoked key is refused forever):
  it moves to the revoked state, stops delivery, and stops reconnecting
  (rebinding requires a fresh identity). `DSH_AUTH_NOT_APPROVED` is
  **recoverable** (never-approved key / expired pending record / concurrent
  replacement): it returns to pending with a backoff reconnect and never writes
  the revoked tombstone.
- **Data root**: `TUVIS_DSH_STATE_DIR` **is** the state directory (same
  semantics as Rust); unset falls back to `~/.tuvis/dsh`. The plugin never
  reads `TUVIS_HOME`.
- **Cookie exchange**: `connection.authenticatedUrl(origin)` → `GET
  /connect?token=…&interactions=answer` with `redirect: "manual"`, then read
  the full `name=value` of the `dsh-auth-*` cookie from a 302 response. Any
  deviation raises a typed error carrying the **stage name**; the token and
  the cookie never enter an error message or a log.

## Development

```bash
pnpm --filter @tuvis/dsh-remote-access build   # tsc: src/ → lib/ + classic-script src/client/ → lib/client.js
pnpm --filter @tuvis/dsh-remote-access test    # build first, then vitest (artifact tests run the built file)
node scripts/build-dsh-plugin.mjs              # deterministic pack → resources/dsh/
node scripts/build-dsh-plugin.mjs --check      # rebuild into a temp dir, byte-compare committed artifacts
```

The client half is compiled by `tsconfig.client.json` with
`module: "preserve"` so its output stays a plain classic script: no module
wrapper, no `export`, and `require` stays a free identifier resolved at run
time by the host loader. `test/client-entry.test.ts` executes the **built**
`lib/client.js` inside a `node:vm` sandbox whose `window.__ModuleLoader__` is a
recording fake, which is how the envelope and slot mounting are verified
without a browser (the review's requirement: verify the real client entry, not
a test factory).

### Deterministic packing

Same input, same bytes (see the header comment of
`scripts/build-dsh-plugin.mjs`): tar entries sorted by path, every mtime set
to epoch 0, uid/gid 0; gzip written without a timestamp; the packed
package.json is rebuilt from a fixed-key whitelist (stripping
`scripts`/`devDependencies`); the archive contains relative paths only.

### Test baseline

The baseline DSH is `0.2.1-rc.3`: devDependencies pin `0.2.1-rc.3` exactly,
peerDependencies declare only the `>=0.2.1-rc.3` lower bound to cooperate
with the host's dynamic contract admission — no exact-version exemption
(`allow-version`).

## License

MIT (see `LICENSE`; `NOTICE` documents third-party material).
