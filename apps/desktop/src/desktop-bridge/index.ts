/**
 * Desktop shell bridge — entry point.
 *
 * Imported for its side effect, first thing in `main.tsx`, before any component
 * can touch `window.hermesDesktop`. Under the Electron shell the preload has
 * already installed the object and this is a no-op, so one renderer bundle
 * serves both shells while the Tauri migration lands.
 */

import { installTauriBridge, isTauriRuntime } from './tauri-bridge'

const installed = installTauriBridge()

if (installed) {
  // Mirrors `HERMES_DESKTOP=1` on the Python side: some renderer features gate
  // on "am I in a desktop shell at all" rather than on a specific API.
  ;(window as unknown as { __HERMES_DESKTOP_SHELL__?: string }).__HERMES_DESKTOP_SHELL__ = 'tauri'
} else if (!isTauriRuntime() && !(window as unknown as { hermesDesktop?: unknown }).hermesDesktop) {
  // Plain-browser dev (`vite dev` with no shell): leave `hermesDesktop`
  // undefined so the renderer's existing optional-chaining fallbacks apply.
}

export { installTauriBridge, isTauriRuntime }
