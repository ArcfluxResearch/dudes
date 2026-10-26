// SPDX-License-Identifier: MIT OR Apache-2.0

import { RelativeTime } from '../RelativeTime.tsx';
import { useSyncExternalStore } from 'react';

import { Button, Notice } from '../../ui/index.ts';
import { Group, HINT, Tag } from '../PageParts.tsx';
import { getDevicesState, answerBrowser, subscribeToDevices } from './devices.ts';
import { signOutHere } from './signOut.ts';
import type { Device } from './devices.ts';

function When({ at }: { readonly at: number | string | null }) {
  return at === null ? 'not yet' : <RelativeTime at={at} />;
}

function Row({ device }: { readonly device: Device }) {
  return (
    <li className="grid gap-2 py-3 first:pt-0 last:pb-0 min-[641px]:grid-cols-[minmax(0,1fr)_auto] min-[641px]:items-center min-[641px]:gap-4">
      <div className="grid min-w-0 gap-1">
        <h4 className="m-0 flex flex-wrap items-center gap-2 text-base font-medium text-ink">
          {device.name}{device.this ? <Tag tone="accent">This browser</Tag> : null}
        </h4>
        <dl className="m-0 flex flex-wrap gap-x-4 gap-y-0.5 text-small text-muted">
          <div className="flex gap-1"><dt>Signed in</dt><dd className="m-0 text-ink"><When at={device.signed_in} /></dd></div>
          <div className="flex gap-1"><dt>First used</dt><dd className="m-0 text-ink"><When at={device.first_used} /></dd></div>
          <div className="flex gap-1"><dt>Last used</dt><dd className="m-0 text-ink"><When at={device.last_used} /></dd></div>
        </dl>
      </div>
      <div className="flex">
        <Button size="sm" aria-label={`Sign out ${device.name}${device.this ? ' (this browser)' : ''}`}
          onClick={() => { void (device.this ? signOutHere() : answerBrowser(device.id, 'remove')); }}>Sign out</Button>
      </div>
    </li>
  );
}

/** Every browser signed in to this Floor, with a way to end each one. */
export function DevicesSection() {
  const { devices, loaded, signOutFailed } = useSyncExternalStore(subscribeToDevices, getDevicesState, getDevicesState);
  return (
    <Group title="Signed-in browsers" headingId="settings-browsers"
      description="A browser stays signed in for 30 days without use and 90 days in all. Signing one out here ends it at once.">
      {!loaded ? <p role="status" className={HINT}>Reading the signed-in browsers.</p> : <ul className="m-0 grid list-none divide-y divide-line p-0">
        {devices.map((device) => <Row key={device.id} device={device} />)}</ul>}
      {signOutFailed ? <Notice tone="warn" role="alert">Could not sign out, so this browser is still signed in. Try again.</Notice> : null}
    </Group>
  );
}
