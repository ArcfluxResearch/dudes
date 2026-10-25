// SPDX-License-Identifier: MIT OR Apache-2.0

import { useRef, useSyncExternalStore } from 'react';

import { clockTime } from '../format.ts';
import { useAutoDismiss } from '../useAutoDismiss.ts';
import { answerBrowser, dismissDeparted, getDevicesState, subscribeToDevices, unansweredDevices } from './devices.ts';
import type { Departed, Device } from './devices.ts';

function NewBrowser({ device }: { readonly device: Device }) {
  return (
    <li className="device-notice">
      <p>A new browser signed in: <strong>{device.name}</strong>, at <span className="device-time">{clockTime(device.signed_in)}</span></p>
      <button type="button" onClick={() => { void answerBrowser(device.id, 'keep'); }}
        aria-label={`Keep ${device.name}`}>Keep</button>
      <button type="button" onClick={() => { void answerBrowser(device.id, 'remove'); }}
        aria-label={`Remove ${device.name}`}>Remove</button>
    </li>
  );
}

function Gone({ notice }: { readonly notice: Departed }) {
  const item = useRef<HTMLLIElement>(null);
  useAutoDismiss(item, notice.key, () => dismissDeparted(notice.key));
  return <li ref={item} className="device-notice device-notice-gone"><p><strong>{notice.name}</strong> {notice.reason}.</p></li>;
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
    <div className="device-notices" aria-live="polite">
      {asking.length === 0 && gone.length === 0 ? null : (
        <ul aria-label="Signed-in browsers">
          {asking.map((device) => <NewBrowser key={device.id} device={device} />)}
          {later > 0 ? <li className="device-notice device-notice-later"><p>{later === 1 ? '1 more browser is' : `${later} more browsers are`} waiting for an answer.</p></li> : null}
          {gone.map((notice) => <Gone key={notice.key} notice={notice} />)}
        </ul>
      )}
    </div>
  );
}
