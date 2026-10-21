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
 * reader reads out what is added to it.
 */
export function DeviceNotices() {
  const { devices, departed } = useSyncExternalStore(subscribeToDevices, getDevicesState, getDevicesState);
  const asking = unansweredDevices(devices);
  return (
    <div className="device-notices" aria-live="polite">
      {asking.length === 0 && departed.length === 0 ? null : (
        <ul aria-label="Signed-in browsers">
          {asking.map((device) => <NewBrowser key={device.id} device={device} />)}
          {departed.map((notice) => <Gone key={notice.key} notice={notice} />)}
        </ul>
      )}
    </div>
  );
}
