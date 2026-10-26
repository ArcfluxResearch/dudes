// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useSyncExternalStore } from 'react';
import { desktopNotificationPreference } from './desktopNotifications.ts';
import { handFocusAfterLeave } from './focus.ts';
import { Button } from '../ui/index.ts';

export function NotificationOffer({ onBoard, leaveFocusTo }: { readonly onBoard: boolean; readonly leaveFocusTo: string }) {
  const visible = useSyncExternalStore(desktopNotificationPreference.subscribe, desktopNotificationPreference.getOffer, desktopNotificationPreference.getOffer);
  const state = useSyncExternalStore(desktopNotificationPreference.subscribe, desktopNotificationPreference.getState, desktopNotificationPreference.getState);
  useEffect(() => {
    if (onBoard && visible) desktopNotificationPreference.presentOffer();
    if (!onBoard) desktopNotificationPreference.hideOffer();
  }, [onBoard, visible]);
  if (!onBoard || !visible) return null;
  return <section className="notification-offer box-border grid w-full max-w-full grid-cols-[minmax(0,1fr)_auto_auto] items-center gap-2 rounded-lg border border-line bg-surface py-2 pr-2 pl-3 font-sans text-base text-ink shadow-pop max-[800px]:flex max-[800px]:flex-wrap" aria-label="Desktop notifications" ref={handFocusAfterLeave}
    data-focus-on-leave={leaveFocusTo}>
    <p className="m-0 max-[800px]:basis-full">Show a desktop notification when a ticket is ready or an agent asks something? This browser, while this tab is open.</p>
    <Button variant="primary" size="sm" disabled={state.kind === 'asking'} onClick={() => { void desktopNotificationPreference.setEnabled(true); }}>
      {state.kind === 'asking' ? 'Asking the browser…' : 'Turn on'}</Button>
    <Button variant="ghost" size="sm" onClick={desktopNotificationPreference.dismissOffer}>Not now</Button>
    {state.kind === 'unavailable' ? <p className="col-span-full m-0 text-small text-muted" role="status">The browser could not turn notifications on. Try again, or use Settings later.</p> : null}
  </section>;
}
