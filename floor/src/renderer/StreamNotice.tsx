// SPDX-License-Identifier: MIT OR Apache-2.0

import { useSyncExternalStore } from 'react';
import type { FloorState } from './floor/store.ts';
import { getProofState, subscribeToProof } from '../proof.ts';
import { getAway, subscribeToAway } from './floor/away.ts';
import { getStreamRetry, refreshFloorSnapshot, subscribeToStreamRetry, STALE_AFTER_MS } from './floor/stream.ts';
import { OpenCommand, SignInExpired } from './SignInExpired.tsx';

/** The one action a connection notice offers: ask the page's stream to try again now. */
function Reconnect() {
  return <button type="button" className="notice-action" onClick={() => refreshFloorSnapshot()}>Reconnect</button>;
}

/** The line a sheet over the page draws when the stream is not confirmed, since the notice behind it is covered. */
export function NotConnected() {
  return <p className="stream-state" role="alert">Not connected to Dudes, so this may be out of date. <Reconnect /></p>;
}

export function StreamNotice({ floor }: { readonly floor: FloorState }) {
  const retry = useSyncExternalStore(subscribeToStreamRetry, getStreamRetry, getStreamRetry);
  const away = useSyncExternalStore(subscribeToAway, getAway, getAway);
  const proof = useSyncExternalStore(subscribeToProof, getProofState, getProofState);
  if (proof === 'refused') return <p className="office-closed stream-notice" role="alert">
    Dudes stopped, or restarted without this sign-in. Run <OpenCommand /> for a new link, or press <Reconnect /> if
    Dudes is back.
  </p>;
  if (away.kind === 'watching') return <p className="office-closed stream-notice" role="status">
    Dudes stopped answering. This page keeps checking and will ask you to reload when it is back.
  </p>;
  if (away.kind === 'updated') return <p className="office-closed stream-notice" role="status">
    Dudes was updated. Reload this tab to continue.
    {' '}<a href="https://github.com/ArcfluxResearch/dudes/releases" target="_blank" rel="noopener noreferrer">See the release notes</a>.
  </p>;
  if (away.kind === 'restarted') return <p className="office-closed stream-notice" role="status">
    Dudes restarted. Reload this tab to continue.
  </p>;
  if (away.kind === 'expired') return <SignInExpired />;
  if (floor.phase === 'closed') return <p className="office-closed stream-notice" role="status">
    Dudes stopped. Start it again with <OpenCommand />, then press <Reconnect />
  </p>;
  if (floor.phase === 'resyncing') return <p className="office-stale stream-notice" role="status">
    {retry.attempt > 0 ? `Reconnecting to Dudes (attempt ${retry.attempt}).` : 'Connecting to Dudes.'}
  </p>;
  if (floor.phase === 'live' && floor.stale) return <p className="office-stale stream-notice" role="status">
    Nothing has come from Dudes for {Math.round(STALE_AFTER_MS / 1000)} seconds, so what you see may be out of date.
    {' '}<Reconnect />
  </p>;
  if (floor.phase === 'connecting' || floor.snapshotEpoch !== floor.epoch) return <p className="stream-notice" role="status">
    Connecting. Waiting for the first snapshot from the daemon.
  </p>;
  return null;
}
