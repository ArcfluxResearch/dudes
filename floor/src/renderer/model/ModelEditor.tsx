// SPDX-License-Identifier: MIT OR Apache-2.0

import { useSyncExternalStore } from 'react';
import type { ReactNode } from 'react';

import { floorSessionGeneration } from '../../session.ts';
import { Button, Field, Input } from '../../ui/index.ts';
import { getFloorState, subscribeToFloor } from '../floor/store.ts';
import { visibleFailure } from '../chat/EngineSaid.tsx';
import { CHOICE, LABEL, NOTE, RADIO, RECEIPT } from '../receipt.ts';
import type { AgentRow } from '../scene/cast.ts';
import { ModelListBar } from './ModelListBar.tsx';
import { modelItems, ranModels, searchModels } from './lists.ts';
import { ENGINE_OWN, REFUSAL_WORDS, browserModelDeps, beginModel, cancelModel, checkModel, editModel, getModelState,
  modelAvailable, modelBlocked, modelProblem, modelView, removeOverride, submitModel, subscribeToModel } from './model.ts';

/** Which model the agent runs, and, where the operator may, a way to change it for the next turn. */
export function ModelEditor({ row, children }: { readonly row: AgentRow; readonly children?: ReactNode }) {
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
  const model = view.current === null
    ? <>Model: <strong>{next}</strong></>
    : <>Model this turn: <strong>{view.current}</strong>{view.differs ? <> · next turn: <strong>{next}</strong></> : null}</>;
  return <section className="agent-model flex flex-wrap items-center gap-x-3 gap-y-1 font-sans text-small text-muted" aria-label={`Model for ${name}`} data-agent-model={row.id}>
    {available && !view.pinned
      ? <Button variant="ghost" size="sm" className="agent-model-line -ml-2 text-small" disabled={blocked} title={editing ? 'Close the model list' : 'Change model'}
        aria-expanded={editing} data-model-current={view.current ?? ''} data-model-next={view.next ?? ''}
        onClick={() => { if (editing) cancelModel(); else beginModel(row); }}><span>{model}</span></Button>
      : <p className="agent-model-line m-0" data-model-current={view.current ?? ''} data-model-next={view.next ?? ''}>
        {model}{view.pinned ? ' (set in the configuration, so the next turn uses it)' : ''}
      </p>}
    {ran.length === 0 ? null : <p className="agent-model-line m-0" data-model-ran={ran.join(' ')}>
      {ran.length === 1 ? <>In use: <strong>{visibleFailure(ran[0])}</strong></> : <>Ran: <strong>{ran.map(visibleFailure).join(', ')}</strong></>}
    </p>}
    {available && view.pinned ? <Button variant="ghost" size="sm" disabled={blocked}
      onClick={() => { void removeOverride(row, browserModelDeps); }}>Remove my override</Button> : null}
    {children}
    {available && !view.pinned && editing ? <form className="mt-[0.4rem] grid basis-full gap-3 rounded-lg border border-line bg-surface p-4" onSubmit={(event) => { event.preventDefault(); void submitModel(row, browserModelDeps); }}
      onKeyDown={(event) => { event.stopPropagation(); if (event.key === 'Escape') { event.preventDefault(); cancelModel(); } }}>
      <fieldset className="model-choices m-0 grid max-h-56 min-w-0 gap-1.5 overflow-y-auto border-0 p-0">
        <legend className={`mb-1.5 p-0 ${LABEL}`}>Model for the next turn</legend>
        {['', ...shown.map((item) => item.id)].map((choice) => <label key={choice} className={CHOICE}>
          <input type="radio" name="agent-model" value={choice} className={RADIO} checked={state.draft === choice}
            onChange={() => editModel(choice)} />
          <span className="min-w-0 [overflow-wrap:anywhere]">{choice === '' ? 'Default model' : choice}</span>
        </label>)}
      </fieldset>
      {more === 0 ? null : <small className={NOTE}>{more} more models match. Keep typing to narrow the list.</small>}
      <Field label="Search the list, or type another model name" id="agent-model-name">
        <Input type="text" autoComplete="off" spellCheck={false}
          value={state.draft} aria-invalid={modelProblem(state.draft) !== null || undefined}
          onChange={(event) => editModel(event.currentTarget.value)} />
      </Field>
      <ModelListBar provider={String(row.provider)} id="agent-model-list" />
      <div className="flex gap-2">
        <Button type="submit" variant="primary" disabled={modelProblem(state.draft) !== null}>Save</Button>
        <Button onClick={cancelModel}>Cancel</Button>
      </div>
      <small className={NOTE}>A run already under way keeps its model. Dudes checks only the spelling; the app reports a name it does not know when the agent next runs.</small>
      {modelProblem(state.draft) === null ? null : <small role="status" className="text-small text-danger">{modelProblem(state.draft)}</small>}
    </form> : null}
    {mine && state.sending ? <small className={`basis-full ${NOTE}`} role="status">Saving model…</small> : null}
    {mine && state.outcome === 'changed' ? <small className={`basis-full ${NOTE}`} role="status">Saved. It applies from the next turn.</small> : null}
    {mine && state.outcome === 'refused' && state.reason !== null ? <small className={`basis-full ${NOTE}`} role="status">{REFUSAL_WORDS[state.reason]}</small> : null}
    {mine && state.outcome === 'forbidden' ? <small className={`basis-full ${NOTE}`} role="status">This Floor session cannot change the model. Open a fresh session.</small> : null}
  </section>;
}

export function ModelStatus() {
  const state = useSyncExternalStore(subscribeToModel, getModelState, getModelState);
  if (state.attempt === null || state.outcome === 'changed' || state.outcome === 'refused'
    || state.outcome === 'conflict' || (state.outcome === 'rejected' && !state.uncertain)) return null;
  return <section className={`model-status ${RECEIPT}`} role="status" aria-label="Model status">
    <p className="m-0">{state.sending ? 'Saving model…' : state.uncertain ? 'Model change is not settled.'
      : 'This Floor session cannot change the model. Open a fresh session.'}</p>
    {state.uncertain ? <Button size="sm" className="justify-self-start" disabled={state.sending}
      onClick={() => { void checkModel(browserModelDeps); }}>Check model status</Button> : null}
  </section>;
}
