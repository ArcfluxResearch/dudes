// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useSyncExternalStore } from 'react';
import { desktopNotificationPreference } from './desktopNotifications.ts';
import { handFocusAfterLeave } from './focus.ts';

export function NotificationOffer({ onBoard, leaveFocusTo }: { readonly onBoard: boolean; readonly leaveFocusTo: string }) {
  const visible = useSyncExternalStore(desktopNotificationPreference.subscribe, desktopNotificationPreference.getOffer, desktopNotificationPreference.getOffer);
  const state = useSyncExternalStore(desktopNotificationPreference.subscribe, desktopNotificationPreference.getState, desktopNotificationPreference.getState);
  useEffect(() => {
    if (onBoard && visible) desktopNotificationPreference.presentOffer();
    if (!onBoard) desktopNotificationPreference.hideOffer();
  }, [onBoard, visible]);
  if (!onBoard || !visible) return null;
  return <section className="notification-offer" aria-label="Desktop notifications" ref={handFocusAfterLeave}
    data-focus-on-leave={leaveFocusTo}>
    <p>Show a desktop notification when a ticket is ready or an agent asks something? This browser, while this tab is open.</p>
    <button type="button" disabled={state.kind === 'asking'} onClick={() => { void desktopNotificationPreference.setEnabled(true); }}>
      {state.kind === 'asking' ? 'Asking the browser…' : 'Turn on'}</button>
    <button type="button" onClick={desktopNotificationPreference.dismissOffer}>Not now</button>
    {state.kind === 'unavailable' ? <p role="status">The browser could not turn notifications on. Try again, or use Settings later.</p> : null}
  </section>;
}
