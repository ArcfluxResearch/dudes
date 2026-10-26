// SPDX-License-Identifier: MIT OR Apache-2.0

import type { FloorState } from './floor/store.ts';
import { Button, cn } from '../ui/index.ts';
import { refreshFloorSnapshot } from './floor/stream.ts';

export type ReadState = 'loading' | 'ready' | 'unavailable' | 'error';

export function snapshotReadState(floor: FloorState, available = true): ReadState {
  if (floor.phase === 'closed') return 'unavailable';
  if (floor.phase !== 'live' || floor.snapshotEpoch !== floor.epoch) return 'loading';
  return available ? 'ready' : 'error';
}

export function ReadError({ message, onRetry = refreshFloorSnapshot, className = 'read-error' }: {
  readonly message: string;
  readonly onRetry?: () => void;
  readonly className?: string;
}) {
  return <p className={cn(className, 'm-0 flex flex-wrap items-center gap-x-3 gap-y-1 font-sans text-base text-danger')} role="alert">
    {message} <Button size="sm" onClick={onRetry}>Retry</Button></p>;
}

export function ReadNotice({ state, label, message }: {
  readonly state: ReadState;
  readonly label: string;
  readonly message: string;
}) {
  if (state === 'ready') return null;
  if (state === 'loading') return <p role="status">Loading {label}. Waiting for Dudes to connect.</p>;
  return <ReadError message={state === 'unavailable'
    ? `${label} is unavailable while disconnected. Retry to reconnect.`
    : message} />;
}

export function StreamReadStatus({ floor }: { readonly floor: FloorState }) {
  const state = snapshotReadState(floor);
  const label = state === 'unavailable' ? 'Disconnected. Reconnect to see updates.'
    : floor.phase === 'resyncing' ? 'Refreshing. Waiting for Dudes.'
      : state === 'loading' ? 'Connecting to Dudes.'
        : floor.stale ? 'Connection interrupted. This view may be out of date.' : 'Connected.';
  return <p className="stream-state" role="status">{label}</p>;
}
