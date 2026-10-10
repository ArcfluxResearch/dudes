// SPDX-License-Identifier: MIT OR Apache-2.0

import { useState } from 'react';

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

/** The agent's live output as plain text, with one status line when there is no text or the stream is incomplete. */
export function LiveWork({ floor, row }: { readonly floor: FloorState; readonly row: AgentRow | null }) {
  const output = floor.output;
  if (output.targetId === null) return null;
  const emptyText = floor.phase !== 'live' || floor.stale ? 'No confirmed assistant text to show.'
    : output.status === 'ended' ? 'Turn finished without an assistant message.'
      : output.status === 'live' && row?.status === 'running' ? 'Working. No assistant message yet.'
        : 'No assistant message in the available output.';
  const incomplete = output.status === 'gap' || output.status === 'unavailable';
  const retry = floor.phase === 'live' && !floor.stale && incomplete;
  const status = output.chunks.length === 0 ? emptyText : incomplete ? streamStatus(floor) : null;
  return (
    <section className="live-work" aria-label="Live work" data-output-agent-id={output.targetId}
      data-output-status={output.status}>
      {status === null ? null : <p>{status}</p>}
      {retry ? <div className="live-work-actions">
        <button type="button" ref={handFocusOnLeave} data-focus-on-leave="selection-clear"
          onClick={() => selectOutputAgent(output.targetId, true)}>Retry output</button>
      </div> : null}
      {output.chunks.length === 0 ? null : <pre className="live-work-text">{output.chunks.join('')}</pre>}
    </section>
  );
}

/** A collapsed disclosure over the agent's live output. The output stays mounted, hidden, while it is closed. */
export function FullOutput({ floor, row, id, open, onToggle }: {
  readonly floor: FloorState;
  readonly row: AgentRow | null;
  readonly id: string;
  readonly open: boolean;
  readonly onToggle: (open: boolean) => void;
}) {
  return (
    <div className="full-output">
      <button type="button" className="full-output-toggle" aria-expanded={open} aria-controls={id}
        onClick={() => onToggle(!open)}>{open ? 'Hide full output' : 'Show full output'}</button>
      <div id={id} hidden={!open}><LiveWork floor={floor} row={row} /></div>
    </div>
  );
}

/** Overview's own disclosure, for an agent whose output has nowhere else to show. */
export function OverviewOutput({ floor, row }: { readonly floor: FloorState; readonly row: AgentRow }) {
  const [open, setOpen] = useState(false);
  return <FullOutput floor={floor} row={row} id="overview-full-output" open={open} onToggle={setOpen} />;
}
