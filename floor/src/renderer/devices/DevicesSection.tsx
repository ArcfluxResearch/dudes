// SPDX-License-Identifier: MIT OR Apache-2.0

import { useSyncExternalStore } from 'react';

import { recordedTime } from '../format.ts';
import { getDevicesState, answerBrowser, subscribeToDevices } from './devices.ts';
import { signOutHere } from './signOut.ts';
import type { Device } from './devices.ts';

function when(at: number | string | null): string {
  return at === null ? 'not yet' : recordedTime(at);
}

function Row({ device }: { readonly device: Device }) {
  return (
    <li className="device-row">
      <h4>{device.name}{device.this ? <span className="device-this"> This browser</span> : null}</h4>
      <dl>
        <div><dt>Signed in</dt><dd>{when(device.signed_in)}</dd></div>
        <div><dt>First used</dt><dd>{when(device.first_used)}</dd></div>
        <div><dt>Last used</dt><dd>{when(device.last_used)}</dd></div>
      </dl>
      <button type="button" aria-label={`Sign out ${device.name}${device.this ? ' (this browser)' : ''}`}
        onClick={() => { void (device.this ? signOutHere() : answerBrowser(device.id, 'remove')); }}>Sign out</button>
    </li>
  );
}

/** Every browser signed in to this Floor, with a way to end each one. */
export function DevicesSection() {
  const { devices, loaded, signOutFailed } = useSyncExternalStore(subscribeToDevices, getDevicesState, getDevicesState);
  return (
    <section aria-labelledby="settings-browsers"><h3 id="settings-browsers">Signed-in browsers</h3>
      <p>A browser stays signed in for 30 days without use and 90 days in all. Signing one out here ends it at once.</p>
      {!loaded ? <p role="status">Reading the signed-in browsers.</p> : <ul className="device-list">
        {devices.map((device) => <Row key={device.id} device={device} />)}</ul>}
      {signOutFailed ? <p className="link-refused" role="alert">Could not sign out, so this browser is still signed in. Try again.</p> : null}
    </section>
  );
}
