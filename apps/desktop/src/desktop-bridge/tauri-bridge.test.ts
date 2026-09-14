import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

import { afterEach, describe, expect, it, vi } from 'vitest'

import { CHANNEL_MAP, FLAT_MAP, NAMESPACE_MAP } from './channel-map'

/**
 * These tests pin the invariants that keep an unwired API from taking the window
 * down. They are contracts, not snapshots: each one fails if the bridge starts
 * throwing where the renderer expects a value, or stops covering a namespace.
 */

/**
 * Resolved from the working directory rather than `import.meta.url`: under the
 * jsdom environment `import.meta.url` is an `http://` URL, and `fileURLToPath`
 * rejects anything that is not a `file:` URL. Vitest runs with the package root
 * as cwd, which is the same directory this path is relative to.
 */
const PRELOAD_PATH = resolve(process.cwd(), 'electron/preload.ts')

/**
 * A bridge path the preload does not declare, standing in for the next method it
 * grows before anyone classifies it.
 *
 * The sweeps in `preload coverage` leave every *declared* path either wired or
 * absent, so this is the only kind of path that can still reject — which is
 * exactly the case `installUnwiredReporter` exists for, and what keeps the
 * reporter's own tests from going vacuous.
 */
const UNCLASSIFIED = 'someMethodAddedToThePreloadLater'

/** Every path the wired maps answer — half of the classification. */
const WIRED_FLAT = new Set([...Object.keys(CHANNEL_MAP), ...Object.keys(FLAT_MAP)])

/**
 * The nested objects `preload.ts` exposes inside `exposeInMainWorld`: two-space
 * indentation followed by an object literal. Flat methods (`foo: () => …`) and
 * the flags (`glassSupported: …`) do not match.
 */
function preloadNamespaces(): string[] {
  const source = readFileSync(PRELOAD_PATH, 'utf8')

  return [...source.matchAll(/^ {2}([A-Za-z][A-Za-z0-9]*): \{$/gm)].map(match => match[1])
}

/**
 * The preload's flat members — two-space `name: (…) => …` entries.
 *
 * Namespaces (`name: {`) are excluded because they carry a brace before the
 * arrow, and so are the plain booleans the preload exposes as flags, which have
 * no arrow at all.
 */
function preloadFlatMethods(): string[] {
  const source = readFileSync(PRELOAD_PATH, 'utf8')

  return [...source.matchAll(/^ {2}([A-Za-z][A-Za-z0-9]*): [^\n{]*=>/gm)].map(match => match[1])
}

/** The members of one namespace block, by the preload's own indentation. */
function preloadNamespaceMembers(namespace: string): string[] {
  const lines = readFileSync(PRELOAD_PATH, 'utf8').split('\n')
  const start = lines.findIndex(line => line === `  ${namespace}: {`)

  if (start === -1) {
    return []
  }

  const members: string[] = []

  for (const line of lines.slice(start + 1)) {
    // The block ends at the first brace back at the namespace's own indent.
    if (/^ {2}\},?$/.test(line)) {
      break
    }

    const member = /^ {4}([A-Za-z][A-Za-z0-9]*):/.exec(line)

    if (member) {
      members.push(member[1])
    }
  }

  return members
}

/**
 * `onFoo` — the preload's subscription convention (`on` + an uppercase letter),
 * the same test the bridge makes.
 *
 * A subscription is answered with an unsubscribe function, never with
 * `undefined`, so a sweep that ignored them would demand the wrong answer.
 */
function preloadEventName(name: string): boolean {
  return name.length > 2 && name.startsWith('on') && name[2] === name[2]?.toUpperCase()
}

/** Install a fresh bridge and hand back the global it produced. */
async function installBridge(): Promise<Record<string, any>> {
  vi.resetModules()

  const { installTauriBridge } = await import('./tauri-bridge')

  expect(installTauriBridge()).toBe(true)

  return (window as any).hermesDesktop
}

afterEach(() => {
  delete (window as any).__TAURI_INTERNALS__
  delete (window as any).hermesDesktop
})

describe('installTauriBridge', () => {
  it('leaves the Electron preload in charge when one is already installed', async () => {
    const existing = { fromElectron: true }

    ;(window as any).hermesDesktop = existing
    ;(window as any).__TAURI_INTERNALS__ = {}

    vi.resetModules()
    const { installTauriBridge } = await import('./tauri-bridge')

    expect(installTauriBridge()).toBe(false)
    expect((window as any).hermesDesktop).toBe(existing)
  })

  it('stays out of the way outside a desktop shell', async () => {
    vi.resetModules()
    const { installTauriBridge } = await import('./tauri-bridge')

    expect(installTauriBridge()).toBe(false)
    // Left undefined so the renderer's existing optional-chaining fallbacks run.
    expect((window as any).hermesDesktop).toBeUndefined()
  })
})

describe('unwired surface', () => {
  it('answers a path nobody classified with a rejected promise, not a throw', async () => {
    ;(window as any).__TAURI_INTERNALS__ = {}
    const bridge = await installBridge()

    // The net for the next method the preload grows. The bridge cannot know it
    // exists, so it stays callable and loud rather than silently inert — a
    // missing feature must be visible — while `preload coverage` below is where
    // the classification itself is enforced.
    //
    // Assigning before awaiting is the assertion: a synchronous throw would fail
    // the test here. Wrapping the call in `expect(() => …).not.toThrow()` instead
    // would discard the rejected promise and leave an unhandled rejection behind.
    const pending = bridge[UNCLASSIFIED]()

    expect(pending).toBeInstanceOf(Promise)
    await expect(pending).rejects.toThrow(/not wired/)
  })

  it('survives a path the renderer walks two levels deep', async () => {
    ;(window as any).__TAURI_INTERNALS__ = {}
    const bridge = await installBridge()

    // `terminal.start` is a real member, and a namespace nobody classified has to
    // answer the same way at depth: `store/pet-overlay.ts` walks
    // `hermesDesktop.petOverlay.open(…)` while its module is still being
    // imported, so a throw there kills the window before it ever renders.
    const pending = bridge[UNCLASSIFIED].nested()

    await expect(pending).rejects.toThrow(/not wired/)
  })

  it('reports capability flags as undefined so the renderer keeps its fallbacks', async () => {
    ;(window as any).__TAURI_INTERNALS__ = {}
    const bridge = await installBridge()

    // `store/translucency.ts` branches on `typeof x === 'boolean'` and falls
    // back to a UA sniff. A stub *function* would be neither a boolean nor
    // falsy, so `if (hermesDesktop.localModelsEnabled)` would advertise local
    // models this shell cannot serve.
    expect(bridge.glassSupported).toBeUndefined()
    expect(bridge.translucencySupported).toBeUndefined()
    expect(bridge.localModelsEnabled).toBeUndefined()
  })

  it('leaves out capabilities the shell cannot serve, so the renderer hides them', async () => {
    ;(window as any).__TAURI_INTERNALS__ = {}
    const bridge = await installBridge()

    // The renderer asks "does this shell have X?" by asking whether a function is
    // there (`store/windows.ts`, `store/hud.ts`, `store/quick-entry.ts`) or
    // whether the namespace is (`if (window.hermesDesktop?.zoom)`,
    // `store/zoom.ts`). Because a stub IS a function, an unimplemented
    // capability has to be *absent* — otherwise the shell advertises a HUD
    // toggle that opens no window, an "Open in terminal" item that can only
    // toast an error, and a Quick Entry row whose reads fail. `zoom` is worse
    // still: the renderer chains `.then()` onto `zoom.get()` at import time with
    // no rejection handler, so stubbing it raises an unhandled error every
    // launch.
    expect(bridge.hud).toBeUndefined()
    expect(bridge.openSessionInTerminal).toBeUndefined()
    expect(bridge.quickEntry).toBeUndefined()
    expect(bridge.zoom).toBeUndefined()

    // The pair reported from Settings. `GatewaySettings` renders its own
    // "unavailable" panel on exactly this probe, and the stub it used to find
    // there is why the panel sat empty behind a toast instead.
    expect(bridge.connections).toBeUndefined()
    expect(bridge.getConnectionConfig).toBeUndefined()

    // The identical probes, spelled the way their call sites spell them.
    expect(typeof bridge.hud?.open).toBe('undefined')
    expect(typeof bridge.quickEntry?.getSettings).toBe('undefined')
    expect(typeof bridge.connections?.list).toBe('undefined')
    expect(typeof bridge.getConnectionConfig?.()).toBe('undefined')

    // Capabilities this shell *does* provide must still answer as functions, or
    // the same probes would hide working features.
    expect(typeof bridge.openWindow).toBe('function')
    expect(typeof bridge.openSessionWindow).toBe('function')
    expect(typeof bridge.terminal.start).toBe('function')
  })

  it('is not mistaken for a promise', async () => {
    ;(window as any).__TAURI_INTERNALS__ = {}
    const bridge = await installBridge()

    // An object whose `then` is a function is promise-like: returning the bridge
    // from an `async` function, or handing it to `Promise.resolve`, makes the
    // runtime call `then(resolve, reject)` to adopt its state. A stub would
    // answer with a rejected promise, so the awaiting caller would never settle.
    // `installBridge` above already returns the bridge from an async function —
    // this assertion names the reason it works.
    expect(bridge.then).toBeUndefined()
    expect(bridge[UNCLASSIFIED].then).toBeUndefined()

    expect(await Promise.resolve(bridge)).toBe(bridge)
  })

  it('hands back a working unsubscribe for events the shell does not emit', async () => {
    ;(window as any).__TAURI_INTERNALS__ = {}
    const bridge = await installBridge()

    // A rejected promise here would turn every `useEffect` cleanup into an
    // unhandled rejection.
    const stop = bridge.onSomethingThisShellNeverEmits(() => {})

    expect(typeof stop).toBe('function')
    expect(() => stop()).not.toThrow()
  })
})

describe('wired surface', () => {
  it('exposes mapped methods and namespaces as callables', async () => {
    ;(window as any).__TAURI_INTERNALS__ = {}
    const bridge = await installBridge()

    expect(typeof bridge.readDir).toBe('function')
    expect(typeof bridge.writeClipboard).toBe('function')
    expect(typeof bridge.getConnection).toBe('function')
    expect(typeof bridge.terminal.start).toBe('function')
    expect(typeof bridge.showMainWindow).toBe('function')
  })

  it('routes the default-project-dir setting to the commands Rust implements', async () => {
    // Tauri's own `mockIPC` swaps this in the same way; `@tauri-apps/api` reads
    // `window.__TAURI_INTERNALS__.invoke` per call, so it observes the real path.
    const invoke = vi.fn(async (..._args: unknown[]) => null)

    ;(window as any).__TAURI_INTERNALS__ = { invoke }

    const bridge = await installBridge()

    await bridge.settings.getDefaultProjectDir()
    await bridge.settings.pickDefaultProjectDir()
    await bridge.settings.setDefaultProjectDir(null)

    // The three exist on the Rust side and satisfy the renderer's contract in
    // `src/global.d.ts`; before this they were unmapped, so `Settings → Sessions`
    // could neither read the setting nor clear it.
    //
    // `null` is what the Clear button sends, and it has to arrive as `null`: the
    // command takes `Option<String>`, so a dropped argument would leave the
    // setting untouched instead of cleared.
    //
    // Only the command and its arguments are asserted — `@tauri-apps/api` always
    // forwards a third `options` argument, `undefined` when there is none.
    expect(invoke.mock.calls.map(([command, args]) => [command, args])).toEqual([
      ['hermes_setting_default_project_dir_get', {}],
      ['hermes_setting_default_project_dir_pick', {}],
      ['hermes_setting_default_project_dir_set', { dir: null }]
    ])
  })

  it('routes the native chrome and power calls to the commands Rust implements', async () => {
    const invoke = vi.fn(async (..._args: unknown[]) => null)

    ;(window as any).__TAURI_INTERNALS__ = { invoke }

    const bridge = await installBridge()

    await bridge.setNativeTheme('dark')
    await bridge.setKeepAwake(true)
    await bridge.getOnBattery()
    await bridge.getRemoteDisplayReason()

    // `mode` is forwarded as the renderer's own literal rather than translated
    // here: the command is what knows that "follow the OS" is the *absence* of a
    // theme, so that mapping lives in one place.
    //
    // `on` is the positional boolean in this group, and it has to arrive named —
    // Tauri deserializes an object, and a parameter that is not `Option` fails on
    // `undefined` instead of reading as false.
    //
    // Only the command and its arguments are asserted — `@tauri-apps/api` always
    // forwards a third `options` argument, `undefined` when there is none.
    expect(invoke.mock.calls.map(([command, args]) => [command, args])).toEqual([
      ['hermes_native_theme_set', { mode: 'dark' }],
      ['hermes_keep_awake_set', { on: true }],
      ['hermes_power_battery_get', {}],
      ['hermes_get_remote_display_reason', {}]
    ])
  })
})

describe('preload coverage', () => {
  /**
   * The whole preload surface, swept against the bridge. Three tests rather than
   * one because the three shapes answer differently — a flat method, a namespace,
   * and a member of one — and a single sweep would have to guess which it had.
   *
   * Every one of them derives its expectations from `electron/preload.ts` and the
   * wired maps rather than from a pinned list, so the invariant is total: a path
   * the preload grows cannot be silently answered with a stub, and a path the
   * shell stops serving cannot be silently left wired. It also means adding a
   * method to the preload is all it takes to make one of these fail loudly here
   * instead of toasting in the app.
   */

  it('answers every flat preload method with a command or with nothing at all', async () => {
    ;(window as any).__TAURI_INTERNALS__ = {}
    const bridge = await installBridge()
    const methods = preloadFlatMethods()

    // Guards the sweep against passing vacuously if the pattern stops matching —
    // the failure mode that hid this bug the first time.
    expect(methods).toContain('readDir')
    expect(methods).toContain('getConnectionConfig')

    // `typeof` cannot separate a stub from a wired method: both are functions.
    // The wired map is what separates them, and the third answer — callable yet
    // unmapped — is the bug this sweep exists for. The caller cannot catch the
    // rejection, so the bridge reports a dead feature and toasts; that is how
    // `setKeepAwake` behaved on every visit to Settings, and how
    // `getConnectionConfig` left the gateway panel empty behind one.
    const stubs = methods.filter(
      name => !preloadEventName(name) && !WIRED_FLAT.has(name) && bridge[name] !== undefined
    )

    expect(stubs).toEqual([])

    // The other direction, which a sweep that only looked for stubs would miss: a
    // path in a wired map has to be callable, or the renderer's `?.()` would
    // guard itself out of a feature the shell actually serves.
    const notCallable = methods.filter(name => WIRED_FLAT.has(name) && typeof bridge[name] !== 'function')

    expect(notCallable).toEqual([])
  })

  it('answers every preload namespace with an object or with nothing at all', async () => {
    ;(window as any).__TAURI_INTERNALS__ = {}
    const bridge = await installBridge()
    const namespaces = preloadNamespaces()

    // Guards the sweep against passing vacuously if the pattern stops matching.
    expect(namespaces).toContain('terminal')

    // Omitted rather than stubbed, so the probe reaches the fallback its author
    // wrote: `ConnectionsRegistrySection` renders `null` on `if (!bridge)`,
    // `store/updates.ts` returns its cached status, and `store/zoom.ts` keeps its
    // own default instead of chaining `.then()` onto a rejection it cannot catch.
    const omitted = namespaces.filter(name => bridge[name] === undefined)

    expect(omitted.sort()).toEqual([
      'cloud',
      'connections',
      'dataUrlReadMax',
      'git',
      'hud',
      'mcpOauth',
      'petOverlay',
      'profile',
      'quickEntry',
      'themes',
      'uninstall',
      'updates',
      'wakeIndicator',
      'zoom'
    ])

    const notObjects = namespaces.filter(
      name => bridge[name] !== undefined && (typeof bridge[name] !== 'object' || bridge[name] === null)
    )

    expect(notObjects).toEqual([])
  })

  it('leaves no member of a provided namespace on a stub', async () => {
    ;(window as any).__TAURI_INTERNALS__ = {}
    const bridge = await installBridge()

    // Non-vacuity, and it matters more here than in the two sweeps above: the
    // members are what the renderer actually calls.
    expect(preloadNamespaceMembers('terminal')).toContain('start')

    const stubs: string[] = []

    for (const namespace of preloadNamespaces()) {
      const provided = bridge[namespace]

      if (provided === undefined) {
        continue
      }

      const wired = new Set(Object.keys(NAMESPACE_MAP[namespace] ?? {}))

      for (const member of preloadNamespaceMembers(namespace)) {
        if (!preloadEventName(member) && !wired.has(member) && provided[member] !== undefined) {
          stubs.push(`${namespace}.${member}`)
        }
      }
    }

    expect(stubs).toEqual([])
  })
})

describe('startup path', () => {
  it('lets a module that probes the bridge at import time keep its own default', async () => {
    // The pristine default, with no bridge in sight.
    vi.resetModules()
    const pristine = (await import('../store/zoom')).$zoomPercent.get()

    ;(window as any).__TAURI_INTERNALS__ = {}
    await installBridge()

    // `store/zoom.ts` reads the bridge while it is still being imported and
    // chains `.then()` onto `zoom.get()` with no rejection handler — so a stub
    // there rejects into nothing, an unhandled error on every launch, and the
    // UI is briefly fed a value the shell never measured. Finding `zoom` absent,
    // the module skips the probe and keeps the default its own comment promises
    // (no flash of 100% before the main process answers).
    const { $zoomPercent } = await import('../store/zoom')

    await new Promise(resolve => setTimeout(resolve, 0))

    expect($zoomPercent.get()).toBe(pristine)
  })
})

describe('unwired rejection reporting', () => {
  /**
   * Install a bridge and hand back the handler it registered for the browser's
   * unhandled-rejection signal.
   *
   * Driven directly rather than provoked with a genuinely unhandled rejection.
   * jsdom tracks unhandled rejections through its own `Promise`, and this realm's
   * promises are Node's, so one created here never reaches `window` — it surfaces
   * on Vitest's process handler, which (correctly) fails the run. Chromium keeps
   * no such split: the rejection the renderer drops is the rejection the window
   * hears. So this drives the bridge's half through the handler it registered,
   * and asserting the handler exists is itself the wiring check.
   */
  async function installBridgeCapturingRejections(): Promise<{
    bridge: Record<string, any>
    report: (reason: unknown) => void
  }> {
    const spy = vi.spyOn(window, 'addEventListener')

    try {
      // `mockRestore` clears `mock.calls` as well as putting the original back,
      // so the handler has to be read out before the `finally` runs.
      const bridge = await installBridge()
      const registered = spy.mock.calls.find(([type]) => type === 'unhandledrejection')?.[1]

      expect(registered).toBeTypeOf('function')

      return {
        bridge,
        report: (reason: unknown) => (registered as (event: unknown) => void)({ reason })
      }
    } finally {
      spy.mockRestore()
    }
  }

  /**
   * The writes the bridge made to `desktop.log`, as `invoke` saw them.
   *
   * Filtered by command rather than asserted over the whole call list: the
   * renderer-error channel is already the shell's log route, so pinning its
   * command name here also pins that the bridge reuses it instead of inventing
   * a second route.
   */
  function loggedWrites(invoke: { mock: { calls: unknown[][] } }): unknown[][] {
    return invoke.mock.calls.filter(([command]) => command === 'hermes_logs_renderer_error')
  }

  /**
   * The messages those writes carried, in order.
   *
   * Reading `.report.message` rather than asserting the payload wholesale is
   * what pins the wire shape against the Rust side: a renamed key would make
   * this throw instead of quietly reading `undefined`.
   */
  function loggedMessages(invoke: { mock: { calls: unknown[][] } }): string[] {
    return loggedWrites(invoke).map(
      ([, args]) => (args as { report: { message: string } }).report.message
    )
  }

  it('turns an unwired rejection into a user-visible error toast', async () => {
    ;(window as any).__TAURI_INTERNALS__ = {}
    const { bridge, report } = await installBridgeCapturingRejections()
    const { $notifications, clearNotifications } = await import('../store/notifications')

    clearNotifications()

    // The bridge's own rejection, from a path the preload does not declare: this
    // is what proves the stub's error is still recognizable after it crosses into
    // the reporter, and that the toast names the path a reader needs to classify.
    report(await bridge[UNCLASSIFIED].open({}).catch((error: unknown) => error))

    await vi.waitFor(() => expect($notifications.get()).toHaveLength(1), { timeout: 5_000 })

    const [toast] = $notifications.get()

    expect(toast.kind).toBe('error')
    expect(toast.message).toContain(`${UNCLASSIFIED}.open is not wired`)
  })

  it('collapses repeats of one path into a single toast', async () => {
    ;(window as any).__TAURI_INTERNALS__ = {}
    const { bridge, report } = await installBridgeCapturingRejections()
    const { $notifications, clearNotifications } = await import('../store/notifications')

    clearNotifications()

    // A call repeated on every render must not stack toasts up the screen.
    report(await bridge[UNCLASSIFIED].open({}).catch((error: unknown) => error))
    report(await bridge[UNCLASSIFIED].open({}).catch((error: unknown) => error))

    await vi.waitFor(() => expect($notifications.get()).toHaveLength(1), { timeout: 5_000 })
  })

  it('leaves a rejection that is not the bridge own alone', async () => {
    const invoke = vi.fn(async (..._args: unknown[]) => null)

    ;(window as any).__TAURI_INTERNALS__ = { invoke }

    const { bridge, report } = await installBridgeCapturingRejections()
    const { $notifications, clearNotifications } = await import('../store/notifications')

    // Warm the reporter's lazy import first. Once the notification module is
    // resolved its `import()` settles within a microtask, so the macrotask flush
    // below is enough to make the silence that follows meaningful instead of
    // merely early. The warm-up is the bridge's own rejection, which is also what
    // gives the log assertion below something to hold against: it proves the log
    // was live and chose to stay quiet, rather than not having had a chance.
    report(await bridge[UNCLASSIFIED].open({}).catch((error: unknown) => error))
    await vi.waitFor(() => expect($notifications.get()).toHaveLength(1), { timeout: 5_000 })
    await vi.waitFor(() => expect(loggedWrites(invoke)).toHaveLength(1), { timeout: 5_000 })

    clearNotifications()
    report(new Error('boom'))

    await new Promise(resolve => setTimeout(resolve, 0))

    expect($notifications.get()).toEqual([])
    // An unrelated rejection is not the bridge's to explain — in the log either.
    expect(loggedWrites(invoke)).toHaveLength(1)
  })

  it('writes an unwired call to desktop.log, where the console will not be', async () => {
    const invoke = vi.fn(async (..._args: unknown[]) => null)

    ;(window as any).__TAURI_INTERNALS__ = { invoke }

    const { bridge, report } = await installBridgeCapturingRejections()

    report(await bridge[UNCLASSIFIED].open({}).catch((error: unknown) => error))

    // The toast is gone the moment the app closes; this line is not. It is what
    // turns "the button does nothing" from an unanswerable bug report into a
    // grep — the situation this shell kept producing with no answer for it.
    await vi.waitFor(() => expect(loggedWrites(invoke)).toHaveLength(1), { timeout: 5_000 })

    // The command name is the existing renderer → log route, not a second one,
    // and the message pins the prefix the grep depends on.
    //
    // Only the command and its arguments are asserted — `@tauri-apps/api` always
    // forwards a third `options` argument, `undefined` when there is none.
    expect(loggedWrites(invoke).map(([command, args]) => [command, args])).toEqual([
      [
        'hermes_logs_renderer_error',
        { report: { message: `unwired bridge call: ${UNCLASSIFIED}.open` } }
      ]
    ])
  })

  it('writes one line per path, however often that path is called', async () => {
    const invoke = vi.fn(async (..._args: unknown[]) => null)

    ;(window as any).__TAURI_INTERNALS__ = { invoke }

    const { bridge, report } = await installBridgeCapturingRejections()

    report(await bridge[UNCLASSIFIED].open({}).catch((error: unknown) => error))
    await vi.waitFor(() => expect(loggedWrites(invoke)).toHaveLength(1), { timeout: 5_000 })

    // The same path again, then a sibling one. The repeat must add nothing — the
    // log is a worklist of dead features, not a call count, and a path that
    // repeats on every render would otherwise bury the whole file.
    //
    // The sibling is what keeps that silence honest: asserting the count reached
    // 2 proves the second write was possible, so the repeat's absence above is a
    // decision and not a race.
    report(await bridge[UNCLASSIFIED].open({}).catch((error: unknown) => error))
    report(await bridge[UNCLASSIFIED].close({}).catch((error: unknown) => error))

    await vi.waitFor(() => expect(loggedWrites(invoke)).toHaveLength(2), { timeout: 5_000 })

    expect(loggedMessages(invoke)).toEqual([
      `unwired bridge call: ${UNCLASSIFIED}.open`,
      `unwired bridge call: ${UNCLASSIFIED}.close`
    ])
  })
})
