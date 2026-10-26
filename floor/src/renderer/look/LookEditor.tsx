// SPDX-License-Identifier: MIT OR Apache-2.0

import { Pencil } from 'lucide-react';
import { useSyncExternalStore } from 'react';

import { CAST_CHARACTERS, accessoryOf, isAccentColour, lookOf } from '../appearance.ts';
import { Portrait } from '../Portrait.tsx';
import { floorSessionGeneration } from '../../session.ts';
import { ICON } from '../../ui/cx.ts';
import { Button, Card, Field, MenuItem, Select, Textarea } from '../../ui/index.ts';
import { getFloorState, subscribeToFloor } from '../floor/store.ts';
import type { AgentRow } from '../scene/cast.ts';
import { AccentChoice } from './AccentChoice.tsx';
import { browserLookDeps, beginLook, cancelLook, checkLook, editLook, getLookState, lookAvailable,
  lookBlocked, lookProblem, submitLook, subscribeToLook } from './look.ts';

export const CHARACTER_OPTIONS = [{ value: '', label: 'Automatic' },
  ...CAST_CHARACTERS.map((character) => ({ value: character, label: `Character ${character.toUpperCase()}` }))] as const;

/** The More menu's way into the look form. */
export function LookMenuItem({ row }: { readonly row: AgentRow }) {
  const floor = useSyncExternalStore(subscribeToFloor, getFloorState, getFloorState);
  const state = useSyncExternalStore(subscribeToLook, getLookState, getLookState);
  if (!lookAvailable(floor, row)) return null;
  const blocked = lookBlocked(state, floorSessionGeneration());
  return <MenuItem icon={<Pencil {...ICON} />} disabled={blocked} className="look-open"
    title={blocked ? 'Wait for the current look change to finish or check its status.' : undefined}
    onSelect={() => beginLook(row)}>Edit look</MenuItem>;
}

/** The look form, drawn while this agent's look is being edited, and the sentences after a save. */
export function LookEditor({ row }: { readonly row: AgentRow }) {
  const floor = useSyncExternalStore(subscribeToFloor, getFloorState, getFloorState);
  const state = useSyncExternalStore(subscribeToLook, getLookState, getLookState);
  if (!lookAvailable(floor, row)) return null;
  const blocked = lookBlocked(state, floorSessionGeneration());
  const mine = state.attempt?.payload.agent === row.id;
  const name = typeof row.name === 'string' ? row.name : row.id;
  const draft = state.editor === row.id && !blocked ? state.draft : null;
  const note = 'm-0 text-small text-muted';
  return <section className="agent-look-editor grid gap-2 font-sans" aria-label={`Appearance and role for ${name}`}>
    {draft === null ? null : <Card className="p-4"><form className="grid gap-4" onSubmit={(event) => { event.preventDefault(); void submitLook(row, browserLookDeps); }}
      onKeyDown={(event) => { event.stopPropagation(); if (event.key === 'Escape' && !event.defaultPrevented) { event.preventDefault(); cancelLook(); } }}>
      <div className="flex items-start justify-between gap-4">
        <Field label="Character" className="min-w-0 flex-1">
          {(c) => <Select id={c.id} aria-describedby={c['aria-describedby']} aria-invalid={c['aria-invalid']} autoFocus options={CHARACTER_OPTIONS} value={draft.character ?? ''}
            onValueChange={(value) => editLook({ character: value || null })} />}
        </Field>
        <LookPreview row={row} character={draft.character} accent={draft.accent} role={draft.role} />
      </div>
      <AccentChoice name="look-accent" value={draft.accent} onChange={(accent) => editLook({ accent })} />
      <Field label="Role">
        {(c) => <Textarea id={c.id} aria-describedby={c['aria-describedby']} aria-invalid={c['aria-invalid']} rows={2} value={draft.role} onChange={(event) => editLook({ role: event.currentTarget.value })} />}
      </Field>
      <div className="flex gap-2">
        <Button type="submit" variant="primary" disabled={lookProblem(draft) !== null} title={lookProblem(draft) ?? undefined}>Save</Button>
        <Button onClick={cancelLook}>Cancel</Button>
      </div>
      {lookProblem(draft) === null ? null : <small role="status" className="text-small text-danger">{lookProblem(draft)}</small>}
    </form></Card>}
    {mine && state.sending ? <small className={note} role="status">Saving look…</small> : null}
    {mine && state.outcome === 'refused' ? <small className={note} role="status">The agent or its look changed. Refresh and try again.</small> : null}
    {mine && state.outcome === 'full' ? <small className={note} role="status">The Floor is at its size limit. Shorten the role and save again.</small> : null}
    {mine && state.outcome === 'forbidden' ? <small className={note} role="status">This Floor session cannot edit the agent. Open a fresh session.</small> : null}
  </section>;
}

export function LookPreview({ row, character, accent, role }: { readonly row: AgentRow;
  readonly character: string | null; readonly accent: string | null; readonly role: string }) {
  return <output className="hire-portrait-preview shrink-0" aria-label="Portrait preview"
    data-character={character ?? 'automatic'} data-accent={accent ?? 'automatic'}>
    <Portrait className="hire-portrait" look={lookOf({ id: row.id, character, accent: isAccentColour(accent) ? accent : null })}
      accessory={accessoryOf(role)} name={row.name} />
  </output>;
}

export function LookStatus() {
  const state = useSyncExternalStore(subscribeToLook, getLookState, getLookState);
  if (state.attempt === null || state.outcome === 'changed' || state.outcome === 'refused'
    || state.outcome === 'conflict' || state.outcome === 'full'
    || (state.outcome === 'rejected' && !state.uncertain)) return null;
  return <section className="look-status" role="status" aria-label="Look status">
    <p>{state.sending ? 'Saving look…' : state.uncertain ? 'Look change is not settled.'
      : 'This Floor session cannot edit the agent. Open a fresh session.'}</p>
    {state.uncertain ? <button type="button" disabled={state.sending}
      title={state.sending ? 'Wait for the current look request to finish.' : undefined}
      onClick={() => { void checkLook(browserLookDeps); }}>Check look status</button> : null}
  </section>;
}
