// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useSyncExternalStore } from 'react';
import { desktopNotificationPreference } from './desktopNotifications.ts';
import { handFocusAfterLeave } from './focus.ts';
import { Button } from '../ui/index.ts';
import { NoticeIcon, noticeCard } from './notices/look.tsx';

export function NotificationOffer({ onBoard, leaveFocusTo }: { readonly onBoard: boolean; readonly leaveFocusTo: string }) {
  const visible = useSyncExternalStore(desktopNotificationPreference.subscribe, desktopNotificationPreference.getOffer, desktopNotificationPreference.getOffer);
  const state = useSyncExternalStore(desktopNotificationPreference.subscribe, desktopNotificationPreference.getState, desktopNotificationPreference.getState);
  useEffect(() => {
    if (onBoard && visible) desktopNotificationPreference.presentOffer();
    if (!onBoard) desktopNotificationPreference.hideOffer();
  }, [onBoard, visible]);
  if (!onBoard || !visible) return null;
  return <section className={noticeCard('info', 'notification-offer grid gap-3')} aria-label="Desktop notifications" ref={handFocusAfterLeave}
    data-focus-on-leave={leaveFocusTo}>
    <p className="m-0"><NoticeIcon />Show a desktop notification when a ticket is ready or an agent asks something? This browser, while this tab is open.</p>
    <div className="flex flex-wrap gap-2">
      <Button variant="primary" size="sm" disabled={state.kind === 'asking'} onClick={() => { void desktopNotificationPreference.setEnabled(true); }}>
        {state.kind === 'asking' ? 'Asking the browser…' : 'Turn on'}</Button>
      <Button size="sm" onClick={desktopNotificationPreference.dismissOffer}>Not now</Button>
    </div>
    {state.kind === 'unavailable' ? <p className="m-0 text-small text-muted" role="status">The browser could not turn notifications on. Try again, or use Settings later.</p> : null}
  </section>;
}
