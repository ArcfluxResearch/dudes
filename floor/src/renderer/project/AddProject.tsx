// SPDX-License-Identifier: MIT OR Apache-2.0

import { FolderOpen } from 'lucide-react';
import { useId, useState, useSyncExternalStore } from 'react';

import { Button, ICON, Input, Field, Notice } from '../../ui/index.ts';
import type { FloorState } from '../floor/store.ts';
import { addPreviewed, addedLine, askToRemove, chooseFolder, getAddState, getRemoveState, lookAtFolder, notChosenLine, previewLines,
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
  const busy = add.kind === 'looking' || add.kind === 'adding' || add.kind === 'choosing';
  const shown = add.kind === 'preview' || add.kind === 'adding' || add.kind === 'unsure' ? add.preview : null;
  return <form data-add-project="form" aria-label="Add a project" className="flex w-full max-w-xl flex-col gap-3"
    onSubmit={(event) => {
      event.preventDefault();
      if (!busy) void lookAtFolder(typed);
    }}>
    <Field label="Folder on this computer" id={id}>
      <Input name="folder" type="text" autoComplete="off" spellCheck={false} className="font-mono"
        placeholder="~/code/my-app" value={typed} disabled={busy}
        onChange={(event) => { setTyped(event.target.value); if (add.kind !== 'idle') resetAdd(); }} />
    </Field>
    <div className="flex flex-wrap items-center gap-2">
      <Button type="submit" variant={shown === null ? 'primary' : 'secondary'} loading={add.kind === 'looking'}
        aria-disabled={busy || typed.trim() === '' || undefined}
        title={busy ? 'Wait for the current folder request to finish.' : typed.trim() === '' ? 'Enter a project folder first.' : undefined}>
        {add.kind === 'looking' ? 'Looking…' : 'Look at folder'}</Button>
      {floor.settings?.folder_picker !== true ? null : <Button data-add-project="choose" icon={<FolderOpen {...ICON} />}
        aria-disabled={busy || undefined} title={busy ? 'Wait for the current folder request to finish.' : undefined} onClick={() => {
          if (busy) return;
          void chooseFolder().then((path) => {
            if (path === null) return;
            setTyped(path);
            void lookAtFolder(path);
          });
        }}>{add.kind === 'choosing' ? 'Waiting for the folder window…' : 'Choose folder…'}</Button>}
    </div>
    {shown === null ? null : <Notice tone="info" role="status" data-add-project="preview">
      <div className="flex flex-col gap-1">{previewLines(shown).map((line) => <p key={line} className="m-0">{line}</p>)}</div>
      {shown.already !== null ? null : <div><Button variant="primary" data-add-project="submit" loading={add.kind === 'adding'}
        aria-disabled={add.kind === 'adding' || undefined} title={add.kind === 'adding' ? 'Wait for the project to be added.' : undefined} onClick={() => { void addPreviewed(); }}>
        {add.kind === 'adding' ? 'Adding…' : add.kind === 'unsure' ? 'Try again' : 'Add project'}</Button></div>}
    </Notice>}
    {add.kind === 'unsure' ? <Notice tone="warn" role="alert">Dudes did not confirm the add. Try again; it will not add the folder twice.</Notice> : null}
    {add.kind === 'added' ? <Notice tone="ok" role="status">{addedLine(add.added)}</Notice> : null}
    {add.kind === 'refused' ? <Notice tone="danger" role="alert">{refusalLine(add.reason)}</Notice> : null}
    {add.kind === 'not_chosen' ? <Notice tone="danger" role="alert">{notChosenLine(add.why)}</Notice> : null}
    {add.kind === 'failed' ? <Notice tone="danger" role="alert">Dudes did not answer. Nothing was added. Try again.</Notice> : null}
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
    return <div role="group" aria-label={`Remove ${label}`} data-remove-project="confirm"
      className="mt-2 flex max-w-xl flex-col gap-3 rounded-md border border-danger/30 bg-danger-soft p-3 text-base text-ink">
      <p className="m-0">Remove {label} from Dudes? Agents' history stays in the journal, and the folder on disk is not
        touched. You can add the project again later.</p>
      {removal.kind === 'unsure' ? <Notice tone="warn" role="alert">Dudes did not confirm the removal. Try again; it will not
        remove anything twice.</Notice> : null}
      <div className="flex flex-wrap gap-2">
        <Button variant="danger" data-remove-project="confirm-button" loading={removing} aria-disabled={removing || undefined}
          title={removing ? 'Wait for the project removal to finish.' : undefined}
          onClick={() => { void removeAsked(); }}>{removing ? 'Removing…' : `Remove ${label}`}</Button>
        <Button aria-disabled={removing || undefined}
          title={removing ? 'Wait for the project removal to finish.' : undefined}
          onClick={() => { if (!removing) resetRemove(); }}>Keep project</Button>
      </div>
    </div>;
  }
  return <div className="mt-2 flex flex-col items-start gap-2">
    <Button size="sm" onClick={() => { askToRemove(projectId, label); }}>Remove project</Button>
    {mine && removal.kind === 'refused' ? <Notice tone="danger" role="alert">{refusalLine(removal.reason)}</Notice> : null}
  </div>;
}

/** What the last remove did, once the project is out of the list. */
export function RemoveNotice() {
  const removal = useSyncExternalStore(subscribeToRemove, getRemoveState, getRemoveState);
  return removal.kind === 'removed' ? <Notice tone="ok" role="status" className="mt-2 max-w-xl">{removal.label} is removed. Its folder was not touched.</Notice> : null;
}
