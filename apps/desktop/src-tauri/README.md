# Hermes Desktop — Tauri shell

The native desktop shell for [Hermes Agent](../../../README.md), implemented as a
[Tauri 2](https://v2.tauri.app/) application in Rust. It replaces the Electron main
process in [`../electron/`](../electron/) while driving **the same renderer**
(`../src/`) against **the same Python backend** (`hermes serve`) as the Electron
shell, the web dashboard, and the CLI.

The three-party contract is unchanged ([`../AGENTS.md`](../AGENTS.md)):

| Party | Authoritative for |
| --- | --- |
| **This Rust shell** | process lifecycle, native filesystem/git/terminal, windows, OS integration |
| **The renderer** (`../src/`) | navigation, presentation, ephemeral interaction state |
| **The Python backend** | sessions, tools, model calls, streaming |

The renderer is not forked. `tauri.conf.json` serves the exact Vite build the
Electron shell serves (`frontendDist: ../dist`), and the JSON-RPC/WebSocket
gateway contract is identical.

> **Looking for install / updating / connections?** See [`../README.md`](../README.md),
> which still covers the Electron shell. This file covers building and running the
> Tauri shell.

---

## Migration status

The shell is an in-progress port. What exists today:

| Area | Status |
| --- | --- |
| Window lifecycle, show/focus handshake | ✅ ported |
| Backend spawn / ready / teardown | ✅ ported (`backend/`) |
| Terminal PTY (replaces `node-pty`) | ✅ ported (`commands/terminal.rs`) |
| Filesystem, git, system info | ✅ ported (`commands/fs.rs`, `commands/system.rs`) |
| Logging into `desktop.log` | ✅ ported (`logging.rs`) |
| Single-instance lock | ✅ ported (lock + focus existing window) |
| First-launch bootstrap installer | ✅ ported (`bootstrap.rs`); see [Build](#build-installers) |
| Overlay / quick-entry / wake windows | ❌ not implemented |

---

## Prerequisites

| Requirement | Notes |
| --- | --- |
| **Node.js** `^22.22.0 \|\| ^24.11.0 \|\| >=26.0.0` | Root `engines` in [`../../../package.json`](../../../package.json). |
| **npm** `<11.10.0 \|\| >=11.17.0` | Same `engines` block. |
| **Rust** ≥ 1.77 | `rust-version` in [`Cargo.toml`](./Cargo.toml). |
| **Windows** | Visual Studio Build Tools with the C++ workload (Rust's MSVC toolchain), plus the WebView2 runtime (preinstalled on Windows 11). |
| **macOS** | Xcode Command Line Tools; WebKit is a system framework. |
| **Linux** | `libwebkit2gtk-4.1-dev`, `build-essential`, `curl`, `wget`, `file`, `libxdo-dev`, `libssl-dev`, `libayatana-appindicator3-dev`, `librsvg2-dev`. |
| **A Hermes Python backend** | Either an existing install or a source checkout venv. **The Tauri shell cannot install one for you** — see [Backend resolution](#backend-resolution). |

The Tauri CLI is **not** a separate install: `@tauri-apps/cli` is a workspace
devDependency of `apps/desktop`. There is no need for `cargo install tauri-cli`
or a global `npm i -g @tauri-apps/cli`. Always invoke it through the npm scripts
below so it resolves from the root `node_modules`.

`reqwest` is built with `rustls-tls`, so no system OpenSSL headers are required.

---

## Install (once, from the repo root)

```bash
npm install
```

This must be run **at the repo root**, not in `apps/desktop`. The workspace
hoists `apps/desktop`'s dependencies into the root `node_modules`, and a partial
root install leaves the app importable-looking but unbuildable.
`scripts/assert-root-install.mjs` guards both `dev:renderer` and `build:renderer`,
failing early with `cd <root> && npm ci` instead of dying inside Vite.

---

## Run in development

Run these **from the repo root** — the first line sets the backend path from where
you are now, before `cd` moves you:

```powershell
# Windows (PowerShell)
$env:HERMES_DESKTOP_BACKEND = "$PWD\.venv\Scripts\hermes.exe"
$env:HERMES_HOME            = "$env:LOCALAPPDATA\hermes"
cd apps\desktop
npm run dev:tauri
```

```bash
# macOS / Linux
export HERMES_DESKTOP_BACKEND="$PWD/.venv/bin/hermes"
export HERMES_HOME="$HOME/.hermes"
cd apps/desktop
npm run dev:tauri
```

Or stay in the root and name the workspace instead:

```bash
npm run dev:tauri -w apps/desktop
```

> **`dev:tauri` lives in `apps/desktop/package.json`, not the root one.** Every
> desktop script (`dev:tauri`, `build:tauri`, `tauri`, …) is defined there, so
> running it from the repo root fails with
> `npm error Missing script: "dev:tauri"`. Either `cd apps/desktop` first or pass
> `-w apps/desktop`.

`npm run dev:tauri` is just `tauri dev`. It runs the full pipeline for you:

1. `beforeDevCommand` → `npm run dev:renderer` starts Vite on
   `http://127.0.0.1:5174` (`tauri.conf.json:8`).
2. Cargo builds the `Hermes` debug binary and launches it.
3. The Rust shell picks up `devUrl` and loads the renderer from the dev server.

**You do not need a second terminal for Vite.** Rust changes rebuild and restart
the app automatically; renderer changes go through Vite HMR.

### Why the two environment variables

The shell resolves its backend through a three-rung ladder, and on a typical
developer machine **none of the rungs resolve by default**
([`src/backend/command.rs:99`](./src/backend/command.rs)):

1. `HERMES_DESKTOP_BACKEND` — an explicit executable path.
2. `<venv>/Scripts/hermes.exe` (Windows) or `<venv>/bin/hermes` (POSIX), where
   `<venv>` is `HERMES_VENV` if set, otherwise `<HERMES_HOME>/venv`.
3. `hermes` on `PATH`.

Setting `HERMES_DESKTOP_BACKEND` hits rung 1 and points the shell at a specific
checkout — [`command.rs:100`](./src/backend/command.rs) documents it as the
escape hatch for exactly this. Setting `HERMES_HOME` makes the app use your real
data instead of an empty home (see the warning below).

An equivalent, if you prefer to stay on the managed-venv rung:

```bash
export HERMES_VENV="<repo-root>/.venv"
```

[`src/paths.rs:38`](./src/paths.rs) reads `HERMES_VENV` first, so rung 2 resolves
to that venv without overriding the backend command. Use one or the other, not
both.

### Sandboxing the run

To keep a dev run away from your real config, point `HERMES_HOME` at a
throwaway directory. The shell passes it to the backend as both `HERMES_HOME`
and the child's working directory ([`src/backend/manager.rs:294`](./src/backend/manager.rs)):

```bash
export HERMES_HOME=/tmp/hermes-dev-scratch
```

> **Windows note.** `paths::hermes_home()` mirrors the Python side's platform
> default (`hermes_constants.py:45`): `%LOCALAPPDATA%\hermes` on Windows
> (falling back to `~\AppData\Local\hermes`), `~/.hermes` elsewhere. Set
> `HERMES_HOME` explicitly to point at a specific home.

---

## Verify the backend came up

The shell logs everything to `<HERMES_HOME>/logs/desktop.log`
([`src/paths.rs:34`](./src/paths.rs)). A healthy boot contains:

```
spawning backend: ...\hermes.exe serve --host 127.0.0.1 --port 0
backend ready on http://127.0.0.1:<port> (pid <pid>)
```

Tail it while the app is starting:

```powershell
Get-Content "$env:LOCALAPPDATA\hermes\logs\desktop.log" -Tail 40 -Wait
```

```bash
tail -f "$HOME/.hermes/logs/desktop.log"
```

The child's stdout/stderr is mirrored into the same file with a
`backend[stdout]` / `backend[stderr]` prefix
([`src/backend/manager.rs:112`](./src/backend/manager.rs)), so Python tracebacks
show up there too.

---

## Build installers

```bash
cd apps/desktop
npm run build:tauri
```

`build:tauri` is `tauri build`. It runs `beforeBuildCommand` →
`npm run build:renderer` (Vite build into `../dist`) and then bundles the Rust
release binary against `frontendDist: ../dist`.

To build only the bundles you want, use the `tauri` passthrough script:

```bash
npm run tauri -- build --bundles nsis,msi          # Windows
npm run tauri -- build --bundles dmg,app           # macOS
npm run tauri -- build --bundles appimage,deb,rpm  # Linux
```

`tauri.conf.json` lists all seven targets (`dmg`, `app`, `nsis`, `msi`,
`appimage`, `deb`, `rpm`), which spans every platform; only the host-applicable
ones are produced. Being explicit with `--bundles` avoids depending on that
filtering.

Artifacts land under `src-tauri/target/release/bundle/<format>/`, and the
unpacked binary is `src-tauri/target/release/Hermes` (`Hermes.exe` on Windows).

Notes:

- **The first build is slow.** `[profile.release]` uses `lto = true`,
  `codegen-units = 1`, `opt-level = "s"`, and `strip = true`
  ([`Cargo.toml:74`](./Cargo.toml)), so expect a long link step.
- **Windows installers embed the WebView2 bootstrapper**
  (`bundle.windows.webviewInstallMode: embedBootstrapper`), so the NSIS/MSI
  packages work on machines without WebView2 preinstalled.
- **A packaged Tauri build requires a pre-existing backend.** The Electron shell
  can bootstrap a runtime into `HERMES_HOME` on first launch; the Tauri shell has
  no equivalent yet — `resolve_backend_command` returns an error and nothing
  installs anything ([`src/backend/command.rs:129`](./src/backend/command.rs)).
  An installer produced here only runs on a machine that already has `hermes` in
  a managed venv or on `PATH`.

### The other build (Electron)

`npm run build`, `npm run pack`, and the `dist:*` scripts still build the
Electron shell. Those are unaffected by anything in this directory.

---

## What happens on launch

The Tauri shell is a **two-part boot**: the Rust process does almost nothing at
startup — it neither creates a visible window nor starts the backend — and the
**renderer drives both** once it has mounted. Electron is the inverse (it creates
the window and starts the backend from the main process, concurrently with page
load).

### Phase 0 — Build time

- Dev: `beforeDevCommand: npm run dev:renderer` → `devUrl: http://127.0.0.1:5174`.
- Production: `beforeBuildCommand: npm run build:renderer` → `frontendDist: ../dist`.
- Both renderer scripts begin with `node scripts/assert-root-install.mjs`.
  ([`tauri.conf.json:7`](./tauri.conf.json))

### Phase 1 — Rust process entry

`src/main.rs` calls `run()`. `run()` is four fixed steps, none of which creates a
window or starts a backend ([`src/lib.rs:19`](./src/lib.rs)):

1. Logging first — Tauri version and resolved `hermes_home` appended to
   `desktop.log`.
2. Register seven plugins, one-for-one replacements for Electron main-process
   capabilities: dialog, opener, process, notification, clipboard-manager,
   global-shortcut, os.
3. `.setup()` does exactly one thing: `AppState::new()` + `app.manage(state)` —
   a shared `reqwest` client, a backend slot initialized to `None`, and a
   terminal registry.
4. Register the `backend` / `window` / `terminal` / `fs` / `system` command
   handlers, then `.run()` into the event loop.

### Phase 2 — A window exists but is invisible

The `main` window is declared in configuration with `"visible": false`
([`tauri.conf.json:27`](./tauri.conf.json)). The process is alive, the window
exists, the webview begins loading — and nothing is on screen. Someone else has
to reveal it.

### Phase 3 — Renderer module evaluation

The import order at the top of `src/main.tsx` is a contract, not style:

- `import './desktop-bridge'` must come first — components read
  `window.hermesDesktop` during module init. It installs the bridge and sets
  `__HERMES_DESKTOP_SHELL__ = 'tauri'`.
- Four side-effect-only store imports (active-work, power, translucency,
  user-bubble-transparency) configure window appearance and state before the
  first frame.
- `@/debug/dev-only` must precede `react-dom`: react-dom grabs the devtools hook
  at module-init time, so importing it later misses every commit. In production
  builds Vite aliases it to a no-op.
- Two imperative installs follow: the clipboard shim and the selection-copy
  color guard.

### Phase 4 — Mount, and the window is revealed

- `?win=` is read first. `overlay`, `quick`, and `wake` each `createRoot` into a
  small standalone app and **do not mount the full App**; everything else (the
  main window) proceeds to the full render.
- `createRoot(#root).render(...)`, with `ShowMainWindowOnMount` as a sibling
  preceding `RootErrorBoundary`.
- Its `useEffect` fires after the first commit and calls `showMainWindow` —
  fire-and-forget — which the bridge channel map routes to Rust's
  `hermes_window_ready` command, ending in `show()` + `set_focus()`.

At this moment the window is visible and **not one byte of the Python backend
has been started.**

### Phase 5 — The backend is started lazily, by the renderer

`ContribWiring` in the render tree attaches `useGatewayBoot`; its effect calls
`desktop.getConnection()`, which is the real entry point into
`ensure_backend` ([`src/commands/backend.rs:83`](./src/commands/backend.rs)):

1. If a live handle for the same profile already exists, reuse it and return.
2. Otherwise tear down any stale handle.
3. `backend::spawn_backend(profile)` — resolve the command, mint a fresh session
   token, spawn the child.

The spawned argv is `[--profile <name>] serve --host 127.0.0.1 --port 0`
([`src/backend/command.rs:20`](./src/backend/command.rs)). Key environment:

| Variable | Value |
| --- | --- |
| `HERMES_HOME` | resolved home; also the child's working directory |
| `HERMES_DESKTOP` | `1` |
| `HERMES_DASHBOARD_SESSION_TOKEN` | fresh UUID per backend, echoed to the renderer |
| `PYTHONUNBUFFERED` | `1` |
| `PYTHONUTF8` | `1` unless you set it |
| `PATH` | managed Node dirs → venv bin → inherited `PATH` → POSIX sane entries |

On Windows the child is spawned with `CREATE_NO_WINDOW`, so no console flashes.
([`src/backend/manager.rs:284`](./src/backend/manager.rs), [`src/backend/env.rs:81`](./src/backend/env.rs))

### Phase 6 — Readiness handshake

Readiness is **two confirmations**, not one
([`src/backend/manager.rs:140`](./src/backend/manager.rs), [`src/backend/ready.rs`](./src/backend/ready.rs)):

1. **stdout sentinel.** The stdout buffer is polled every 50 ms for
   `HERMES_BACKEND_READY port=<N>` (the legacy `HERMES_DASHBOARD_READY port=<N>`
   is also accepted). Budget: 90 s by default, because a cold install must import
   the whole `hermes_cli.main` → FastAPI/uvicorn chain and Windows real-time AV
   rescans every freshly written `.pyc`. If the child exits first, or the
   deadline passes, the error carries the last 2 KB of child output.
2. **HTTP probe.** `GET /api/health` with the session token; a `404` switches to
   `/api/status` for backends predating the health route. Any 2xx passes.
   500 ms between attempts, 5 s per request, 45 s total.

If the **first** spawn dies before announcing and the argv contains `serve`, the
shell retries once as `dashboard --no-open` — an older runtime exits at once on
the unknown `serve` argument, and both forms start the same headless gateway.
A failed attempt is always reaped, so the retry never leaves an orphan holding
the venv open. ([`src/backend/manager.rs:233`](./src/backend/manager.rs))

### Phase 7 — Gateway connection and the boot overlay

- Rust returns `ws://127.0.0.1:<port>/api/ws?token=<token>`; the renderer calls
  `gateway.connect(wsUrl)` to open the JSON-RPC channel. `hermes_api` is the only
  HTTP path to the backend and injects the same token.
- The overlay state machine lives **entirely in the renderer**: `$desktopBoot`
  starts at `phase: 'renderer.init'`, `progress: 2`, `visible: true`; the boot
  hook advances it; `completeDesktopBoot()` sets `progress: 100`,
  `visible: false`, after which the overlay unmounts.
- **The Tauri shell emits no boot progress.** The bridge maps `onBootProgress`
  to a `hermes:boot-progress` event, but nothing in Rust emits it. Progress is
  computed renderer-side — still real, just no longer round-tripping through a
  main process.

---

## Backend resolution

Precedence is written down in one place, as a pure function, in
[`src/backend/command.rs:99`](./src/backend/command.rs).

| # | Candidate | Set via | Probed before use? |
| --- | --- | --- | --- |
| 1 | Explicit executable | `HERMES_DESKTOP_BACKEND` | **No — used verbatim** |
| 2 | Managed venv `<venv>/Scripts/hermes.exe` (Windows) or `<venv>/bin/hermes` (POSIX) | `HERMES_VENV`, else `<HERMES_HOME>/venv` | Yes (`is_file`) |
| 3 | `hermes` on `PATH` | — | Yes (`which`) |

> **Rung 1 is taken on trust.** [`command.rs:102-111`](./src/backend/command.rs)
> passes the value straight to `PathBuf::from` with no existence check and no
> absolutization, so a **relative** path resolves against the shell process's
> working directory — under `tauri dev` that is `apps/desktop/src-tauri`, not the
> repo root. `.\.venv\Scripts\hermes.exe` becomes `apps/desktop/src-tauri\.venv\…`
> and every spawn fails with `os error 3` (path not found), which the boot path
> re-attempts rather than aborting. Give rung 1 an **absolute** path.

If all three fail, startup reports:

```
Hermes backend not found. Looked for a managed runtime at <path> and for `hermes` on PATH.
```

Unlike the Electron shell, this ladder has **no source-checkout rung** and no
first-launch bootstrap installer rung — those exist only in
[`../electron/backend-command.ts`](../electron/backend-command.ts).

---

## Environment variables

Variables this shell reads:

| Variable | Read at | Purpose |
| --- | --- | --- |
| `HERMES_DESKTOP_BACKEND` | `command.rs:102` | Rung 1 — absolute path to the backend executable. Whitespace-only is ignored. |
| `HERMES_VENV` | `paths.rs:39` | Venv root overriding `<HERMES_HOME>/venv`. |
| `HERMES_HOME` | `paths.rs:12` | Hermes home. Used for `logs/`, the venv fallback, and passed to the child along with its working directory. |
| `HERMES_DESKTOP_PORT_ANNOUNCE_TIMEOUT_MS` | `ready.rs:31` | Port-announcement budget. Clamped to a 45 s floor so a malformed value cannot make boot flakier than the default; default 90 s. |
| `PYTHONUTF8` | `env.rs:96` | Passed through to the child; forced to `1` when unset. |
| `PYTHONPATH` | `env.rs:84` | Passed through to the child. |
| `PATH` | `env.rs:69` | Inherited by the child after the venv bin directory. |

**Do not use the Electron shell's names here.** The website's
[environment variable reference](../../../website/docs/reference/environment-variables.md)
lists `HERMES_DESKTOP_HERMES`, `HERMES_DESKTOP_HERMES_ROOT`, and
`HERMES_DESKTOP_PYTHON` — those are read by `../electron/`, **not** by this
shell. The Tauri shell reads `HERMES_DESKTOP_BACKEND`.

---

## Known differences from the Electron shell

| Concern | Electron | Tauri |
| --- | --- | --- |
| Single instance | `requestSingleInstanceLock()` + deep-link routing on `second-instance` | Lock + focus existing window (deep-link routing not yet wired) |
| Window creation | `createWindow()` with `show: false`, auto-revealed on `ready-to-show` | Declared in config, revealed by an explicit renderer callback |
| Backend start | Immediately after `loadWindowUrl()`, in parallel with page load | Lazy, on first renderer `getConnection` |
| Boot progress | Main process broadcasts `hermes:boot-progress` | No producer; computed renderer-side |
| Auxiliary windows | overlay / quick / wake | No corresponding commands |
| First-launch bootstrap | Installs a runtime into `HERMES_HOME` | Runs `install.ps1`/`install.sh` stage-by-stage (`bootstrap.rs`) |
| Backend command override | `HERMES_DESKTOP_HERMES` | `HERMES_DESKTOP_BACKEND` |
| Default `HERMES_HOME` on Windows | `%LOCALAPPDATA%\hermes` | `%LOCALAPPDATA%\hermes` |

---

## Troubleshooting

| Symptom | Likely cause | What to do |
| --- | --- | --- |
| `Hermes backend not found…` on startup | No rung resolved | Set `HERMES_DESKTOP_BACKEND` to your `hermes` executable. |
| Window opens, then a boot overlay that never completes | Backend failed to start or never became ready | Read `<HERMES_HOME>/logs/desktop.log`. The spawn error and the child's stderr tail are both there. |
| `Timed out waiting for Hermes backend port announcement` | Cold start slower than 90 s (slow disk, aggressive AV scanning `.pyc`) | Raise `HERMES_DESKTOP_PORT_ANNOUNCE_TIMEOUT_MS`. |
| Boot stalls ~25 s then times out; `desktop.log` shows several `spawning backend` lines, each followed within a second by `stopped backend pid …`, alternating between `serve` and `--profile default serve` | A healthy backend was recycled on every alternating dial. `getConnection()` sends no profile while `getConnection("default")` sends one, and the reuse check compared the raw options — so each spelling looked like a different backend and paid a full cold start | Fixed in `backend/manager.rs::serves_profile`. The recycle path now logs `recycling backend pid … (held profile …, requested …)`; if that line reappears, the two profiles really do differ. |
| `Hermes backend did not become ready: …` | Process bound its port but never answered the health probe | Check the child's stderr in `desktop.log`; the ready timeout is not configurable. |
| `assert-root-install: the desktop build needs …` | Partial root install | `cd <repo-root> && npm ci`. |
| Blank window, no errors | Split `react`/`react-dom` versions (React error #527) | The install guard catches this — reinstall from the root. |
| App uses an empty/blank config | `HERMES_HOME` unset, so the platform default was used instead of a relocated install | Set `HERMES_HOME` explicitly to your real home. |
| First `npm run dev:tauri` takes several minutes | Cargo compiling the dependency graph | Expected; subsequent runs are incremental. |
| `tauri: command not found` | Invoked outside the workspace install | Run `npm run dev:tauri` / `npm run tauri -- …` from `apps/desktop`, after a root `npm install`. |
| `npm error Missing script: "dev:tauri"` | A desktop script was run from the repo root; they only exist in `apps/desktop/package.json` | `cd apps/desktop` first, or pass `-w apps/desktop`. |
| `spawning backend: … os error 3` repeating every second in `desktop.log` | `HERMES_DESKTOP_BACKEND` is a **relative** path; rung 1 is not probed and resolves from the shell's cwd | Set it to an absolute path. |
| Error boundary: `destroy is not a function` | An effect implicit-returned a value from an unwired shell call, so React received a non-function cleanup | Fixed in `src/themes/context.tsx` (`syncNativeTheme`, and the two `ThemeProvider` effects). The bug class recurs on any new `useEffect(() => hermesDesktop.…(…))` body — write those as blocks. |

Reset a wedged state:

```powershell
# Rebuild the Rust binary from scratch
Remove-Item -Recurse -Force .\src-tauri\target
```

```bash
rm -rf src-tauri/target
```

---

## Source map

| Path | Role |
| --- | --- |
| `src/main.rs` | Binary entry; calls `run()`. |
| `src/lib.rs` | `run()` — logging, plugins, `AppState`, command registration, event loop. |
| `src/paths.rs` | Hermes home, venv root, venv bin dir, `desktop.log` path. |
| `src/backend/command.rs` | Backend resolution ladder and argv construction. |
| `src/backend/env.rs` | Child environment and `PATH` construction. |
| `src/backend/manager.rs` | Spawn, port wait, health probe, teardown. |
| `src/backend/ready.rs` | The `HERMES_BACKEND_READY port=` sentinel contract. |
| `src/commands/backend.rs` | `ensure_backend`, connection descriptor, gateway URL. |
| `src/commands/window.rs` | Window creation/control, including `hermes_window_ready`. |
| `src/commands/terminal.rs` | PTY sessions (replaces `node-pty`). |
| `src/commands/fs.rs`, `src/commands/system.rs` | Filesystem/git and system info capabilities. |
| `src/state.rs` | `AppState` — shared HTTP client, backend slot, terminal registry. |
| `src/terminal.rs` | PTY backend for the terminal registry. |
| `src/logging.rs` | `desktop.log` writer. |
| `tauri.conf.json` | Build hooks, window declarations, CSP, bundle targets. |
| `Cargo.toml` | Dependencies and the release profile. |

## Related docs

- [`../README.md`](../README.md) — the desktop app overall (install, updating, connections, projects).
- [`../AGENTS.md`](../AGENTS.md) — engineering invariants for the desktop app.
- [`../DESIGN.md`](../DESIGN.md) — visual system and interaction contract.
- [`../src/AGENTS.md`](../src/AGENTS.md) — backend contract, slash palette, Bot Mode.
- [`../../../AGENTS.md`](../../../AGENTS.md) — repo-wide contribution rules.
