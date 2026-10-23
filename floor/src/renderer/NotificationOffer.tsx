// SPDX-License-Identifier: MIT OR Apache-2.0

import { useSyncExternalStore } from 'react';
import { desktopNotificationPreference } from './desktopNotifications.ts';

export function NotificationOffer() {
  const visible = useSyncExternalStore(desktopNotificationPreference.subscribe, desktopNotificationPreference.getOffer, desktopNotificationPreference.getOffer);
  const state = useSyncExternalStore(desktopNotificationPreference.subscribe, desktopNotificationPreference.getState, desktopNotificationPreference.getState);
  if (!visible) return null;
  return <section className="notification-offer" aria-label="Ticket notifications">
    <p>Tell me when it&apos;s ready?</p>
    <button type="button" disabled={state.kind === 'asking'} onClick={() => { void desktopNotificationPreference.setEnabled(true); }}>
      {state.kind === 'asking' ? 'Asking the browser…' : 'Yes, tell me'}</button>
    <button type="button" onClick={desktopNotificationPreference.dismissOffer}>Not now</button>
    {state.kind === 'unavailable' ? <p role="status">The browser could not turn notifications on. Try again, or use Settings later.</p> : null}
  </section>;
}
