// SPDX-License-Identifier: MIT OR Apache-2.0

import { Pencil } from 'lucide-react';
import { useSyncExternalStore } from 'react';

import { floorSessionGeneration } from '../../session.ts';
import { Button, ICON, IconButton, Input } from '../../ui/index.ts';
import { getFloorState, subscribeToFloor } from '../floor/store.ts';
import { NOTE, RECEIPT } from '../receipt.ts';
import type { AgentRow } from '../scene/cast.ts';
import { beginRename, browserRenameDeps, cancelRename, checkRename, editRename, getRenameState,
  nameProblem, renameAvailable, renameBlocked, submitRename, subscribeToRename } from './rename.ts';

export function NameEditor({ row, surface, showName = true }: {
  readonly row: AgentRow;
  readonly surface: 'detail';
  readonly showName?: boolean;
}) {
  const floor = useSyncExternalStore(subscribeToFloor, getFloorState, getFloorState);
  const state = useSyncExternalStore(subscribeToRename, getRenameState, getRenameState);
  const editor = `${surface}:${row.id}`;
  const name = typeof row.name === 'string' && row.name !== '' ? row.name : row.id;
  if (!renameAvailable(floor, row)) return showName ? <>{name}</> : null;
  const blocked = renameBlocked(state, floorSessionGeneration());
  const mine = state.attempt?.payload.agent === row.id;
  return <span className="agent-name-editor inline-flex flex-wrap items-center gap-1.5" data-rename-agent-id={row.id}>
    {state.editor === editor && !blocked ? <span className="agent-name-form flex flex-wrap items-center gap-1.5">
      <Input autoFocus aria-label={`New name for ${name}`} value={state.draft} className="h-7 w-40"
        onFocus={(event) => event.currentTarget.select()}
        onChange={(event) => editRename(event.currentTarget.value)}
        onKeyDown={(event) => {
          event.stopPropagation();
          if (event.nativeEvent.isComposing || event.keyCode === 229) return;
          if (event.key === 'Escape') { event.preventDefault(); cancelRename(); }
          if (event.key === 'Enter') {
            event.preventDefault(); void submitRename(row, browserRenameDeps);
          }
        }} />
      <Button variant="primary" size="sm" aria-label={`Save name for ${name}`} disabled={nameProblem(state.draft) !== null}
        onClick={() => { void submitRename(row, browserRenameDeps); }}>Save</Button>
      <Button size="sm" onClick={cancelRename}>Cancel</Button>
      {nameProblem(state.draft) === null ? null : <small role="status" className="text-small text-danger">{nameProblem(state.draft)}</small>}
    </span> : <>
      {showName ? <span>{name}</span> : null}
      <IconButton size="sm" label={`Rename ${name}`} icon={<Pencil {...ICON} />} disabled={blocked}
        onClick={() => beginRename(editor, row)} />
    </>}
    {mine && state.sending ? <small className={NOTE} role="status">Saving name…</small> : null}
    {mine && state.outcome === 'refused' ? <small className={NOTE} role="status">The agent is no longer on the floor.</small> : null}
    {mine && state.outcome === 'forbidden' ? <small className={NOTE} role="status">This Floor session cannot rename. Open a fresh session.</small> : null}
  </span>;
}


export function RenameStatus() {
  const state = useSyncExternalStore(subscribeToRename, getRenameState, getRenameState);
  if (state.attempt === null || state.outcome === 'changed' || state.outcome === 'refused'
    || state.outcome === 'conflict' || (state.outcome === 'rejected' && !state.uncertain)) return null;
  const unresolved = state.uncertain;
  return <section className={`rename-status ${RECEIPT}`} role="status" aria-label="Rename status">
    <p className="m-0">{state.sending ? 'Saving name…' : unresolved ? 'Rename is not settled.' : 'This Floor session cannot rename. Open a fresh session.'}</p>
    {unresolved ? <Button size="sm" className="justify-self-start" disabled={state.sending}
      onClick={() => { void checkRename(browserRenameDeps); }}>Check rename status</Button> : null}
  </section>;
}
