// SPDX-License-Identifier: MIT OR Apache-2.0

import { useSyncExternalStore } from 'react';

import { ACCENT_COLOURS, CAST_CHARACTERS, accessoryOf, isAccentColour, lookOf } from '../appearance.ts';
import { Portrait } from '../Portrait.tsx';

import { handFocusOnLeave } from '../focus.ts';
import { refreshFloorSnapshot } from '../floor/stream.ts';
import type { FloorState } from '../floor/store.ts';
import type { AgentRow } from '../scene/cast.ts';
import { UnavailableControls } from '../settings/SettingsView.tsx';
import { INSTALL_COMMANDS, rowFor } from '../setup/daemon.ts';
import { changeSetting } from '../settings/edit.ts';
import { copyToClipboard } from '../setup/SetupView.tsx';
import { noProjects, viewConfirmed } from '../ticket/model.ts';
import { AddProject, projectAddOn } from '../project/AddProject.tsx';
import { EngineSignIn } from '../login/EngineSignIn.tsx';
import type { ProviderRow } from '../setup/daemon.ts';
import { HIRE_ENGINES, isHireEngine } from './catalog.ts';
import type { HireEngine } from './catalog.ts';
import {
  LIMITS, MODEL_SUGGESTIONS, NO_PROJECT_LIST, agentsIn, checkHireStatus, lengthNote, ownCopyOffer, SECTIONS, browserHireDeps, chosenWorkspace,
  chosenEngine, editDraft, freshSince, getHireState, moveSection, presenceOf, problemsIn, releasable,
  releaseAttempt, submitHire, TICKETS_OFF_AFTER, ticketsOff,
  sessionAfterRefusal, showSection, startAnotherHire, subscribeToHire,
} from './hire.ts';
import type { FieldName, HireDraft, HireOutcome, HireState, OwnCopy } from './hire.ts';
import type { EnrolledWorkspace } from './catalog.ts';

/** What each engine may touch, as the operator must read it before choosing. */
const ENGINE_REACH: Readonly<Record<HireEngine, string>> = Object.freeze({
  claude: 'Does not ask before it acts. It can change any file your account can.',
  codex: 'Can only write inside this project. The check that decides the task is done runs the '
    + "project's own code without that protection.",
  pi: 'Nothing stops it changing files. It can change any file your account can.',
});

function text(value: unknown): string {
  return typeof value === 'string' && value !== '' ? value : '—';
}

function goalOf(row: AgentRow): Record<string, unknown> | null {
  return typeof row.goal === 'object' && row.goal !== null ? row.goal as Record<string, unknown> : null;
}

/** An agent's current goal as the projection states it. */
function goalLine(row: AgentRow): string {
  const goal = goalOf(row);
  if (goal === null) return 'No goal';
  return `${text(goal.text)} · ${text(goal.status)}`;
}

function LengthNote({ value, limit }: { value: string; limit: number }) {
  const note = lengthNote(value, limit);
  return note === null ? null : <p className="hire-count" role="status">{note}</p>;
}

/**
 * Why the engine blocks this Hire, or `null`. Only a report that the program
 * is not found, or that it is signed out, blocks: an absent report, an unknown
 * sign-in, or no row for the engine, never does.
 */
function engineMissing(row: ProviderRow | null, engine: HireEngine | null): string | null {
  const name = engine === null ? 'This app' : HIRE_ENGINES[engine];
  if (row?.found === false) {
    return `${name} was not found on this computer, so this agent could not start. `
      + 'Check Setup for its location. If it is already installed, restart Dudes so it looks again.';
  }
  if (row?.found === true && row.signIn === 'signed_out') {
    return `${name} is installed but not signed in, so this agent could not start. Sign in, then check again.`;
  }
  return null;
}

/** The names of the agents already working in a project. */
function sharedProject(floor: FloorState, workspaceId: string): readonly string[] {
  return agentsIn(floor.agents, workspaceId).map((row) => text(row.name));
}

/** What an own copy is, where it lives and what becomes of it, in the words the person reads. */
const COPY_EXPLAINED = 'A copy is a separate checkout of this project on its own branch, so two agents do not edit '
  + "the same files. Dudes keeps it in its own data folder, not inside your project. When the agent is retired the "
  + 'copy and its branch stay, so nothing it wrote is lost. Dudes never merges, pushes or deletes them for you.';

/** What trusting a project allows, said once. */
const TRUST_ALLOWS = "A copy runs the repository's own Claude settings, such as environment values and helper "
  + 'commands, without asking.';

function SharedProjectNote({ names, copy }: { readonly names: readonly string[]; readonly copy: OwnCopy }) {
  if (names.length === 0) return null;
  return <p className="hire-note hire-shared" role="note" data-hire-shared="true">
    {names.join(' and ')} already {names.length === 1 ? 'works' : 'work'} in this project. Two agents in one
    folder can overwrite each other, so Dudes holds their finished tickets for your review.{' '}
    {copy.state === 'available'
      ? 'To keep them apart, work in its own copy (on the Engine step), or choose another project.'
      : 'To keep them apart, choose another project.'}
  </p>;
}

/** The own-copy choice on the App step: a box, or why there is none. */
function OwnCopyChoice({ copy, workspace, names, frozen }: {
  readonly copy: OwnCopy;
  readonly workspace: EnrolledWorkspace;
  readonly names: readonly string[];
  readonly frozen: boolean;
}) {
  if (copy.state === 'none') return null;
  if (copy.state === 'untrusted') {
    if (names.length === 0) return null;
    return <div className="hire-copy" data-hire-copy="untrusted">
      <p className="hire-note">
        This project needs trust before an agent can use its own copy. You can continue in the shared folder or choose another project.
      </p>
      <details><summary>Trust details</summary><p>{COPY_EXPLAINED} {TRUST_ALLOWS}</p>
        <p>To trust it, run <code>{`dudes project trust ${workspace.workspaceId}`}</code> in a terminal, then reload this page.</p>
      </details>
    </div>;
  }
  if (copy.state === 'engine') {
    return <div className="hire-copy" data-hire-copy="engine">
      <p className="hire-note">Only Claude can work in its own copy of the project for now.</p>
    </div>;
  }
  return <div className="hire-copy" data-hire-copy="available">
    <label className="hire-choice">
      <input type="checkbox" name="own_copy" checked={copy.checked} disabled={frozen}
        onChange={(event) => editDraft({ ownCopy: event.target.checked })} />
      <span className="hire-choice-label">Work in its own copy of the project</span>
    </label>
    {copy.preselected && copy.checked
      ? <p className="hire-note" data-hire-copy-reason="shared">{`Ticked because ${names.join(' and ')} already `
        + `${names.length === 1 ? 'works' : 'work'} in this project's folder. You can untick it.`}</p>
      : null}
    <p className="hire-note">{COPY_EXPLAINED}</p>
  </div>;
}

/** Which model the agent runs: the engine's own default, a suggestion, or any name typed. */
function ModelChoice({ engine, draft, frozen, configured, fixed, invalid }: {
  readonly engine: HireEngine;
  readonly draft: HireDraft;
  readonly frozen: boolean;
  /** The model the daemon's configuration gives every agent of this engine, if it does. */
  readonly configured: string | null;
  /** Whether the configuration fixes that model beyond what a hire can replace. */
  readonly fixed: boolean;
  readonly invalid: boolean;
}) {
  if (fixed) {
    return <p className="hire-note" data-hire-model="fixed">
      {configured === null ? 'The configuration sets this engine\u2019s model, so it cannot be chosen here.'
        : `The configuration sets this engine\u2019s model to ${configured}, so it cannot be chosen here.`}
    </p>;
  }
  const suggestions = MODEL_SUGGESTIONS[engine];
  const chosen = draft.model ?? '';
  const typed = suggestions.includes(chosen) ? '' : chosen;
  return <div className="hire-model" data-hire-model="true">
    <fieldset className="hire-choices" disabled={frozen}>
      <legend>Which model?</legend>
      {['', ...suggestions].map((name) => (
        <label key={name} className="hire-choice">
          <input type="radio" name="model" value={name} checked={chosen === name}
            onChange={() => editDraft({ model: name })} />
          <span className="hire-choice-label">{name === '' ? (configured === null ? 'Default model' : `Configured: ${configured}`) : name}</span>
        </label>
      ))}
    </fieldset>
    <div className="hire-field">
      <label htmlFor="hire-model-name">Another model name</label>
      <input id="hire-model-name" name="model_name" type="text" autoComplete="off" spellCheck={false}
        placeholder="Leave empty to use the app's default model"
        value={typed} disabled={frozen} aria-invalid={invalid}
        aria-describedby={invalid ? 'hire-problem' : undefined}
        onChange={(event) => editDraft({ model: event.target.value })} />
      <p className="hire-count">{configured === null
        ? `Leave empty and ${HIRE_ENGINES[engine]} chooses its default model.`
        : `Leave empty to use the configured model, ${configured}.`} The engine checks the name when the agent first runs. You can change it later from the agent panel.</p>
    </div>
  </div>;
}

/** Why Hire cannot be pressed right now, or `null` when it can. */
function unavailable(floor: FloorState): string | null {
  if (floor.phase === 'closed') return 'The Floor stream is disconnected, so Hire is unavailable.';
  if (floor.phase !== 'live' || floor.snapshotEpoch !== floor.epoch) return 'Waiting for a snapshot from the daemon.';
  if (floor.stale) return 'The view is unconfirmed, so Hire waits for the stream.';
  if (floor.onboarding === null) {
    return NO_PROJECT_LIST;
  }
  return null;
}

export function HireWizard({ floor, onClose, onSettings }: {
  readonly floor: FloorState;
  readonly onClose: () => void;
  readonly onSettings?: () => void;
}) {
  const hire = useSyncExternalStore(subscribeToHire, getHireState, getHireState);
  return <HireWizardView hire={hire} floor={floor} onClose={onClose} onSettings={onSettings} />;
}

export function HireWizardView({ hire, floor, onClose, onSettings }: {
  readonly hire: HireState;
  readonly floor: FloorState;
  readonly onClose: () => void;
  readonly onSettings?: () => void;
}) {
  return (
    <section className="hire-wizard" id="hire-wizard" aria-labelledby="hire-title">
      <header className="hire-head">
        <h2 id="hire-title" tabIndex={-1}>Hire an agent</h2>
        {hire.outcome === null ? null : <button type="button" ref={handFocusOnLeave}
          data-focus-on-leave="hire-open" onClick={onClose}>Close</button>}
      </header>
      {floor.onboarding === null && noProjects(floor) && floor.settings?.switches.hire !== false
        ? null : <UnavailableControls floor={floor} control="hire" />}
      {hire.outcome === null ? <HireForm hire={hire} floor={floor} onClose={onClose} onSettings={onSettings} />
        : <HireAnswer hire={hire} floor={floor} onClose={onClose} />}
    </section>
  );
}

function HireForm({ hire, floor, onClose, onSettings }: {
  readonly hire: HireState;
  readonly floor: FloorState;
  readonly onClose: () => void;
  readonly onSettings?: () => void;
}) {
  const catalog = floor.onboarding;
  const noProject = catalog === null && noProjects(floor);
  const at = SECTIONS.findIndex((entry) => entry.key === hire.section);
  const workspace = chosenWorkspace(hire.draft, catalog);
  const engine = chosenEngine(hire.draft, workspace, floor.daemon);
  const sharing = workspace === null ? [] : sharedProject(floor, workspace.workspaceId);
  const copy = ownCopyOffer(hire.draft, workspace, engine, sharing.length);
  const row = engine === null ? null : rowFor(floor.daemon, engine);
  const missing = engineMissing(row, engine);
  const withdrawn = workspace !== null && (floor.settings?.providers
    .some((name) => isHireEngine(name) && !workspace.engines.includes(name)) ?? false);
  const blocked = unavailable(floor) ?? missing;
  // A missing or signed-out engine holds the wizard on Engine, where its reason, Sign in and Check again are.
  const stuck = hire.section === 'engine' && missing !== null;
  const frozen = hire.attempt !== null;
  const stepProblems = problemsIn(hire.draft, floor);
  const invalid = (field: FieldName) => hire.problem?.field === field;
  const described = (field: FieldName) => (invalid(field) ? 'hire-problem' : undefined);

  return (
    <>
      {hire.prefill === null ? null : <p className="hire-note">
        This prepares a new agent from archived details. Review each section.
      </p>}
      <div className="hire-body">
        <nav className="hire-index" aria-label="Hire sections">
          <ol>
            {SECTIONS.map((entry, index) => (
              <li key={entry.key}>
                <button type="button" aria-current={entry.key === hire.section ? 'step' : undefined}
                  aria-disabled={stuck || (index > at && stepProblems.some((problem) =>
                    SECTIONS.findIndex((section) => section.key === problem.section) < index))}
                  onClick={() => { if (!stuck) showSection(entry.key, floor); }}>
                  <span className="hire-index-number">{index + 1}</span>
                  <span className="hire-index-label">{entry.label}</span>
                  <span className="hire-index-hint">{entry.hint}</span>
                </button>
              </li>
            ))}
          </ol>
        </nav>
        <div className="hire-section" data-hire-section={hire.section} data-hire-engine={engine ?? undefined}>
          <h3>{SECTIONS[at]?.label}</h3>
          {hire.prefill?.notes[hire.section] === undefined ? null : <p className="hire-note">{hire.prefill.notes[hire.section]}</p>}
          {hire.section !== 'identity' ? null : (
            <>
              <div className="hire-field">
                <label htmlFor="hire-name">Name</label>
                <input id="hire-name" name="name" type="text" required autoComplete="off" spellCheck={false}
                  value={hire.draft.name} disabled={frozen}
                  aria-invalid={invalid('name')} aria-describedby={described('name')}
                  onChange={(event) => editDraft({ name: event.target.value })} />
                <LengthNote value={hire.draft.name} limit={LIMITS.name} />
              </div>
              <details className="hire-more" open={invalid('role')}>
              <summary>Role and look</summary>
              <div className="hire-appearance">
                <div className="hire-field">
                  <label htmlFor="hire-character">Character</label>
                  <select id="hire-character" name="character" value={hire.draft.character ?? ''} disabled={frozen}
                    onChange={(event) => editDraft({ character: event.target.value })}>
                    <option value="">Automatic</option>
                    {CAST_CHARACTERS.map((character) => <option key={character} value={character}>
                      Character {character.toUpperCase()}
                    </option>)}
                  </select>
                </div>
                <fieldset className="hire-accents" disabled={frozen}>
                  <legend>Accent colour</legend>
                  <label><input type="radio" name="accent" value="" checked={hire.draft.accent === undefined}
                    onChange={() => editDraft({ accent: void 0 })} />Automatic</label>
                  {Object.keys(ACCENT_COLOURS).filter(isAccentColour).map((accent) => (
                    <label key={accent}>
                      <input type="radio" name="accent" value={accent} checked={hire.draft.accent === accent}
                        onChange={() => editDraft({ accent })} />
                      <span className="hire-accent-swatch" style={{ backgroundColor: ACCENT_COLOURS[accent] }} />
                      {accent[0]?.toUpperCase()}{accent.slice(1)}
                    </label>
                  ))}
                </fieldset>
                <HirePortrait draft={hire.draft} />
              </div>
              <div className="hire-field">
                <label htmlFor="hire-role">Role</label>
                <textarea id="hire-role" name="role" rows={2} autoComplete="off"
                  value={hire.draft.role} disabled={frozen}
                  aria-invalid={invalid('role')} aria-describedby={described('role')}
                  onChange={(event) => editDraft({ role: event.target.value })} />
                <LengthNote value={hire.draft.role} limit={LIMITS.role} />
                <p className="hire-count">A label shown to people and other agents, not instructions for the agent.
                  Leave empty to use engineer.</p>
              </div>
              </details>
            </>
          )}
          {hire.section !== 'workspace' ? null : (
            <>
              {noProject ? <NoHireProjectHint floor={floor} /> : catalog === null ? <p>{NO_PROJECT_LIST}</p> : (
                <fieldset className="hire-choices" aria-invalid={invalid('workspace_id')}
                  aria-describedby={described('workspace_id')}>
                  <legend>Which project?</legend>
                  {catalog.workspaces.map((space) => (
                    <label key={space.workspaceId} className="hire-choice">
                      <input type="radio" name="workspace_id" value={space.workspaceId}
                        checked={hire.draft.workspaceId === space.workspaceId} disabled={frozen}
                        onChange={() => editDraft({ workspaceId: space.workspaceId })} />
                      <span className="hire-choice-label">{space.label}</span>
                      {space.reloadMark?.kind !== 'changed' ? null : (
                        <span className="hire-note" data-reload-mark={space.reloadMark.kind}>
                          Updated while the daemon was running.
                        </span>
                      )}
                    </label>
                  ))}
                </fieldset>
              )}
              {catalog === null ? null : <><p className="hire-note">Choose the project where this agent will work.</p>
                <details className="hire-more"><summary>Project details</summary><dl className="hire-facts">
                  {catalog.workspaces.map(space => <div key={space.workspaceId}><dt>{space.label}</dt><dd><code>{space.workspaceId}</code></dd></div>)}
                </dl></details>
              </>}
              {catalog === null || !projectAddOn(floor) ? null : <details className="hire-more" data-hire-add-project="true">
                <summary>Add another project</summary>
                <AddProject floor={floor} />
              </details>}
              {workspace === null ? null : <SharedProjectNote names={sharing} copy={copy} />}
            </>
          )}
          {hire.section !== 'engine' ? null : (
            <>
              {workspace === null ? <p className="hire-note">Choose a project to see the engines it offers.</p> : (
                <fieldset className="hire-choices" aria-invalid={invalid('engine')} aria-describedby={described('engine')}>
                  <legend>Which engine should do the work?</legend>
                  {workspace.engines.map((name) => (
                    <label key={name} className="hire-choice">
                      <input type="radio" name="engine" value={name} checked={engine === name} disabled={frozen}
                        onChange={() => editDraft({ engine: name })} />
                      <span className="hire-choice-label">{HIRE_ENGINES[name]}</span>
                      <span className="hire-note">{ENGINE_REACH[name]}</span>
                    </label>
                  ))}
                </fieldset>
              )}
              {engine === null ? null : <ModelChoice engine={engine} draft={hire.draft} frozen={frozen}
                configured={row?.configuredModel ?? null} fixed={row?.modelFixed === true} invalid={invalid('model')} />}
              {workspace === null ? null
                : <OwnCopyChoice copy={copy} workspace={workspace} names={sharing} frozen={frozen} />}
              {withdrawn ? <p className="hire-note">An engine missing here may be turned off, or this project may need a spending limit to use it. Check Settings.</p>
                : null}
              {engine === null || row === null ? null : <p className="hire-note" data-engine-found={row.found}>
                {row.found ? `Dudes found ${HIRE_ENGINES[engine]}’s command on this computer.`
                  : `Dudes did not find ${HIRE_ENGINES[engine]}’s command on this computer.`}
              </p>}
              {missing === null ? null : (
                <div className="hire-problem" role="alert">
                  <p>{missing}</p>
                  {engine === null || row?.found !== false || INSTALL_COMMANDS[engine] === undefined ? null : <details>
                    <summary>Installation details</summary><p className="hire-install">
                      <code>{INSTALL_COMMANDS[engine]}</code>{' '}
                      <button type="button" onClick={() => copyToClipboard(INSTALL_COMMANDS[engine]!)}>Copy</button>
                    </p>
                  </details>}
                  <button type="button" onClick={() => refreshFloorSnapshot()}>Check again</button>
                </div>
              )}
              {engine === null ? null : <EngineSignIn engine={engine} row={row} terminal={floor.terminal} onSettings={onSettings} />}
            </>
          )}
          {hire.problem === null || (noProject && hire.problem.field === 'workspace_id') ? null : (
            <p className="hire-problem" id="hire-problem" role="alert">
              {hire.problem.message}
            </p>
          )}
        </div>
      </div>
      <Reconciliation floor={floor} />
      <footer className="hire-actions">
        <button type="button" ref={handFocusOnLeave} data-focus-on-leave="hire-open"
          onClick={onClose}>Cancel</button>
        <button type="button" aria-disabled={at === 0 || frozen || stuck}
          onClick={() => { if (at !== 0 && !frozen && !stuck) moveSection(-1, floor); }}>Back</button>
        {at === SECTIONS.length - 1
          ? <button type="button" className="hire-submit" aria-disabled={blocked !== null || frozen || hire.sending}
            ref={handFocusOnLeave} data-focus-on-leave="hire-title" aria-busy={hire.sending}
            onClick={() => { if (blocked === null && !frozen && !hire.sending) void submitHire(floor, browserHireDeps); }}>
            {hire.sending ? 'Hiring…' : 'Hire'}
          </button>
          : <button type="button" className="hire-next" aria-disabled={frozen || stuck
            || stepProblems.some((problem) => problem.section === hire.section)}
            onClick={() => { if (!frozen && !stuck) moveSection(1, floor); }}>Next</button>}
      </footer>
      {noProject && viewConfirmed(floor) ? (hire.section === 'workspace' ? null : <NoHireProjectHint floor={floor} />)
        : blocked === null || stuck ? null : <p className="hire-note" role="status">{blocked}</p>}
    </>
  );
}

/** The hire is complete, but with tickets off nobody can give the agent a task: say so and offer the switch. */
function TicketsOffNote({ floor }: { readonly floor: FloorState }) {
  const switches = floor.settings?.switches;
  const needed = (['control', 'goal'] as const).filter((key) => switches?.[key] === false);
  return <div className="hire-warning" role="status" data-hire-tickets="off">
    <p>{TICKETS_OFF_AFTER}</p>
    {switches?.board === false
      ? <button type="button" onClick={() => { void changeSetting('floor.board', 'on'); }}>Turn on tickets</button>
      : null}
    {needed.length === 0 ? null : <p>{`Tickets also need ${needed.map((key) => key === 'control' ? 'Pause, resume and rename' : 'Change tasks').join(' and ')} on, under Floor switches in Settings.`}</p>}
  </div>;
}

function NoHireProjectHint({ floor }: { readonly floor: FloorState }) {
  return <>
    {projectAddOn(floor)
      ? <p className="hire-note">Agents work in a project, and none is set up yet. Add the folder of your project below.</p>
      : <p className="hire-note">Agents work in a project, and none is set up yet. Turn on Add and remove projects under Floor switches in Settings, then add your project folder.</p>}
    <AddProject floor={floor} />
  </>;
}

function HirePortrait({ draft }: { readonly draft: HireDraft }) {
  return <output className="hire-portrait-preview" aria-label="Portrait preview"
    data-character={draft.character || 'automatic'} data-accent={draft.accent ?? 'automatic'}>
    {!draft.character ? <span>Assigned on hire</span> : <Portrait className="hire-portrait"
      look={lookOf({ id: '', character: draft.character, accent: draft.accent })} accessory={accessoryOf(draft.role)}
      name={draft.name} />}
  </output>;
}

/** What the journal already holds, for an operator who lost track of a Hire. */
function Reconciliation({ floor }: { readonly floor: FloorState }) {
  return (
    <details className="hire-roster hire-more" aria-labelledby="hire-roster-title">
      <summary id="hire-roster-title">Agents already on the floor</summary>
      <p>
        This page forgets a hire in progress if it reloads. Look here for that agent before hiring again: every
        hire makes a new agent.
      </p>
      {floor.phase !== 'live' ? <p>Waiting for the floor.</p> : floor.agents.length === 0 ? (
        <p>No current agents.</p>
      ) : (
        <ul>
          {floor.agents.map((row) => (
            <li key={row.id} data-roster-agent-id={row.id}>
              <strong>{text(row.name)}</strong> <span>{text(row.role)}</span> <code>{row.id}</code>
              <span className="hire-roster-goal">{goalLine(row)}</span>
            </li>
          ))}
        </ul>
      )}
    </details>
  );
}

function HireAnswer({ hire, floor, onClose }: {
  readonly hire: HireState;
  readonly floor: FloorState;
  readonly onClose: () => void;
}) {
  const outcome = hire.outcome as HireOutcome;
  const settled = outcome.kind === 'complete';
  const noEffect = releasable(hire);
  const fresh = freshSince(floor, hire.askedAtEpoch);
  const hired = outcome.kind === 'complete' ? outcome.agent : null;
  const shown = hired !== null && presenceOf(hired, floor, hire.askedAtEpoch) === 'present';
  const hiredName = hired === null ? '' : agentName(hire, floor, hired);
  return (
    <div className="hire-answer" data-hire-outcome={outcome.kind} aria-live="polite">
      <AnswerBody outcome={outcome} hire={hire} floor={floor} />
      {!settled ? null : !fresh ? <p className="hire-note">You can hire another once the Floor has refreshed.</p>
        : floor.onboarding === null ? <p className="hire-note">This session offers no further hires.</p> : null}
      <footer className="hire-actions">
        {settled ? null : (
          <button type="button" aria-disabled={hire.sending} aria-busy={hire.sending}
            ref={handFocusOnLeave} data-focus-on-leave="hire-title"
            onClick={() => { if (!hire.sending) void checkHireStatus(browserHireDeps); }}>
            {hire.sending ? 'Checking…' : 'Check status'}
          </button>
        )}
        {noEffect ? <button type="button" aria-disabled={hire.sending}
          ref={handFocusOnLeave} data-focus-on-leave="hire-title"
          onClick={() => { if (!hire.sending) releaseAttempt(); }}>Edit the form</button> : null}
        {settled ? (
          <button type="button" aria-disabled={!fresh || floor.onboarding === null}
            ref={handFocusOnLeave} data-focus-on-leave="hire-title"
            onClick={() => { if (fresh && floor.onboarding !== null) startAnotherHire(); }}>Hire another</button>
        ) : null}
        {shown ? <button type="button" className="hire-next" ref={handFocusOnLeave} data-focus-on-leave="hire-open"
          onClick={onClose}>Open {hiredName}</button> : null}
      </footer>
    </div>
  );
}

/** The name this Hire gave the agent, or the Floor's name for it. */
function agentName(hire: HireState, floor: FloorState, agent: string): string {
  const named = hire.attempt?.payload.name ?? '';
  if (named !== '') return named;
  const row = floor.agents.find((entry) => entry.id === agent);
  return row === undefined ? 'The agent' : text(row.name);
}

/** The record ids, for the rare reader who needs them; the names above are what to read. */
function RecordIds({ agent }: { readonly agent: string }) {
  return <details className="hire-more"><summary>Technical details</summary>
    <dl className="hire-facts">
      <div><dt>AgentId</dt><dd data-hire-agent-id={agent}>{agent}</dd></div>
    </dl>
  </details>;
}

function AnswerBody({ outcome, hire, floor }: {
  readonly outcome: HireOutcome;
  readonly hire: HireState;
  readonly floor: FloorState;
}) {
  switch (outcome.kind) {
    case 'complete': {
      const presence = presenceOf(outcome.agent, floor, hire.askedAtEpoch);
      return (
        <>
          {presence === 'absent' ? (
            <p className="hire-warning" role="status">
              Hired, then retired: this agent is no longer on the floor.
            </p>
          ) : (
            <p className="hire-success" role="status">
              {`Hired ${agentName(hire, floor, outcome.agent)}.`}
            </p>
          )}
          {ticketsOff(floor) ? <TicketsOffNote floor={floor} /> : null}
          <dl className="hire-facts">
            {hire.attempt?.payload.own_copy === true ? (
              <div><dt>Works in</dt><dd data-hire-own-copy="true">Its own copy of the project, on its own branch</dd></div>
            ) : null}
            <div><dt>On the floor</dt><dd>{presence === 'present' ? 'Yes, on the Floor'
              : presence === 'absent' ? 'No, retired since the Hire' : 'Waiting for the Floor to refresh'}</dd></div>
          </dl>
          <RecordIds agent={outcome.agent} />
        </>
      );
    }
    case 'pending':
      return (
        <p className="hire-pending" role="status">
          Not confirmed yet. The daemon has this Hire in progress, or cannot confirm it yet. No agent is shown
          until it does; Check status asks again with the same attempt.
        </p>
      );
    case 'unavailable':
      return (
        <p className="hire-pending" role="status">
          The daemon could not confirm this Hire, so no agent is shown. Check again later with the same attempt;
          this page does not retry by itself. If this keeps happening, the operator may need to restart the daemon.
        </p>
      );
    case 'unknown':
      return (
        <p className="hire-pending" role="status">
          The page lost the daemon&apos;s answer, so it cannot say whether an agent was hired. Check status asks
          again with the same attempt, which never hires twice.
        </p>
      );
    case 'refused': {
      const session = sessionAfterRefusal(floor, hire.askedAtEpoch);
      if (outcome.note !== undefined && !hire.uncertain) {
        return <><p className="hire-error" role="alert">No agent was hired.</p>
          <p className="hire-note">{outcome.note}</p></>;
      }
      if (hire.uncertain) {
        // This refusal answered a check. The request before it has no known
        // result, so nothing here may say that no agent was hired.
        return (
          <p className="hire-error" role="alert" data-hire-session={session}>
            {session === 'checking'
              ? 'The daemon refused this check before any effect. Checking whether this Floor session is still live…'
              : `${session === 'ended' ? 'Your Floor session has ended or was revoked. ' : ''}The daemon refused `
                + 'this check before any effect, but an earlier request for this Hire has no known result, so an '
                + 'agent may already have been hired. Open Dudes to sign in again in '
                + 'this browser: this page keeps the Hire, and Check status asks again with the same '
                + 'attempt, which never hires twice.'}
          </p>
        );
      }
      const workspace = hire.attempt?.payload.workspace_id ?? '';
      const offered = floor.onboarding?.workspaces.some((space) => space.workspaceId === workspace) ?? false;
      return (
        <><p className="hire-error" role="alert" data-hire-session={session}>
          {session === 'checking'
            ? 'The daemon refused this Hire before any effect. Checking whether this Floor session is still live…'
            : session === 'ended'
              ? 'Your Floor session has ended or was revoked. No agent was hired. Open Dudes to sign in again '
                + 'in this browser: this page keeps the Hire, and Check status asks '
                + 'again with the same attempt.'
              : floor.onboarding === null
                ? 'Hiring is no longer available to this session. The daemon refused before any effect: the '
                  + 'session may have used its four hires, or its enrollment is no longer eligible. No agent was hired.'
                : !offered
                  ? 'This project is no longer offered. No agent was hired. Choose another project.'
                  : 'The daemon refused this Hire before any effect. No agent was hired.'}
        </p>{workspace === '' ? null : <details className="hire-more"><summary>Details</summary>
          <p>Project: <code>{workspace}</code></p>
        </details>}</>
      );
    }
    case 'conflict':
      return (
        <p className="hire-error" role="alert">
          The daemon holds a different request under this attempt, so it refused this one before any effect. This
          is a page error; nothing is reported as hired.
        </p>
      );
    case 'stale':
      return (
        <p className="hire-error" role="alert">
          A reload changed this project in the daemon&apos;s configuration after this page was offered it, so
          the daemon refused this Hire before any effect. No agent was hired. Review the project in the fresh
          snapshot and hire again.
        </p>
      );
    case 'rejected':
      return (
        <><p className="hire-error" role="alert">
          Dudes could not use this request. No agent was hired.
        </p><details className="hire-more"><summary>Details</summary><p>HTTP status: {outcome.status}</p></details></>
      );
  }
}
