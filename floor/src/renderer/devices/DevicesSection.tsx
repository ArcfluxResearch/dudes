// SPDX-License-Identifier: MIT OR Apache-2.0

import { RelativeTime } from '../RelativeTime.tsx';
import { useSyncExternalStore } from 'react';

import { getDevicesState, answerBrowser, subscribeToDevices } from './devices.ts';
import { signOutHere } from './signOut.ts';
import type { Device } from './devices.ts';
import { Button } from '../../ui/index.ts';

function When({ at }: { readonly at: number | string | null }) {
  return at === null ? 'not yet' : <RelativeTime at={at} />;
}

function Row({ device }: { readonly device: Device }) {
  return (
    <li className="device-row grid justify-items-start gap-1 border-t border-line py-2">
      <h4 className="m-0 text-base font-semibold [overflow-wrap:anywhere]">{device.name}{device.this
        ? <span className="device-this ml-2 rounded-pill border border-accent px-2 text-caption font-semibold text-accent">This browser</span> : null}</h4>
      <dl className="m-0 flex flex-wrap gap-x-5 gap-y-0.5 text-small">
        <div className="block border-0 p-0"><dt className="inline after:content-[':_']">Signed in</dt><dd className="m-0 inline"><When at={device.signed_in} /></dd></div>
        <div className="block border-0 p-0"><dt className="inline after:content-[':_']">First used</dt><dd className="m-0 inline"><When at={device.first_used} /></dd></div>
        <div className="block border-0 p-0"><dt className="inline after:content-[':_']">Last used</dt><dd className="m-0 inline"><When at={device.last_used} /></dd></div>
      </dl>
      <Button size="sm" aria-label={`Sign out ${device.name}${device.this ? ' (this browser)' : ''}`}
        onClick={() => { void (device.this ? signOutHere() : answerBrowser(device.id, 'remove')); }}>Sign out</Button>
    </li>
  );
}

/** Every browser signed in to this Floor, with a way to end each one. */
export function DevicesSection() {
  const { devices, loaded, signOutFailed } = useSyncExternalStore(subscribeToDevices, getDevicesState, getDevicesState);
  return (
    <section aria-labelledby="settings-browsers"><h3 id="settings-browsers">Signed-in browsers</h3>
      <p>A browser stays signed in for 30 days without use and 90 days in all. Signing one out here ends it at once.</p>
      {!loaded ? <p role="status">Reading the signed-in browsers.</p> : <ul className="device-list m-0 grid list-none gap-2 p-0">
        {devices.map((device) => <Row key={device.id} device={device} />)}</ul>}
      {signOutFailed ? <p className="link-refused m-0 text-danger" role="alert">Could not sign out, so this browser is still signed in. Try again.</p> : null}
    </section>
  );
}
