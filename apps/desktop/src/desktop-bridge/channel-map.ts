/**
 * Maps the preload `window.hermesDesktop` surface onto Tauri commands.
 *
 * Why a table and not a name transform: the Electron channel name is not
 * derivable from the method name (`openSessionWindow` →
 * `hermes:window:openSession`), and the argument shapes differ between the two
 * worlds. The preload took positional arguments (`getConnection(profile, opts)`)
 * while a Tauri command takes one named-arguments object. Both facts are
 * encoded here so the renderer keeps its existing call sites.
 *
 * Every entry whose `command` is present on the Rust side is live. A method with
 * no entry is answered by the proxy in `tauri-bridge.ts`, which either returns
 * `undefined` — the surface this shell does not serve, listed in
 * `ABSENT_CAPABILITIES` — or rejects with an explicit "not wired yet" error for a
 * path nobody has classified yet.
 */

export interface ChannelSpec {
  /** Rust command name: the Electron channel with `:` flattening to `_`. */
  command: string
  /** Positional JS args → Tauri's named-arguments object. */
  args?: (args: readonly unknown[]) => Record<string, unknown>
}

/** Pass a value straight through, preserving `null` for `Option<T>`. */
const nullish = (value: unknown): unknown => (value === undefined ? null : value)

export const CHANNEL_MAP: Record<string, ChannelSpec> = {
  // ---------------------------------------------------------------- connection
  getConnection: {
    command: 'hermes_connection',
    args: ([profile, opts]) => ({ profile: nullish(profile), opts: nullish(opts) })
  },
  getConnectionFor: {
    command: 'hermes_connection_for',
    args: ([payload]) => ({ payload: nullish(payload) })
  },
  revalidateConnection: { command: 'hermes_connection_revalidate' },
  touchBackend: {
    command: 'hermes_backend_touch',
    args: ([profile]) => ({ profile: nullish(profile) })
  },
  recycleBackend: {
    command: 'hermes_backend_recycle',
    args: ([profile]) => ({ profile: nullish(profile) })
  },
  getGatewayWsUrl: {
    command: 'hermes_gateway_ws_url',
    args: ([profile]) => ({ profile: nullish(profile) })
  },
  getGatewayWsUrlFor: {
    command: 'hermes_gateway_ws_url_for',
    args: ([payload]) => ({ payload: nullish(payload) })
  },
  getPoolLimits: { command: 'hermes_pool_limits_get' },
  api: {
    command: 'hermes_api',
    args: ([request]) => ({ request })
  },

  // ------------------------------------------------------------------ windows
  /**
   * Not an Electron method: the Tauri main window is created hidden so the user
   * never sees a blank webview, and only the renderer knows when there is
   * something worth showing. `main.tsx` calls this after mount.
   */
  showMainWindow: { command: 'hermes_window_ready' },
  openWindow: { command: 'hermes_window_open_instance' },
  openSessionWindow: {
    command: 'hermes_window_open_session',
    args: ([sessionId, opts]) => ({ sessionId, opts: nullish(opts) })
  },
  openBrowserWindow: {
    command: 'hermes_window_open_browser',
    args: ([tabId]) => ({ tabId: nullish(tabId) })
  },
  getWindowState: { command: 'hermes_window_state' }
}

/**
 * The nested namespaces are grouped objects in the preload, not flat methods, so
 * they get their own tables. These are the wired ones; a namespace with no table
 * here is answered as `undefined` (see `NAMESPACES` in `tauri-bridge.ts`), which
 * is the absence its call sites already probe for.
 */
export const NAMESPACE_MAP: Record<string, Record<string, ChannelSpec>> = {
  settings: {
    getDefaultProjectDir: { command: 'hermes_setting_default_project_dir_get' },
    pickDefaultProjectDir: { command: 'hermes_setting_default_project_dir_pick' },
    /**
     * `null` clears the setting — the Clear button sends it, and so does an
     * argumentless call, matching the preload's `typeof dir === 'string'` test.
     */
    setDefaultProjectDir: {
      command: 'hermes_setting_default_project_dir_set',
      args: ([dir]) => ({ dir: nullish(dir) })
    }
  },
  terminal: {
    attach: { command: 'hermes_terminal_attach', args: ([id]) => ({ id }) },
    cwd: { command: 'hermes_terminal_cwd', args: ([id]) => ({ id }) },
    dispose: { command: 'hermes_terminal_dispose', args: ([id]) => ({ id }) },
    resize: { command: 'hermes_terminal_resize', args: ([id, size]) => ({ id, size: nullish(size) }) },
    start: { command: 'hermes_terminal_start', args: ([options]) => ({ options: nullish(options) }) },
    write: { command: 'hermes_terminal_write', args: ([id, data]) => ({ id, data }) }
  }
}

/** Flat filesystem/system methods, kept together for readability. */
export const FLAT_MAP: Record<string, ChannelSpec> = {
  readDir: { command: 'hermes_fs_read_dir', args: ([dirPath]) => ({ dirPath }) },
  selectPaths: { command: 'hermes_select_paths', args: ([options]) => ({ options: nullish(options) }) },
  selectSavePath: { command: 'hermes_select_save_path', args: ([options]) => ({ options: nullish(options) }) },
  writeClipboard: { command: 'hermes_clipboard_write', args: ([text]) => ({ text }) },
  readClipboard: { command: 'hermes_clipboard_read' },
  gitRoot: { command: 'hermes_fs_git_root', args: ([startPath]) => ({ startPath }) },
  revealPath: { command: 'hermes_fs_reveal', args: ([targetPath]) => ({ targetPath }) },
  openDir: { command: 'hermes_fs_open_dir', args: ([dirPath]) => ({ dirPath }) },
  desktopPluginsRoot: { command: 'hermes_fs_desktop_plugins_root' },
  logsRoot: { command: 'hermes_fs_logs_root' },
  agentPluginsRoot: { command: 'hermes_fs_agent_plugins_root' },
  renamePath: { command: 'hermes_fs_rename', args: ([targetPath, newName]) => ({ targetPath, newName }) },
  writeTextFile: { command: 'hermes_fs_write_text', args: ([filePath, content]) => ({ filePath, content }) },
  trashPath: { command: 'hermes_fs_trash', args: ([targetPath]) => ({ targetPath }) },
  /** Called through `readFileText` and the plugin loader; both want the same shape. */
  readFileText: { command: 'hermes_fs_read_text', args: ([filePath, maxBytes]) => ({ filePath, maxBytes: nullish(maxBytes) }) },
  readPluginSource: { command: 'hermes_fs_read_text', args: ([filePath]) => ({ filePath }) },

  getVersion: { command: 'hermes_version' },
  relaunchApp: { command: 'hermes_app_relaunch' },
  openExternal: { command: 'hermes_open_external', args: ([url]) => ({ url }) },
  notify: { command: 'hermes_notify', args: ([payload]) => ({ payload }) },
  revealLogs: { command: 'hermes_logs_reveal' },
  getRecentLogs: { command: 'hermes_logs_recent' },
  reportRendererError: { command: 'hermes_logs_renderer_error', args: ([report]) => ({ report }) },

  /**
   * Boot progress.
   *
   * Wired rather than declared absent, and the deciding factor is the call site
   * rather than the size of the feature: `useGatewayBoot` reads the snapshot as
   * `desktop.getBootProgress().then(…)` and `DesktopInstallOverlay` reads the
   * other the same way — neither behind a guard, because both were written for
   * the Electron shell where the methods could not be missing. Deleting one makes
   * that call throw a *synchronous* `TypeError` from inside a `useEffect`, which
   * React hands to the root error boundary: the window renders "Something broke
   * in the interface" and the app never mounts.
   *
   * So a shell that cannot answer still has to answer something — `phase: "idle"`
   * for the progress, `active: false` for the installer — and
   * `src-tauri/src/boot.rs` is where both answers live.
   */
  getBootProgress: { command: 'hermes_boot_progress_get' },
  getBootstrapState: { command: 'hermes_bootstrap_state_get' },

  /**
   * First-launch installer actions. These back the `DesktopInstallOverlay`
   * buttons — the overlay probes for the functions before wiring their click
   * handlers, and they are fire-and-forget from the renderer's side, so the
   * result is `{ ok }` (or `{ ok, cancelled }` for Cancel) and nothing more.
   */
  continueBootstrapLocal: { command: 'hermes_bootstrap_start' },
  cancelBootstrap: { command: 'hermes_bootstrap_cancel' },
  resetBootstrap: { command: 'hermes_bootstrap_reset' },
  repairBootstrap: { command: 'hermes_bootstrap_repair' },

  /**
   * Native chrome and power.
   *
   * The preload sends the three setters with `ipcRenderer.send` rather than
   * `invoke`: they are notifies, so the renderer never looks at a result. An
   * unwired one could therefore only ever surface as an unhandled rejection, and
   * `installUnwiredReporter` turns each of those into a toast — which is how
   * `setNativeTheme` on boot and `setKeepAwake` in the settings panel were
   * behaving before these four entries existed.
   */
  setNativeTheme: { command: 'hermes_native_theme_set', args: ([mode]) => ({ mode: nullish(mode) }) },
  setKeepAwake: { command: 'hermes_keep_awake_set', args: ([on]) => ({ on }) },
  getOnBattery: { command: 'hermes_power_battery_get' },
  getRemoteDisplayReason: { command: 'hermes_get_remote_display_reason' }
}
