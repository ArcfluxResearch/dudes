// SPDX-License-Identifier: MIT OR Apache-2.0

import { useSyncExternalStore } from 'react';

import { ACCENT_COLOURS, CAST_CHARACTERS, accessoryOf, isAccentColour, lookOf } from '../appearance.ts';
import { Portrait } from '../Portrait.tsx';
import { floorSessionGeneration } from '../../session.ts';
import { getFloorState, subscribeToFloor } from '../floor/store.ts';
import type { AgentRow } from '../scene/cast.ts';
import { browserLookDeps, beginLook, cancelLook, checkLook, editLook, getLookState, lookAvailable,
  lookBlocked, lookProblem, submitLook, subscribeToLook } from './look.ts';

export function LookEditor({ row }: { readonly row: AgentRow }) {
  const floor = useSyncExternalStore(subscribeToFloor, getFloorState, getFloorState);
  const state = useSyncExternalStore(subscribeToLook, getLookState, getLookState);
  if (!lookAvailable(floor, row)) return null;
  const blocked = lookBlocked(state, floorSessionGeneration());
  const mine = state.attempt?.payload.agent === row.id;
  const name = typeof row.name === 'string' ? row.name : row.id;
  const draft = state.editor === row.id && !blocked ? state.draft : null;
  return <section className="agent-look-editor" aria-label={`Appearance and role for ${name}`}>
    {draft === null ? <button type="button" disabled={blocked} onClick={() => beginLook(row)}>
      Edit look
    </button> : <form onSubmit={(event) => { event.preventDefault(); void submitLook(row, browserLookDeps); }}
      onKeyDown={(event) => { event.stopPropagation(); if (event.key === 'Escape') { event.preventDefault(); cancelLook(); } }}>
      <label htmlFor="agent-look-character">Character</label>
      <select id="agent-look-character" autoFocus value={draft.character ?? ''}
        onChange={(event) => editLook({ character: event.currentTarget.value || null })}>
        <option value="">Automatic</option>
        {CAST_CHARACTERS.map((character) => <option key={character} value={character}>
          Character {character.toUpperCase()}
        </option>)}
      </select>
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
      <label htmlFor="agent-look-role">Role</label>
      <textarea id="agent-look-role" rows={2} value={draft.role}
        onChange={(event) => editLook({ role: event.currentTarget.value })} />
      <button type="submit" disabled={lookProblem(draft) !== null}>Save</button>
      <button type="button" onClick={cancelLook}>Cancel</button>
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
      onClick={() => { void checkLook(browserLookDeps); }}>Check look status</button> : null}
  </section>;
}
