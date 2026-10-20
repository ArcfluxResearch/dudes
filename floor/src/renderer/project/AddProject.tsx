// SPDX-License-Identifier: MIT OR Apache-2.0

import { useId, useState, useSyncExternalStore } from 'react';
import type { FloorState } from '../floor/store.ts';
import { addPreviewed, addedLine, askToRemove, getAddState, getRemoveState, lookAtFolder, previewLines,
  refusalLine, removeAsked, resetAdd, resetRemove, subscribeToAdd, subscribeToRemove } from './add.ts';

/** Whether this session may add a project from the page. */
export function projectAddOn(floor: FloorState): boolean {
  return floor.settings?.switches.project === true;
}

/**
 * The box where a newcomer types a project's folder. It shows what the daemon found and adds it
 * on one click. Where the daemon has this turned off, `off` is what the page says instead.
 */
export function AddProject({ floor, off }: { readonly floor: FloorState; readonly off?: React.ReactNode }) {
  const add = useSyncExternalStore(subscribeToAdd, getAddState, getAddState);
  const [typed, setTyped] = useState('');
  const id = useId();
  if (!projectAddOn(floor)) return off === undefined ? null : <>{off}</>;
  const busy = add.kind === 'looking' || add.kind === 'adding';
  const shown = add.kind === 'preview' || add.kind === 'adding' || add.kind === 'unsure' ? add.preview : null;
  return <form className="add-project" aria-label="Add a project" onSubmit={(event) => {
    event.preventDefault();
    if (!busy) void lookAtFolder(typed);
  }}>
    <label htmlFor={id}>Folder on this computer</label>
    <input id={id} name="folder" type="text" autoComplete="off" spellCheck={false}
      placeholder="~/code/my-app" value={typed} disabled={busy}
      onChange={(event) => { setTyped(event.target.value); if (add.kind !== 'idle') resetAdd(); }} />
    <button type="submit" aria-disabled={busy || typed.trim() === '' || undefined}>
      {add.kind === 'looking' ? 'Looking…' : 'Look at folder'}</button>
    {shown === null ? null : <div className="add-project-preview" role="status">
      {previewLines(shown).map((line) => <p key={line}>{line}</p>)}
      {shown.already !== null ? null : <button type="button" className="add-project-submit"
        aria-disabled={add.kind === 'adding' || undefined} onClick={() => { void addPreviewed(); }}>
        {add.kind === 'adding' ? 'Adding…' : add.kind === 'unsure' ? 'Try again' : 'Add project'}</button>}
    </div>}
    {add.kind === 'unsure' ? <p role="alert">Dudes did not confirm the add. Try again; it will not add the folder twice.</p> : null}
    {add.kind === 'added' ? <p role="status">{addedLine(add.added)}</p> : null}
    {add.kind === 'refused' ? <p role="alert">{refusalLine(add.reason)}</p> : null}
    {add.kind === 'failed' ? <p role="alert">Dudes did not answer. Nothing was added. Try again.</p> : null}
  </form>;
}

/**
 * The Remove control beside one project. It asks once, on the page, before anything is sent.
 * Where the daemon has this turned off, it shows nothing.
 */
export function RemoveProject({ floor, projectId, label }: {
  readonly floor: FloorState;
  readonly projectId: string;
  readonly label: string;
}) {
  const removal = useSyncExternalStore(subscribeToRemove, getRemoveState, getRemoveState);
  if (!projectAddOn(floor)) return null;
  const mine = (removal.kind === 'asking' || removal.kind === 'removing' || removal.kind === 'unsure'
    || removal.kind === 'refused') && removal.projectId === projectId;
  if (mine && removal.kind !== 'refused') {
    const removing = removal.kind === 'removing';
    return <div className="remove-project" role="group" aria-label={`Remove ${label}`}>
      <p>Remove {label} from Dudes? Agents' history stays in the journal, and the folder on disk is not
        touched. You can add the project again later.</p>
      {removal.kind === 'unsure' ? <p role="alert">Dudes did not confirm the removal. Try again; it will not
        remove anything twice.</p> : null}
      <button type="button" className="remove-project-confirm" aria-disabled={removing || undefined}
        onClick={() => { void removeAsked(); }}>{removing ? 'Removing…' : `Remove ${label}`}</button>
      <button type="button" aria-disabled={removing || undefined}
        onClick={() => { if (!removing) resetRemove(); }}>Keep project</button>
    </div>;
  }
  return <div className="remove-project">
    <button type="button" onClick={() => { askToRemove(projectId, label); }}>Remove project</button>
    {mine && removal.kind === 'refused' ? <p role="alert">{refusalLine(removal.reason)}</p> : null}
  </div>;
}

/** What the last remove did, once the project is out of the list. */
export function RemoveNotice() {
  const removal = useSyncExternalStore(subscribeToRemove, getRemoveState, getRemoveState);
  return removal.kind === 'removed' ? <p role="status">{removal.label} is removed. Its folder was not touched.</p> : null;
}
