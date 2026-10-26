// SPDX-License-Identifier: MIT OR Apache-2.0

import { useSyncExternalStore } from 'react';
import { Button, cn } from '../ui/index.ts';
import type { FloorState } from './floor/store.ts';
import { getAway, subscribeToAway } from './floor/away.ts';
import { getStreamRetry, refreshFloorSnapshot, subscribeToStreamRetry, STALE_AFTER_MS } from './floor/stream.ts';
import { OpenCommand, SignInExpired } from './SignInExpired.tsx';

const CLOSED = cn('office-closed stream-notice m-0 font-sans text-base text-danger');
const STALE = cn('office-stale stream-notice m-0 font-sans text-base text-muted');

/** The one action a connection notice offers: ask the page's stream to try again now. */
function Reconnect() {
  return <Button size="sm" className="notice-action" onClick={() => refreshFloorSnapshot()}>Reconnect</Button>;
}

/** The line a sheet over the page draws when the stream is not confirmed, since the notice behind it is covered. */
export function NotConnected() {
  return <p className="stream-state" role="alert">Not connected to Dudes, so this may be out of date. <Reconnect /></p>;
}

/**
 * What the connection to Dudes says, if anything. A lost or ended connection stays in the Floor's banner
 * until it is back. The plain wait for the first snapshot is a passing line for the stack. Without
 * `placement` every line shows.
 */
export function StreamNotice({ floor, placement }: { readonly floor: FloorState; readonly placement?: 'banner' | 'stack' }) {
  const retry = useSyncExternalStore(subscribeToStreamRetry, getStreamRetry, getStreamRetry);
  const away = useSyncExternalStore(subscribeToAway, getAway, getAway);
  const passing = away.kind === 'present' && floor.phase !== 'closed' && floor.phase !== 'resyncing'
    && !(floor.phase === 'live' && floor.stale) && (floor.phase === 'connecting' || floor.snapshotEpoch !== floor.epoch);
  if (placement !== undefined && (placement === 'stack') !== passing) return null;
  if (away.kind === 'watching') return <p className={CLOSED} role="status">
    Dudes stopped answering. This page keeps checking and will ask you to reload when it is back.
  </p>;
  if (away.kind === 'updated') return <p className={CLOSED} role="status">
    Dudes was updated. Reload this tab to continue.
    {' '}<a href="https://github.com/ArcfluxResearch/dudes/releases" target="_blank" rel="noopener noreferrer">See the release notes</a>.
  </p>;
  if (away.kind === 'restarted') return <p className={CLOSED} role="status">
    Dudes restarted. Reload this tab to continue.
  </p>;
  if (away.kind === 'expired') return <SignInExpired />;
  if (floor.phase === 'closed') return <p className={CLOSED} role="status">
    Dudes stopped. Start it again with <OpenCommand />, then press <Reconnect />
  </p>;
  if (floor.phase === 'resyncing') return <p className={STALE} role="status">
    {retry.attempt > 0 ? `Reconnecting to Dudes (attempt ${retry.attempt}).` : 'Connecting to Dudes.'}
  </p>;
  if (floor.phase === 'live' && floor.stale) return <p className={STALE} role="status">
    Nothing has come from Dudes for {Math.round(STALE_AFTER_MS / 1000)} seconds, so what you see may be out of date.
    {' '}<Reconnect />
  </p>;
  if (floor.phase === 'connecting' || floor.snapshotEpoch !== floor.epoch) return <p className="stream-notice" role="status">
    Connecting to Dudes. Waiting for the latest agents and tickets.
  </p>;
  return null;
}
