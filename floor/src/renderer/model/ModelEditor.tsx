// SPDX-License-Identifier: MIT OR Apache-2.0

import { useSyncExternalStore } from 'react';

import { floorSessionGeneration } from '../../session.ts';
import { Button, Input } from '../../ui/index.ts';
import { getFloorState, subscribeToFloor } from '../floor/store.ts';
import { visibleFailure } from '../chat/EngineSaid.tsx';
import type { AgentRow } from '../scene/cast.ts';
import { ModelListBar } from './ModelListBar.tsx';
import { modelItems, ranModels, searchModels } from './lists.ts';
import { ENGINE_OWN, REFUSAL_WORDS, browserModelDeps, beginModel, cancelModel, checkModel, editModel, getModelState,
  modelAvailable, modelBlocked, modelProblem, modelView, removeOverride, submitModel, subscribeToModel } from './model.ts';

/** Which model the agent runs, and, where the operator may, a way to change it for the next turn. */
export function ModelEditor({ row }: { readonly row: AgentRow }) {
  const floor = useSyncExternalStore(subscribeToFloor, getFloorState, getFloorState);
  const state = useSyncExternalStore(subscribeToModel, getModelState, getModelState);
  const view = modelView(floor, row);
  const mine = state.attempt?.payload.agent === row.id;
  const items = modelItems(floor, row.provider);
  const { shown, more } = searchModels(items, state.draft);
  const ran = ranModels(floor, row.id);
  const name = typeof row.name === 'string' ? row.name : row.id;
  const available = modelAvailable(floor, row);
  const blocked = modelBlocked(state, floorSessionGeneration());
  const editing = available && state.editor === row.id && !blocked;
  const next = view.next === null ? ENGINE_OWN : view.next;
  return <section className="agent-model flex flex-wrap items-center gap-x-3 gap-y-1 font-sans text-base text-muted" aria-label={`Model for ${name}`} data-agent-model={row.id}>
    <p className="agent-model-line m-0" data-model-current={view.current ?? ''} data-model-next={view.next ?? ''}>
      {view.current === null
        ? <>Model: <strong>{next}</strong></>
        : <>Model this turn: <strong>{view.current}</strong>{view.differs ? <> · next turn: <strong>{next}</strong></> : null}</>}
      {view.pinned ? ' (set in the configuration, so the next turn uses it)' : ''}
    </p>
    {ran.length === 0 ? null : <p className="agent-model-line m-0" data-model-ran={ran.join(' ')}>
      {ran.length === 1 ? <>In use: <strong>{visibleFailure(ran[0])}</strong></> : <>Ran: <strong>{ran.map(visibleFailure).join(', ')}</strong></>}
    </p>}
    {!available ? null : view.pinned ? <Button size="sm" disabled={blocked}
      onClick={() => { void removeOverride(row, browserModelDeps); }}>Remove my override</Button> : editing ? <form className="grid basis-full gap-2" onSubmit={(event) => { event.preventDefault(); void submitModel(row, browserModelDeps); }}
      onKeyDown={(event) => { event.stopPropagation(); if (event.key === 'Escape') { event.preventDefault(); cancelModel(); } }}>
      <fieldset className="hire-choices model-choices">
        <legend className="text-small font-medium text-ink">Model for the next turn</legend>
        {['', ...shown.map((item) => item.id)].map((choice) => <label key={choice} className="hire-choice">
          <input type="radio" name="agent-model" value={choice} checked={state.draft === choice}
            onChange={() => editModel(choice)} />
          <span className="hire-choice-label">{choice === '' ? 'Default model' : choice}</span>
        </label>)}
      </fieldset>
      {more === 0 ? null : <small className="text-small">{more} more models match. Keep typing to narrow the list.</small>}
      <label htmlFor="agent-model-name" className="text-small font-medium text-ink">Search the list, or type another model name</label>
      <Input id="agent-model-name" type="text" autoComplete="off" spellCheck={false}
        value={state.draft} aria-invalid={modelProblem(state.draft) !== null}
        onChange={(event) => editModel(event.currentTarget.value)} />
      <ModelListBar provider={String(row.provider)} id="agent-model-list" />
      <div className="flex gap-2">
        <Button type="submit" variant="primary" size="sm" disabled={modelProblem(state.draft) !== null}>Save</Button>
        <Button size="sm" onClick={cancelModel}>Cancel</Button>
      </div>
      <small className="text-small">A run already under way keeps its model. Dudes checks only the spelling; the app reports a name it does not know when the agent next runs.</small>
      {modelProblem(state.draft) === null ? null : <small role="status" className="text-small text-danger">{modelProblem(state.draft)}</small>}
    </form> : <Button variant="ghost" size="sm" disabled={blocked} onClick={() => beginModel(row)}>Change model</Button>}
    {mine && state.sending ? <small className="basis-full text-small" role="status">Saving model…</small> : null}
    {mine && state.outcome === 'changed' ? <small className="basis-full text-small" role="status">Saved. It applies from the next turn.</small> : null}
    {mine && state.outcome === 'refused' && state.reason !== null ? <small className="basis-full text-small" role="status">{REFUSAL_WORDS[state.reason]}</small> : null}
    {mine && state.outcome === 'forbidden' ? <small className="basis-full text-small" role="status">This Floor session cannot change the model. Open a fresh session.</small> : null}
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
