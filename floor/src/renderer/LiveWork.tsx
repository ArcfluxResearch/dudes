// SPDX-License-Identifier: MIT OR Apache-2.0

import type { FloorState } from './floor/store.ts';
import { selectOutputAgent } from './floor/store.ts';
import { handFocusOnLeave } from './focus.ts';
import type { AgentRow } from './scene/cast.ts';

const UNAVAILABLE: Record<string, string> = {
  unknown: 'Agent unavailable', retired: 'Agent retired', attached: 'Managed output unavailable for Attached agents',
  evicted: 'Recent output evicted', cleared: 'Selection cleared',
};

export function streamStatus(floor: FloorState): string {
  if (floor.phase === 'closed') return 'Unavailable — session or daemon disconnected';
  if (floor.phase === 'resyncing' || floor.phase === 'connecting') return 'Reconnecting — waiting for the Floor';
  if (floor.stale) return 'Unconfirmed — waiting for the stream';
  const output = floor.output;
  switch (output.status) {
    case 'idle': return 'No output selected';
    case 'selecting': return 'Selecting — waiting for output';
    case 'reconnecting': return 'Reconnecting — waiting for output';
    case 'waiting': return output.reason === 'unbound' ? 'Waiting — no Managed turn bound yet'
      : 'Waiting — no output in this daemon run';
    case 'live': return output.truncated ? 'Turn in progress — earlier output truncated' : 'Turn in progress';
    case 'ended': return output.truncated ? 'Last turn ended — earlier output truncated' : 'Last turn ended';
    case 'gap': return 'Gap — output incomplete; retry to get a fresh snapshot';
    case 'unavailable': return UNAVAILABLE[output.reason ?? ''] ?? 'Output unavailable';
  }
}

export function SelectionBar({ floor, onClear, leaveFocusTo, showStatus = true }: {
  readonly floor: FloorState;
  readonly onClear: () => void;
  readonly leaveFocusTo: string;
  /** False when the header's own status line already carries the stream state. */
  readonly showStatus?: boolean;
}) {
  const close = (
    <button type="button" id="selection-clear" className="drawer-close" aria-label="Close agent details"
      title="Close (Escape)" ref={handFocusOnLeave} data-focus-on-leave={leaveFocusTo} onClick={onClear}>×</button>
  );
  return showStatus ? (
    <div className="selection-bar">
      <p className="stream-state" role="status">{streamStatus(floor)}</p>
      {close}
    </div>
  ) : close;
}

export function LiveWork({ floor, row }: { readonly floor: FloorState; readonly row: AgentRow | null }) {
  const output = floor.output;
  if (output.targetId === null) return null;
  const emptyText = floor.phase !== 'live' || floor.stale ? 'No confirmed assistant text to show.'
    : output.status === 'ended' ? 'Turn finished without an assistant message.'
      : output.status === 'live' && row?.status === 'running' ? 'Working. No assistant message yet.'
        : 'No assistant message in the available output.';
  const name = typeof row?.name === 'string' && row.name !== '' ? row.name : output.targetId;
  const retry = floor.phase === 'live' && !floor.stale
    && (output.status === 'gap' || output.status === 'unavailable');
  return (
    <section className="live-work" aria-label="Live work" data-output-agent-id={output.targetId}
      data-output-status={output.status}>
      <div className="live-work-head">
        <div><p className="agent-detail-kicker">Read-only</p><h3>Live work</h3></div>
        <div className="live-work-actions">
          {retry ? <button type="button" ref={handFocusOnLeave} data-focus-on-leave="selection-clear"
            onClick={() => selectOutputAgent(output.targetId, true)}>Retry output</button> : null}
        </div>
      </div>
      <p className="live-work-agent">{name} · {output.targetId}</p>
      {output.turnId === null ? null : <p className="live-work-turn">TurnId {output.turnId}</p>}
      {output.chunks.length === 0 ? <p>{emptyText}</p>
        : <pre className="live-work-text">{output.chunks.join('')}</pre>}
    </section>
  );
}
