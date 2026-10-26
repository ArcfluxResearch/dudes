// SPDX-License-Identifier: MIT OR Apache-2.0

import { useSyncExternalStore } from 'react';
import type { ReactNode } from 'react';
import type { FloorState } from './floor/store.ts';
import { getAway, subscribeToAway } from './floor/away.ts';
import { getStreamRetry, refreshFloorSnapshot, subscribeToStreamRetry, STALE_AFTER_MS } from './floor/stream.ts';
import { OpenCommand, SignInExpired } from './SignInExpired.tsx';
import { Button, cn } from '../ui/index.ts';
import { LINK, NoticeIcon, noticeCard, noticeLine } from './notices/look.tsx';
import type { NoticeTone } from './notices/look.tsx';

/** The one action a connection notice offers: ask the page's stream to try again now. */
function Reconnect() {
  return <Button size="sm" className="notice-action" onClick={() => refreshFloorSnapshot()}>Reconnect</Button>;
}

/** The line a sheet over the page draws when the stream is not confirmed, since the notice behind it is covered. */
export function NotConnected() {
  return <p className="stream-state m-0 text-small font-[700] text-danger" role="alert">Not connected to Dudes, so this may be out of date. <Reconnect /></p>;
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
  const line = (tone: NoticeTone, className: string, children: ReactNode) => (
    <p className={cn(placement === 'banner' ? noticeLine(tone) : noticeCard(tone), className)} role="status">
      <NoticeIcon tone={tone} />{children}
    </p>
  );
  const kind = placement === 'banner' ? 'line' : 'card';
  if (away.kind === 'watching') return line('danger', 'office-closed stream-notice',
    'Dudes stopped answering. This page keeps checking and will ask you to reload when it is back.');
  if (away.kind === 'updated') return line('danger', 'office-closed stream-notice', <>
    Dudes was updated. Reload this tab to continue.
    {' '}<a className={LINK} href="https://github.com/ArcfluxResearch/dudes/releases" target="_blank" rel="noopener noreferrer">See the release notes</a>.
  </>);
  if (away.kind === 'restarted') return line('danger', 'office-closed stream-notice', 'Dudes restarted. Reload this tab to continue.');
  if (away.kind === 'expired') return <SignInExpired variant={kind} />;
  if (floor.phase === 'closed') return line('danger', 'office-closed stream-notice', <>
    Dudes stopped. Start it again with <OpenCommand />, then press <Reconnect />
  </>);
  if (floor.phase === 'resyncing') return line('warn', 'office-stale stream-notice',
    retry.attempt > 0 ? `Reconnecting to Dudes (attempt ${retry.attempt}).` : 'Connecting to Dudes.');
  if (floor.phase === 'live' && floor.stale) return line('warn', 'office-stale stream-notice', <>
    Nothing has come from Dudes for {Math.round(STALE_AFTER_MS / 1000)} seconds, so what you see may be out of date.
    {' '}<Reconnect />
  </>);
  if (floor.phase === 'connecting' || floor.snapshotEpoch !== floor.epoch) return line('info', 'stream-notice',
    'Connecting to Dudes. Waiting for the latest agents and tickets.');
  return null;
}
