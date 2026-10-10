// SPDX-License-Identifier: MIT OR Apache-2.0

import { useSyncExternalStore } from 'react';
import type { FloorState } from './floor/store.ts';
import { getStreamRetry, subscribeToStreamRetry, STALE_AFTER_MS } from './floor/stream.ts';

export function StreamNotice({ floor }: { readonly floor: FloorState }) {
  const retry = useSyncExternalStore(subscribeToStreamRetry, getStreamRetry, getStreamRetry);
  if (floor.phase === 'closed') return <p className="office-closed stream-notice" role="status">
    Disconnected. The page is not retrying{retry.attempt > 0 ? ` after ${retry.attempt} reconnect attempts` : ''}.
    {' '}The session may have ended or the daemon may have stopped. After a daemon restart, get a fresh link with
    {' '}<code>dudes floor open</code>. Paste the fresh link into this tab's address bar.
    {' '}A reload would drop a Hire or an answer this page is holding.
  </p>;
  if (floor.phase === 'resyncing') return <p className="office-stale stream-notice" role="status">
    The stream disconnected or requested a fresh snapshot. {retry.delayMs === null
      ? 'The page is connecting and waiting for a snapshot.'
      : `The page is retrying. Reconnect attempt ${retry.attempt} starts after ${retry.delayMs} ms.`}
    {' '}Wait for the snapshot. After a daemon restart, get a fresh link with <code>dudes floor open</code>.
  </p>;
  if (floor.phase === 'live' && floor.stale) return <p className="office-stale stream-notice" role="status">
    View unconfirmed. Nothing received for {Math.round(STALE_AFTER_MS / 1000)} seconds or more.
    {' '}The connection is still open. The page is waiting for a frame and is not retrying.
    {' '}If the daemon restarted, get a fresh link with <code>dudes floor open</code>.
  </p>;
  if (floor.phase === 'connecting' || floor.snapshotEpoch !== floor.epoch) return <p className="stream-notice" role="status">
    Connecting. Waiting for the first snapshot from the daemon.
  </p>;
  return null;
}
