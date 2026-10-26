// SPDX-License-Identifier: MIT OR Apache-2.0

import { useState } from 'react';

import { Button } from '../ui/index.ts';
import type { FloorState } from './floor/store.ts';
import { selectOutputAgent } from './floor/store.ts';
import { handFocusOnLeave } from './focus.ts';
import type { AgentRow } from './scene/cast.ts';
import { X } from 'lucide-react';
import { ICON, IconButton } from '../ui/index.ts';

const UNAVAILABLE: Record<string, string> = {
  unknown: 'Agent unavailable', retired: 'Agent retired', attached: 'Output unavailable for agents Dudes did not start',
  evicted: 'Recent output evicted', cleared: 'Selection cleared',
};

export function streamStatus(floor: FloorState): string {
  if (floor.phase === 'closed') return 'Unavailable — this page lost its connection to Dudes';
  if (floor.phase === 'resyncing' || floor.phase === 'connecting') return 'Reconnecting — waiting for the Floor';
  if (floor.stale) return 'Unconfirmed — waiting for the stream';
  const output = floor.output;
  switch (output.status) {
    case 'idle': return 'No output selected';
    case 'selecting': return 'Selecting — waiting for output';
    case 'reconnecting': return 'Reconnecting — waiting for output';
    case 'waiting': return output.reason === 'unbound' ? 'Waiting — this agent has no work running yet'
      : 'Idle — nothing has run since Dudes started';
    case 'live': return output.truncated ? 'Turn in progress — earlier output truncated' : 'Turn in progress';
    case 'ended': return output.truncated ? 'Last turn ended — earlier output truncated' : 'Last turn ended';
    case 'gap': return 'Gap — output incomplete; retry to load it again';
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
    <IconButton id="selection-clear" className="drawer-close absolute right-2 top-2" label="Close agent details" icon={<X {...ICON} />}
      title="Close (Escape)" ref={handFocusOnLeave} data-focus-on-leave={leaveFocusTo} onClick={onClear} />
  );
  return showStatus ? (
    <div className="selection-bar">
      <p className="stream-state m-0 mb-[0.45rem] font-[700]" role="status">{streamStatus(floor)}</p>
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
    <section className="live-work flex flex-col gap-2 rounded-lg border border-line bg-surface-2 p-3 font-sans text-base text-ink" aria-label="Live work" data-output-agent-id={output.targetId}
      data-output-status={output.status}>
      {status === null ? null : <p className="m-0">{status}</p>}
      {retry ? <div className="live-work-actions flex items-center gap-2">
        <Button size="sm" ref={handFocusOnLeave} data-focus-on-leave="selection-clear"
          onClick={() => selectOutputAgent(output.targetId, true)}>Retry output</Button>
      </div> : null}
      {output.chunks.length === 0 ? null : <pre className="live-work-text m-0 max-h-80 overflow-auto rounded-md border border-line bg-surface p-2 font-mono text-small whitespace-pre-wrap break-words select-text">{output.chunks.join('')}</pre>}
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
    <div className="full-output mt-2 flex flex-col gap-2">
      <Button size="sm" className="full-output-toggle self-start" aria-expanded={open} aria-controls={id}
        onClick={() => onToggle(!open)}>{open ? 'Hide full output' : 'Show full output'}</Button>
      <div id={id} hidden={!open}><LiveWork floor={floor} row={row} /></div>
    </div>
  );
}

/** Overview's own disclosure, for an agent whose output has nowhere else to show. */
export function OverviewOutput({ floor, row }: { readonly floor: FloorState; readonly row: AgentRow }) {
  const [open, setOpen] = useState(false);
  return <FullOutput floor={floor} row={row} id="overview-full-output" open={open} onToggle={setOpen} />;
}
