// SPDX-License-Identifier: MIT OR Apache-2.0

import { useSyncExternalStore } from 'react';

import { floorSessionGeneration } from '../../session.ts';
import { getFloorState, subscribeToFloor } from '../floor/store.ts';
import { MODEL_SUGGESTIONS } from '../onboarding/hire.ts';
import { isHireEngine } from '../onboarding/catalog.ts';
import type { AgentRow } from '../scene/cast.ts';
import { ENGINE_OWN, REFUSAL_WORDS, browserModelDeps, beginModel, cancelModel, checkModel, editModel, getModelState,
  modelAvailable, modelBlocked, modelProblem, modelView, removeOverride, submitModel, subscribeToModel } from './model.ts';

/** Which model the agent runs, and, where the operator may, a way to change it for the next turn. */
export function ModelEditor({ row }: { readonly row: AgentRow }) {
  const floor = useSyncExternalStore(subscribeToFloor, getFloorState, getFloorState);
  const state = useSyncExternalStore(subscribeToModel, getModelState, getModelState);
  const view = modelView(floor, row);
  const mine = state.attempt?.payload.agent === row.id;
  const suggestions = isHireEngine(row.provider) ? MODEL_SUGGESTIONS[row.provider] : [];
  const name = typeof row.name === 'string' ? row.name : row.id;
  const available = modelAvailable(floor, row);
  const blocked = modelBlocked(state, floorSessionGeneration());
  const editing = available && state.editor === row.id && !blocked;
  const next = view.next === null ? ENGINE_OWN : view.next;
  return <section className="agent-model" aria-label={`Model for ${name}`} data-agent-model={row.id}>
    <p className="agent-model-line" data-model-current={view.current ?? ''} data-model-next={view.next ?? ''}>
      {view.current === null
        ? <>Model: <strong>{next}</strong></>
        : <>Model this turn: <strong>{view.current}</strong>{view.differs ? <> · next turn: <strong>{next}</strong></> : null}</>}
      {view.pinned ? ' (set in the configuration, so the next turn uses it)' : ''}
    </p>
    {!available ? null : view.pinned ? <button type="button" disabled={blocked}
      onClick={() => { void removeOverride(row, browserModelDeps); }}>Remove my override</button> : editing ? <form onSubmit={(event) => { event.preventDefault(); void submitModel(row, browserModelDeps); }}
      onKeyDown={(event) => { event.stopPropagation(); if (event.key === 'Escape') { event.preventDefault(); cancelModel(); } }}>
      <fieldset className="hire-choices">
        <legend>Model for the next turn</legend>
        {['', ...suggestions].map((choice) => <label key={choice} className="hire-choice">
          <input type="radio" name="agent-model" value={choice} checked={state.draft === choice}
            onChange={() => editModel(choice)} />
          <span className="hire-choice-label">{choice === '' ? 'Default model' : choice}</span>
        </label>)}
      </fieldset>
      <label htmlFor="agent-model-name">Another model name</label>
      <input id="agent-model-name" type="text" autoComplete="off" spellCheck={false}
        value={suggestions.includes(state.draft) ? '' : state.draft} aria-invalid={modelProblem(state.draft) !== null}
        onChange={(event) => editModel(event.currentTarget.value)} />
      <button type="submit" disabled={modelProblem(state.draft) !== null}>Save</button>
      <button type="button" onClick={cancelModel}>Cancel</button>
      <small>A run already under way keeps its model. Dudes checks only the spelling; the app reports a name it does not know when the agent next runs.</small>
      {modelProblem(state.draft) === null ? null : <small role="status">{modelProblem(state.draft)}</small>}
    </form> : <button type="button" disabled={blocked} onClick={() => beginModel(row)}>Change model</button>}
    {mine && state.sending ? <small role="status">Saving model…</small> : null}
    {mine && state.outcome === 'changed' ? <small role="status">Saved. It applies from the next turn.</small> : null}
    {mine && state.outcome === 'refused' && state.reason !== null ? <small role="status">{REFUSAL_WORDS[state.reason]}</small> : null}
    {mine && state.outcome === 'forbidden' ? <small role="status">This Floor session cannot change the model. Open a fresh session.</small> : null}
  </section>;
}

export function ModelStatus() {
  const state = useSyncExternalStore(subscribeToModel, getModelState, getModelState);
  if (state.attempt === null || state.outcome === 'changed' || state.outcome === 'refused'
    || state.outcome === 'conflict' || (state.outcome === 'rejected' && !state.uncertain)) return null;
  return <section className="model-status" role="status" aria-label="Model status">
    <p>{state.sending ? 'Saving model…' : state.uncertain ? 'Model change is not settled.'
      : 'This Floor session cannot change the model. Open a fresh session.'}</p>
    {state.uncertain ? <button type="button" disabled={state.sending}
      onClick={() => { void checkModel(browserModelDeps); }}>Check model status</button> : null}
  </section>;
}
