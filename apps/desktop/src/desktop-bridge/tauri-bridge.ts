/**
 * `window.hermesDesktop`, implemented on Tauri.
 *
 * The renderer's ~240 direct `window.hermesDesktop.*` call sites are the
 * contract; this module satisfies it so no component has to know which shell it
 * runs under. Three deliberate design choices:
 *
 *  1. **The Tauri API loads lazily.** Installing the bridge must not block the
 *     renderer's first paint on a dynamic import, and every method here returns
 *     a promise anyway — so each call awaits a shared `ready` promise before it
 *     touches `invoke`.
 *  2. **A path the preload declares but this shell has no command for is
 *     `undefined`, not a stub.** See `ABSENT_CAPABILITIES` — the renderer's
 *     `?.` probes are the fallback those paths were written for.
 *  3. **A path nobody classified is a callable stub, and loud.** See `stub`.
 *  4. **Namespace shape is declared, not guessed.** See `NAMESPACES`.
 */

import { CHANNEL_MAP, type ChannelSpec, FLAT_MAP, NAMESPACE_MAP } from './channel-map'

type InvokeFn = (command: string, args?: Record<string, unknown>) => Promise<unknown>
type ListenFn = (
  event: string,
  handler: (event: { payload: unknown }) => void
) => Promise<() => void>

let invokeFn: InvokeFn | null = null
let listenFn: ListenFn | null = null
let readyPromise: Promise<void> | null = null

/**
 * Where an unwired API is actually implemented. Pointing the error at the crate
 * root (`lib.rs`'s module docs list the wired surface) is more useful than a
 * bare "not implemented".
 */
const MIGRATION_TARGET = 'apps/desktop/src-tauri (Rust command surface)'

/**
 * The rejection an unwired API produces.
 *
 * A class, not a message match: the reporter below has to pick its own
 * rejections out of everything else the renderer throws, and sniffing for the
 * `[hermes-desktop]` prefix would also catch the bridge's genuine failures —
 * those are bugs, and they deserve their own handling rather than a "not wired
 * yet" toast.
 */
export class UnwiredBridgeError extends Error {
  /** Dotted bridge path that was called (`hud.open`), used to dedupe toasts. */
  readonly bridgePath: string

  constructor(bridgePath: string) {
    super(
      `[hermes-desktop] ${bridgePath} is not wired in the Tauri shell yet — add a command in ` +
        `${MIGRATION_TARGET}, then map it in src/desktop-bridge/channel-map.ts`
    )

    this.name = 'UnwiredBridgeError'
    this.bridgePath = bridgePath
  }
}

/**
 * The Tauri API is loaded on demand so that installing the bridge never blocks
 * the renderer's first paint, and so the Electron shell never fetches a chunk it
 * cannot use.
 *
 * The specifiers are written as literals on purpose. Routing them through a
 * variable (with `@vite-ignore`) would leave a bare `import('@tauri-apps/api/core')`
 * in the bundle, and a browser cannot resolve a bare specifier at runtime — the
 * bridge would fail to initialise in dev *and* in production. As literals, Vite
 * splits them into their own chunk: emitted once, fetched only when a call is
 * actually made.
 */
function ensureApi(): Promise<void> {
  if (!readyPromise) {
    readyPromise = Promise.all([import('@tauri-apps/api/core'), import('@tauri-apps/api/event')]).then(
      ([core, event]) => {
        invokeFn = (core as { invoke: InvokeFn }).invoke
        listenFn = (event as { listen: unknown }).listen as ListenFn
      }
    )
  }

  return readyPromise
}

async function call(spec: ChannelSpec, args: readonly unknown[]): Promise<unknown> {
  await ensureApi()

  if (!invokeFn) {
    throw new Error('[hermes-desktop] the Tauri IPC bridge failed to initialise')
  }

  return invokeFn(spec.command, spec.args ? spec.args(args) : {})
}

/** True when this renderer is running inside the Tauri shell. */
export function isTauriRuntime(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
}

/**
 * Top-level keys `electron/preload.ts` exposes as plain booleans rather than
 * functions.
 *
 * They must resolve to `undefined`, not to a stub: the renderer type-checks
 * them (`typeof x === 'boolean'`) to decide whether to use a native capability
 * or fall back to a UA sniff, and a stub *function* is truthy — which would make
 * `if (hermesDesktop.localModelsEnabled)` pass and advertise local models this
 * shell cannot serve.
 */
const FLAG_KEYS = new Set(['glassSupported', 'translucencySupported', 'localModelsEnabled'])

/**
 * Capabilities this shell does not provide, answered as `undefined` rather than
 * as a stub.
 *
 * A stub is a *function*, and the renderer asks whether a feature exists by
 * asking whether a function does:
 *
 *   typeof hermesDesktop?.hud?.open === 'function'          store/hud.ts
 *   typeof hermesDesktop?.quickEntry?.getSettings === '…'   store/quick-entry.ts
 *   typeof hermesDesktop?.openSessionInTerminal === '…'     store/windows.ts
 *
 * — or by testing the namespace itself (`if (window.hermesDesktop?.zoom)`,
 * `store/zoom.ts`). A stub satisfies every one of those, so the shell ends up
 * advertising what it cannot serve: a HUD toggle that opens no window, an
 * "Open in terminal" item that can only toast an error, and a Quick Entry row
 * whose reads fail. `zoom` is worse — the renderer chains `.then()` onto
 * `zoom.get()` while its module is still being imported, with no rejection
 * handler, so the rejection lands as an unhandled error on every launch.
 *
 * Each call site already guards for absence (`if (!api) return`, `?.`, or the
 * probe itself), so `undefined` is the answer those guards were written for.
 * None of these has a command in `src-tauri/src/commands/`.
 *
 * The native-chrome group below is the mirror image: *notifies* rather than
 * probes, absent for the opposite reason. The renderer asks for nothing there,
 * but their receiver was never ported, and every call site passes the payload
 * through `?.` and drops the result — so `undefined` is a silent no-op, which is
 * what an accepting command would do too except that it would claim the payload
 * went somewhere.
 *
 *   setTranslucency          Only the renderer half is ported. Glass is *live*
 *                            here — `glassSupported` is a flag key above, so
 *                            `GLASS_SUPPORTED` falls back to the UA sniff and
 *                            reads true on Windows — and `applyGlassSurfaces`
 *                            thins the surfaces for real. What is missing is the
 *                            other half: Electron's main process turned the same
 *                            payload into a native window material and opacity
 *                            (`normalizeTranslucency`). Accepting it would claim
 *                            a material nothing applies.
 *   setTitleBarTheme         Electron repaints a *native titlebar overlay*
 *                            (`applyTitleBarOverlay`, for a `titleBarOverlay`
 *                            window). This one is a full native title bar
 *                            (`decorations: true` in tauri.conf.json), whose
 *                            caption `setNativeTheme` already follows.
 *   setActiveWork            Published for Electron's quit guard and its stream
 *                            throttle (`electron/quit-guard.ts`). Neither is
 *                            ported, so accepting it would guard nothing.
 *   setPreviewShortcutActive Gates Electron's `before-input-event` chords. There
 *                            is no pre-input keyboard layer here to gate.
 *   setDisableF12            Electron swallowed F12 in `before-input-event`;
 *                            WebView2's devtools accelerator cannot be
 *                            intercepted, and a packaged build has devtools off.
 *   signalDeepLinkReady      The queue it signals lives in the Electron main
 *                            process — `onDeepLink` is inert here for the same
 *                            reason.
 *   setActiveConnectionRoute Records a per-window route in Electron's connection
 *                            registry. Connections here are resolved per call
 *                            from the profile the renderer passes, so there is no
 *                            registry to record into.
 *
 * Entries are checked *after* the wired maps below, so wiring a command always
 * wins; drop the entry at the same time.
 *
 * `preload coverage` in `tauri-bridge.test.ts` sweeps *every* path the preload
 * declares — namespaces, their members, and the flat methods — and fails unless
 * each one is wired or listed here. That totality is the point, and it is what
 * the earlier sweep over `send`-based methods alone missed: a `send` can only
 * ever reject unhandled, so it toasted on every launch, while an `invoke`-based
 * path looked harmless and instead left a settings panel stuck on a spinner —
 * its author had already written the "unavailable" state, and a stub *function*
 * sailed straight past the capability probe meant to reach it.
 *
 * So the groups below are not a priority order. `src-tauri/README.md` carries the
 * migration status; this list is where "the renderer falls back here" is
 * recorded, and answering `undefined` is what lets that fallback run.
 */
const ABSENT_CAPABILITIES = new Set([
  // ── Windows this shell never opens ─────────────────────────────────────────
  // Electron's main process owns these auxiliary windows and the IPC behind
  // them. This shell creates only the main window, so a toggle would flip a
  // value nothing reads.
  'hud',
  'openSessionInTerminal',
  'petOverlay',
  'quickEntry',
  'wakeIndicator',
  'zoom',

  // ── The connection registry, and the config store behind it ────────────────
  // `hermes serve` is spawned directly here, so there is no registry to record
  // into, no per-connection auth store to write, and nothing that re-resolves a
  // window when the route changes. `GatewaySettings` and
  // `ConnectionsRegistrySection` both bail to their own "unavailable" render on
  // the probe — which is the whole reason these have to be `undefined`.
  'applyConnectionConfig',
  'cloud',
  'connections',
  'getAgentRoster',
  'getConnectionConfig',
  'getProfileRoutes',
  'getSecretStorageEncryption',
  'mcpOauth',
  'oauthLoginConnectionConfig',
  'oauthLogoutConnectionConfig',
  'probeConnectionConfig',
  'saveConnectionConfig',
  'setActiveConnectionRoute',
  'setSecretStorageEncryption',
  'sshConfigHosts',
  'sshResolveHost',
  'testConnectionConfig',

  // ── Profiles ───────────────────────────────────────────────────────────────
  // Switching one rewrites the launch argv and reloads the window, all of it
  // main-process work. `switchProfile` keeps the selection it was given rather
  // than showing a pill the backend does not back.
  'profile',

  // ── File reads, previews, and directory watching ───────────────────────────
  // Data-URL reads, `fs.watch`, screenshot capture and OS drop-target path
  // resolution are all Electron main-process work. `lib/desktop-fs.ts` routes
  // the reads through the backend's own mirror instead; the rest have no
  // counterpart at all.
  'capturePreview',
  'dataUrlReadMax',
  'fetchLinkTitle',
  'getPathForFile',
  'normalizePreviewTarget',
  'openPreviewInBrowser',
  'reachPreviewUrl',
  'readFileDataUrl',
  'readFileDataUrlForAttach',
  'resolveFavicon',
  'stopPreviewFileWatch',
  'watchDirectory',
  'watchPreviewFile',

  // ── Clipboard and image writes ─────────────────────────────────────────────
  'saveClipboardImage',
  'saveGatewayFile',
  'saveImageBuffer',
  'saveImageFromUrl',

  // ── Native menus, find-in-page, and keyboard layers ────────────────────────
  // Electron's native menu roles and its `before-input-event` layer have no
  // WebView2 counterpart to hang these on.
  'contextMenuCopyImage',
  'contextMenuEdit',
  'contextMenuGuestAddWord',
  'contextMenuSpellcheck',
  'findInPage',
  'setDisableF12',
  'setPreviewShortcutActive',
  'stopFindInPage',

  // ── Git, routed around the shell ───────────────────────────────────────────
  // There are no Rust git commands. `lib/desktop-git.ts` falls back to the
  // backend's own /api/git mirror — the same route a remote gateway uses — so
  // the coding rail keeps working; this entry is what lets that fallback see the
  // gap instead of a stub it cannot tell from a working bridge.
  'git',

  // ── Surfaces that are Electron-only products ───────────────────────────────
  'themes',
  'uninstall',
  'updates',

  // ── Native window chrome ───────────────────────────────────────────────────
  // See the per-entry notes above.
  'setActiveWork',
  'setTitleBarTheme',
  'setTranslucency',
  'signalDeepLinkReady',

  // ── Single calls with no home in this shell ────────────────────────────────
  'claimAmbientCue',
  'installDesktopPlugin',
  'probePluginRepo',
  'readWindowBelow',
  'requestMicrophoneAccess',
  'sanitizeWorkspaceCwd',
  'setPoolLimits'
])

/**
 * Every nested object this shell provides.
 *
 * Listed by hand because a name alone cannot tell the bridge whether
 * `hermesDesktop.terminal` is a namespace or a method — and guessing wrong is
 * fatal, not merely incomplete: a stub *function* has no `.start`, so
 * `terminal.start()` throws `undefined is not a function` mid-render.
 *
 * Only the namespaces with a wired member are here. The other eleven the preload
 * declares are in `ABSENT_CAPABILITIES` instead: their members are uniformly
 * unported, and absence is what their call sites already probe for
 * (`if (!window.hermesDesktop?.updates)`, `store/updates.ts`; `if (!bridge)`,
 * `app/settings/connections-registry.tsx`). Answering with an object would defeat
 * every one of those probes.
 */
const NAMESPACES = ['settings', 'terminal']

/**
 * Events the Tauri shell actually emits. Anything absent gets a no-op
 * unsubscribe: a subscription that never fires is the correct behavior for an
 * event the shell no longer produces, and it keeps every `useEffect` cleanup
 * working.
 */
const EVENT_MAP: Record<string, string> = {
  onBackendExit: 'hermes:backend-exit',
  onWindowStateChanged: 'hermes:window-state-changed',
  onBootProgress: 'hermes:boot-progress',
  onBootstrapEvent: 'hermes:bootstrap-event',
  onPreviewFileChanged: 'hermes:preview-file-changed'
}

/** `onFoo` in the preload's own casing convention (`on` + uppercase). */
function isEventHandler(property: string): boolean {
  return property.length > 2 && property.startsWith('on') && property[2] === property[2]?.toUpperCase()
}

function noopUnsubscribe(): () => void {
  return () => undefined
}

function subscribe(eventName: string, callback: (payload: unknown) => void): () => void {
  let disposed = false
  let unlisten: (() => void) | null = null

  void ensureApi().then(async () => {
    if (disposed || !listenFn) {
      return
    }

    const stop = await listenFn(eventName, event => callback(event.payload))

    if (disposed) {
      stop()
    } else {
      unlisten = stop
    }
  })

  return () => {
    disposed = true
    unlisten?.()
  }
}

/**
 * The one property the bridge must never answer with a callable.
 *
 * JavaScript treats any object whose `then` is a function as promise-like: an
 * `async` function that returns the bridge, or `Promise.resolve(bridge)`, or
 * `Promise.all([bridge])`, calls `bridge.then(resolve, reject)` to adopt its
 * state. A stub answers that call with a rejected promise, so the awaiting
 * caller never settles — a hang whose stack points at the `await` and nowhere
 * near here. Answering `undefined` keeps the bridge an ordinary value.
 */
const THEN = 'then'

/**
 * A callable stand-in for an unwired API.
 *
 * The renderer does not know this API is missing, so it will use it in whatever
 * shape the real one had. A stub therefore has to survive all three without
 * throwing:
 *
 *   stub()             → a rejected promise naming the gap
 *   stub.something     → another stub (nested namespaces: `hud.windowing`)
 *   stub.onChange(cb)  → a no-op unsubscribe, not a rejected promise
 *
 * Rejecting (rather than resolving `undefined` silently) is the point: a
 * missing feature must be loud, and inert at runtime. `installUnwiredReporter`
 * turns the rejections nobody awaited into a user-visible toast, while the ones
 * a caller does await stay catchable — `store/windows.ts` relies on that. It
 * never throws synchronously, so it cannot take a module down mid-import.
 */
function stub(path: string): unknown {
  const callable = () => Promise.reject(new UnwiredBridgeError(path))

  return new Proxy(callable, {
    get(target, property) {
      // A stub that looked thenable would make `await hermesDesktop.someThing`
      // call back into the stub instead of settling.
      if (property === THEN) {
        return undefined
      }

      // Symbols (`Symbol.iterator`, `Symbol.toPrimitive`, …) fall to `undefined`
      // so spreading or coercing a stub fails fast instead of looping.
      if (typeof property !== 'string') {
        return undefined
      }

      if (isEventHandler(property)) {
        return noopUnsubscribe
      }

      // Ordinary function plumbing (`name`, `length`, `bind`, `toString`) keeps
      // working; only unknown API names become child stubs.
      if (property in Function.prototype) {
        return Reflect.get(target, property)
      }

      return stub(`${path}.${property}`)
    }
  })
}

/** A namespace object: mapped methods work, everything else is a stub. */
function buildNamespace(namespace: string): Record<string, unknown> {
  const spec = NAMESPACE_MAP[namespace] ?? {}
  const target: Record<string, unknown> = {}

  for (const [method, channelSpec] of Object.entries(spec)) {
    target[method] = (...args: unknown[]) => call(channelSpec, args)
  }

  return new Proxy(target, {
    get(inner, property, receiver) {
      if (typeof property !== 'string') {
        return Reflect.get(inner, property, receiver)
      }

      if (property === THEN) {
        return undefined
      }

      if (Reflect.has(inner, property)) {
        return Reflect.get(inner, property, receiver)
      }

      if (isEventHandler(property)) {
        return noopUnsubscribe
      }

      return stub(`${namespace}.${property}`)
    }
  })
}

function buildBridge(): Record<string, unknown> {
  const target: Record<string, unknown> = {
    /** Set by the preload in Electron; the shell discriminator for the renderer. */
    shell: 'tauri'
  }

  for (const [method, spec] of Object.entries(CHANNEL_MAP)) {
    target[method] = (...args: unknown[]) => call(spec, args)
  }

  for (const [method, spec] of Object.entries(FLAT_MAP)) {
    target[method] = (...args: unknown[]) => call(spec, args)
  }

  for (const namespace of NAMESPACES) {
    target[namespace] = buildNamespace(namespace)
  }

  return new Proxy(target, {
    get(inner, property, receiver) {
      if (typeof property !== 'string') {
        return Reflect.get(inner, property, receiver)
      }

      // Must never answer with a callable — see `THEN` above.
      if (property === THEN) {
        return undefined
      }

      if (Reflect.has(inner, property)) {
        return Reflect.get(inner, property, receiver)
      }

      if (FLAG_KEYS.has(property)) {
        return undefined
      }

      // Checked after the wired maps so that wiring a command later always wins.
      if (ABSENT_CAPABILITIES.has(property)) {
        return undefined
      }

      // Subscriptions: wired events forward to Tauri, the rest are inert.
      if (isEventHandler(property)) {
        const eventName = EVENT_MAP[property]

        return eventName
          ? (callback: (payload: unknown) => void) => subscribe(eventName, callback)
          : noopUnsubscribe
      }

      return stub(property)
    }
  })
}

/**
 * Show unwired-bridge rejections to the user instead of only to the console.
 *
 * Many call sites fire and forget (`void api.open(…)`, `void zoom.get().then(…)`),
 * which leaves the rejection unobserved: an unwired button then fails with
 * nothing but a console line nobody reads, and the click just looks dead. This
 * routes those rejections to the same interrupting error toast every other
 * failure uses, so the app says which feature it cannot serve.
 *
 * Scope is deliberately narrow — only `UnwiredBridgeError` — because an
 * unrelated library rejection is not the bridge's to explain, and because the
 * toast id is derived from the path, so a call repeated on every render
 * collapses into one toast instead of filling the stack. The browser's own
 * console report is left in place (no `preventDefault`): the toast is for
 * whoever is using the app, the stack is for whoever is porting it.
 *
 * `@/store/notifications` is imported lazily on purpose. This module has to
 * finish evaluating before every other import (see `main.tsx`) — a static
 * import would drag the app graph, i18n included, in ahead of it. Anything
 * worth reporting happens long after boot, so the module is always warm by then.
 *
 * Reaching this handler at all is the scope: a call site that awaits and
 * catches (`store/windows.ts` does) has handled its rejection, so no
 * `unhandledrejection` fires and nothing is reported. What lands here is the
 * fire-and-forget set — the calls whose failure nobody sees. That is exactly
 * the set worth both a toast and a log line.
 *
 * The toast and the log line are separate audiences. The toast is for whoever
 * is using the app right now. The log line is for whoever is told *afterwards*
 * that a button does nothing, with the console long gone — which is the
 * situation this shell kept producing and had no answer for.
 */
function installUnwiredReporter(): void {
  window.addEventListener('unhandledrejection', event => {
    const reason: unknown = event.reason

    if (!(reason instanceof UnwiredBridgeError)) {
      return
    }

    logUnwiredCall(reason)

    void import('@/store/notifications')
      .then(({ notify }) =>
        notify({
          id: `bridge-unwired:${reason.bridgePath}`,
          kind: 'error',
          message: reason.message
        })
      )
      .catch(() => undefined)
  })
}

/**
 * Bridge paths already written to `desktop.log` this session.
 *
 * One line per path, not one per call. A path that repeats on every render would
 * otherwise append a line per frame and bury everything else in the file. The
 * useful shape is the *set* of features this shell cannot serve — a worklist —
 * and that is what a single line each produces.
 */
const loggedUnwiredPaths = new Set<string>()

/**
 * Record an unwired call in `desktop.log`, once per path.
 *
 * Reuses `FLAT_MAP.reportRendererError` rather than adding a command: that is
 * the renderer's existing route into the shell's log, and the line lands in
 * `errors.log` as well, which is where a bug report gets triaged. The level is
 * right — a user-visible feature is dead, not misbehaving.
 *
 * The entry is read out of the map, so nothing here can drift from the wiring
 * the renderer's own error boundary uses. If its key is ever renamed the
 * reference goes undefined and the contract test fails, rather than the log
 * silently going quiet.
 *
 * No stack. The path already names what to wire and `channel-map.ts` is where it
 * gets wired; the frames above the stub only say which component asked, and the
 * console still carries them (the report is not `preventDefault`ed).
 *
 * Fire-and-forget: a diagnostic that can itself throw is worse than a missing
 * line, and `call` rejects rather than throws if the API never loaded.
 */
function logUnwiredCall(reason: UnwiredBridgeError): void {
  if (loggedUnwiredPaths.has(reason.bridgePath)) {
    return
  }

  loggedUnwiredPaths.add(reason.bridgePath)

  // The prefix is the grep: `unwired bridge call:` lists every dead feature.
  void call(FLAT_MAP.reportRendererError, [
    { message: `unwired bridge call: ${reason.bridgePath}` }
  ]).catch(() => undefined)
}

/**
 * Install the bridge. Returns false (and changes nothing) when the Electron
 * preload already provided `hermesDesktop`, so the same renderer bundle works
 * under both shells during the migration.
 */
export function installTauriBridge(): boolean {
  if (typeof window === 'undefined') {
    return false
  }

  const holder = window as unknown as { hermesDesktop?: unknown }

  if (holder.hermesDesktop) {
    return false
  }

  if (!isTauriRuntime()) {
    return false
  }

  holder.hermesDesktop = buildBridge()
  installUnwiredReporter()

  return true
}
