// SPDX-License-Identifier: MIT OR Apache-2.0

import { useSyncExternalStore } from 'react';

import { Button } from '../../ui/Button.tsx';
import { Field, Textarea } from '../../ui/Field.tsx';
import { MenuItem } from '../../ui/Menu.tsx';
import { Select } from '../../ui/Select.tsx';
import { EditIcon } from '../../ui/icons.tsx';

import { ACCENT_COLOURS, CAST_CHARACTERS, accessoryOf, isAccentColour, lookOf } from '../appearance.ts';
import { Portrait } from '../Portrait.tsx';
import { floorSessionGeneration } from '../../session.ts';
import { getFloorState, subscribeToFloor } from '../floor/store.ts';
import type { AgentRow } from '../scene/cast.ts';
import { browserLookDeps, beginLook, cancelLook, checkLook, editLook, getLookState, lookAvailable,
  lookBlocked, lookProblem, submitLook, subscribeToLook } from './look.ts';

/** The More actions entry that starts an edit. The form it opens is drawn by LookEditor, outside the menu. */
export function LookMenuItem({ row }: { readonly row: AgentRow }) {
  const floor = useSyncExternalStore(subscribeToFloor, getFloorState, getFloorState);
  const state = useSyncExternalStore(subscribeToLook, getLookState, getLookState);
  if (!lookAvailable(floor, row)) return null;
  const blocked = lookBlocked(state, floorSessionGeneration());
  return <MenuItem icon={<EditIcon />} disabled={blocked} className="agent-look-open"
    title={blocked ? 'Wait for the current look change to finish or check its status.' : undefined}
    onSelect={() => beginLook(row)}>Edit look</MenuItem>;
}

const CHARACTER_OPTIONS = [{ value: '', label: 'Automatic' },
  ...CAST_CHARACTERS.map((character) => ({ value: character, label: `Character ${character.toUpperCase()}` }))];

export function LookEditor({ row }: { readonly row: AgentRow }) {
  const floor = useSyncExternalStore(subscribeToFloor, getFloorState, getFloorState);
  const state = useSyncExternalStore(subscribeToLook, getLookState, getLookState);
  if (!lookAvailable(floor, row)) return null;
  const blocked = lookBlocked(state, floorSessionGeneration());
  const mine = state.attempt?.payload.agent === row.id;
  const name = typeof row.name === 'string' ? row.name : row.id;
  const draft = state.editor === row.id && !blocked ? state.draft : null;
  const said = mine && (state.sending || state.outcome === 'refused' || state.outcome === 'full' || state.outcome === 'forbidden');
  if (draft === null && !said) return null;
  return <section className="agent-look-editor" aria-label={`Appearance and role for ${name}`}>
    {draft === null ? null : <form onSubmit={(event) => { event.preventDefault(); void submitLook(row, browserLookDeps); }}
      onKeyDown={(event) => { event.stopPropagation(); if (event.key === 'Escape') { event.preventDefault(); cancelLook(); } }}>
      <Field label="Character" id="agent-look-character"><Select id="agent-look-character" autoFocus value={draft.character ?? ''}
        options={CHARACTER_OPTIONS} onValueChange={(character) => editLook({ character: character || null })} /></Field>
      <fieldset className="hire-accents">
        <legend>Accent colour</legend>
        <label><input type="radio" name="look-accent" value="" checked={draft.accent === null}
          onChange={() => editLook({ accent: null })} />Automatic</label>
        {Object.keys(ACCENT_COLOURS).filter(isAccentColour).map((accent) => <label key={accent}>
          <input type="radio" name="look-accent" value={accent} checked={draft.accent === accent}
            onChange={() => editLook({ accent })} />
          <span className="hire-accent-swatch" style={{ backgroundColor: ACCENT_COLOURS[accent] }} />
          {accent[0]?.toUpperCase()}{accent.slice(1)}
        </label>)}
      </fieldset>
      <LookPreview row={row} character={draft.character} accent={draft.accent} role={draft.role} />
      <Field label="Role" id="agent-look-role"><Textarea id="agent-look-role" rows={2} value={draft.role}
        onChange={(event) => editLook({ role: event.currentTarget.value })} /></Field>
      <div className="agent-look-actions">
        <Button type="submit" variant="primary" disabled={lookProblem(draft) !== null} title={lookProblem(draft) ?? undefined}>Save</Button>
        <Button onClick={cancelLook}>Cancel</Button>
      </div>
      {lookProblem(draft) === null ? null : <small role="status">{lookProblem(draft)}</small>}
    </form>}
    {mine && state.sending ? <small role="status">Saving look…</small> : null}
    {mine && state.outcome === 'refused' ? <small role="status">The agent or its look changed. Refresh and try again.</small> : null}
    {mine && state.outcome === 'full' ? <small role="status">The Floor is at its size limit. Shorten the role and save again.</small> : null}
    {mine && state.outcome === 'forbidden' ? <small role="status">This Floor session cannot edit the agent. Open a fresh session.</small> : null}
  </section>;
}

function LookPreview({ row, character, accent, role }: { readonly row: AgentRow;
  readonly character: string | null; readonly accent: string | null; readonly role: string }) {
  return <output className="hire-portrait-preview" aria-label="Portrait preview"
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
