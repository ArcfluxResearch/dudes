// SPDX-License-Identifier: MIT OR Apache-2.0

import { useRef, useSyncExternalStore } from 'react';

import { clockTime } from '../format.ts';
import { useAutoDismiss } from '../useAutoDismiss.ts';
import { answerBrowser, dismissDeparted, getDevicesState, subscribeToDevices, unansweredDevices } from './devices.ts';
import type { Departed, Device } from './devices.ts';
import { Button, cn } from '../../ui/index.ts';
import { NoticeIcon, noticeCard, noticeLine } from '../notices/look.tsx';

function NewBrowser({ device, banner }: { readonly device: Device; readonly banner: boolean }) {
  return (
    <li className={cn(banner ? 'm-0' : noticeCard('warn'), 'device-notice flex flex-wrap items-center gap-x-3 gap-y-2')}>
      <p className="m-0 min-w-0 flex-[1_1_192px]"><NoticeIcon tone="warn" />A new browser signed in: <strong className="font-semibold">{device.name}</strong>, at <span className="device-time whitespace-nowrap">{clockTime(device.signed_in)}</span></p>
      <div className="flex gap-2">
        <Button size="sm" onClick={() => { void answerBrowser(device.id, 'keep'); }}
          aria-label={`Keep ${device.name}`}>Keep</Button>
        <Button size="sm" onClick={() => { void answerBrowser(device.id, 'remove'); }}
          aria-label={`Remove ${device.name}`}>Remove</Button>
      </div>
    </li>
  );
}

function Gone({ notice }: { readonly notice: Departed }) {
  const item = useRef<HTMLLIElement>(null);
  useAutoDismiss(item, notice.key, () => dismissDeparted(notice.key));
  return <li ref={item} className={cn(noticeCard('info'), 'device-notice device-notice-gone')}><p className="m-0"><strong className="font-semibold">{notice.name}</strong> {notice.reason}.</p></li>;
}

/**
 * A new sign-in waits here, on every open Floor but its own, until someone keeps or removes it. A
 * browser that went is named for a few seconds. The region is drawn even when empty, so a screen
 * reader reads out what is added to it. The Floor draws the waiting sign-ins in its banner, which stays
 * until they are answered, and the departed in its floating stack; `part` picks one, and without it both show.
 * The banner names the first waiting browser and counts the rest, so it stays one line; answering shows the next.
 */
export function DeviceNotices({ part }: { readonly part?: 'asking' | 'gone' } = {}) {
  const { devices, departed } = useSyncExternalStore(subscribeToDevices, getDevicesState, getDevicesState);
  const waiting = part === 'gone' ? [] : unansweredDevices(devices);
  const asking = part === 'asking' ? waiting.slice(0, 1) : waiting;
  const later = waiting.length - asking.length;
  const gone = part === 'asking' ? [] : departed;
  return (
    <div className={cn('device-notices', part === 'asking' && asking.length > 0 && noticeLine('warn'), part !== 'asking' && 'pointer-events-none')} aria-live="polite">
      {asking.length === 0 && gone.length === 0 ? null : (
        <ul className="m-0 grid list-none gap-2 p-0" aria-label="Signed-in browsers">
          {asking.map((device) => <NewBrowser key={device.id} device={device} banner={part === 'asking'} />)}
          {later > 0 ? <li className="device-notice device-notice-later m-0 text-small text-muted"><p className="m-0">{later === 1 ? '1 more browser is' : `${later} more browsers are`} waiting for an answer.</p></li> : null}
          {gone.map((notice) => <Gone key={notice.key} notice={notice} />)}
        </ul>
      )}
    </div>
  );
}
