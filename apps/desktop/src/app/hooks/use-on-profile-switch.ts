import { useStore } from '@nanostores/react'
import { useEffect, useRef } from 'react'

import { $activeGatewayProfile } from '@/store/profile'

/** Run `onSwitch` when the active gateway profile changes — never on first
 *  mount. For dropping per-profile view state (probes, cached usage, drafts)
 *  when the backend the app talks to swaps underneath a still-mounted view.
 *
 *  The guard compares against the profile seen on mount rather than a boolean
 *  "first render" flag: React StrictMode double-invokes effects (mount →
 *  cleanup → mount), so a one-shot `first.current` flag is false on the second
 *  invocation and fires `onSwitch` on every boot — which resets consumers'
 *  state (e.g. the settings config draft) right after they seeded it. Comparing
 *  the previous profile value keeps the double-invoke a no-op and only fires
 *  when the profile genuinely changes. */
export function useOnProfileSwitch(onSwitch: () => void): void {
  const profile = useStore($activeGatewayProfile)
  const prev = useRef(profile)

  // eslint-disable-next-line no-restricted-syntax -- legitimate non-atom ref write (see eslint rule comment)
  useEffect(() => {
    if (prev.current !== profile) {
      prev.current = profile
      onSwitch()
    }
    // Fire on profile change only; onSwitch identity is intentionally ignored.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [profile])
}
