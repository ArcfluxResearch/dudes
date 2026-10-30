// SPDX-License-Identifier: MIT OR Apache-2.0

import { useSyncExternalStore } from 'react';
import type { ReactNode } from 'react';

import { accessoryOf, lookOf } from '../appearance.ts';
import { Portrait } from '../Portrait.tsx';
import { Button, Choice, Disclosure, Field, Input, Notice, Select, Textarea, buttonClass, cn } from '../../ui/index.ts';
import { AccentChoice, CHARACTER_OPTIONS } from '../look/LookFields.tsx';

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
import type { ModelItem, ProviderRow } from '../setup/daemon.ts';
import { ModelListBar } from '../model/ModelListBar.tsx';
import { modelItems, searchModels } from '../model/lists.ts';
import { groupPinsModel, HIRE_ENGINES, isHireEngine } from './catalog.ts';
import type { HireEngine } from './catalog.ts';
import {
  LIMITS, NO_PROJECT_LIST, agentsIn, checkHireStatus, lengthNote, ownCopyOffer, SECTIONS, browserHireDeps, chosenWorkspace,
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

const NOTE = 'm-0 text-small leading-normal text-muted';
const FIELDSET = 'm-0 flex min-w-0 flex-col gap-2 border-0 p-0';
const LEGEND = 'mb-2 p-0 text-small font-medium text-ink';
const FACTS = 'm-0 grid grid-cols-[minmax(0,10rem)_minmax(0,1fr)] gap-x-4 gap-y-1 text-base max-[641px]:grid-cols-1';
const CODE = 'rounded-sm bg-surface-2 px-1 font-mono text-small wrap-anywhere';

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
  return <Notice tone="warn" role="note" data-hire-shared="true">
    <p className="m-0">{names.join(' and ')} already {names.length === 1 ? 'works' : 'work'} in this project. Two agents in one
      folder can overwrite each other, so Dudes holds their finished tickets for your review.{' '}
      {copy.state === 'available'
        ? 'To keep them apart, work in its own copy (on the Engine step), or choose another project.'
        : 'To keep them apart, choose another project.'}</p>
  </Notice>;
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
    return <div className="flex flex-col gap-2" data-hire-copy="untrusted">
      <p className={NOTE}>
        This project needs trust before an agent can use its own copy. You can continue in the shared folder or choose another project.
      </p>
      <Disclosure summary="Trust details"><div className="flex flex-col gap-2"><p className={NOTE}>{COPY_EXPLAINED} {TRUST_ALLOWS}</p>
        <p className={NOTE}>To trust it, run <code className={CODE}>{`dudes project trust ${workspace.workspaceId}`}</code> in a terminal, then reload this page.</p>
      </div></Disclosure>
    </div>;
  }
  if (copy.state === 'engine') {
    return <div data-hire-copy="engine">
      <p className={NOTE}>Only Claude can work in its own copy of the project for now.</p>
    </div>;
  }
  return <div className="flex flex-col gap-2" data-hire-copy="available">
    <Choice type="checkbox" name="own_copy" checked={copy.checked} disabled={frozen}
      onChange={(event) => editDraft({ ownCopy: event.target.checked })}
      label="Work in its own copy of the project" note={COPY_EXPLAINED} />
    {copy.preselected && copy.checked
      ? <p className={NOTE} data-hire-copy-reason="shared">{`Ticked because ${names.join(' and ')} already `
        + `${names.length === 1 ? 'works' : 'work'} in this project's folder. You can untick it.`}</p>
      : null}
  </div>;
}

/** Which model the agent runs: the engine's own default, a suggestion, or any name typed. */
function ModelChoice({ engine, items, draft, frozen, configured, fixed, invalid }: {
  readonly engine: HireEngine;
  /** What the daemon says the engine offers, or the short list the page falls back to. */
  readonly items: readonly ModelItem[];
  readonly draft: HireDraft;
  readonly frozen: boolean;
  /** The model the daemon's configuration gives every agent of this engine, if it does. */
  readonly configured: string | null;
  /** Whether the configuration fixes that model beyond what a hire can replace. */
  readonly fixed: boolean;
  readonly invalid: boolean;
}) {
  if (fixed) {
    return <p className={NOTE} data-hire-model="fixed">
      {configured === null ? 'The configuration sets this engine\u2019s model, so it cannot be chosen here.'
        : `The configuration sets this engine\u2019s model to ${configured}, so it cannot be chosen here.`}
    </p>;
  }
  const chosen = draft.model ?? '';
  const { shown, more } = searchModels(items, chosen);
  return <div className="flex flex-col gap-3" data-hire-model="true">
    <fieldset className={FIELDSET} disabled={frozen}>
      <legend className={LEGEND}>Which model?</legend>
      <div className="flex max-h-56 flex-col gap-1.5 overflow-y-auto">
        {['', ...shown.map((item) => item.id)].map((name) => (
          <Choice key={name} type="radio" name="model" value={name} checked={chosen === name}
            onChange={() => editDraft({ model: name })}
            label={name === '' ? (configured === null ? 'Default model' : `Configured: ${configured}`) : name} />
        ))}
      </div>
    </fieldset>
    {more === 0 ? null : <p className={NOTE}>{more} more models match. Keep typing to narrow the list.</p>}
    <Field id="hire-model-name" label="Search the list, or type another model name" hint={<>{configured === null
      ? `Leave empty and ${HIRE_ENGINES[engine]} chooses its default model.`
      : `Leave empty to use the configured model, ${configured}.`} The engine checks the name when the agent first runs. You can change it later from the agent panel.</>}>
      <Input name="model_name" type="text" autoComplete="off" spellCheck={false}
        placeholder="Leave empty to use the app's default model"
        value={chosen} disabled={frozen} aria-invalid={invalid ? true : undefined}
        aria-describedby={invalid ? 'hire-problem' : undefined}
        onChange={(event) => editDraft({ model: event.target.value })} />
    </Field>
    <ModelListBar provider={engine} id="hire-model-list" />
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
    <section className="flex flex-col font-sans text-ink" id="hire-wizard" data-hire-wizard="true" aria-labelledby="hire-title">
      <header className="flex items-center justify-between gap-3 border-b border-line px-6 py-4 max-[641px]:px-4">
        <h2 id="hire-title" tabIndex={-1}
          className="m-0 rounded-md text-title font-semibold outline-0 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-solid focus-visible:outline-focus">Hire an agent</h2>
        {hire.outcome === null ? null : <Button ref={handFocusOnLeave}
          data-focus-on-leave="hire-open" onClick={onClose}>Close</Button>}
      </header>
      {floor.onboarding === null && noProjects(floor) && floor.settings?.switches.hire !== false
        ? null : <div className="px-6 pt-4 max-[641px]:px-4"><UnavailableControls floor={floor} control="hire" /></div>}
      {hire.outcome === null ? <HireForm hire={hire} floor={floor} onClose={onClose} onSettings={onSettings} />
        : <HireAnswer hire={hire} floor={floor} onClose={onClose} />}
    </section>
  );
}

/** The bar of buttons along the bottom of the dialog, which stays in view while the steps scroll. */
function Actions({ children }: { readonly children: ReactNode }) {
  return <footer data-hire-actions="true"
    className="sticky bottom-0 z-10 flex flex-wrap justify-end gap-2 border-t border-line bg-surface px-6 py-3 max-[641px]:px-4">{children}</footer>;
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
  const flagged = (field: FieldName) => (invalid(field) ? true : undefined);
  const described = (field: FieldName) => (invalid(field) ? 'hire-problem' : undefined);
  const nameNote = lengthNote(hire.draft.name, LIMITS.name);
  const roleNote = lengthNote(hire.draft.role, LIMITS.role);

  return (
    <>
      {hire.prefill === null ? null : <p className={cn(NOTE, 'px-6 pt-4 max-[641px]:px-4')}>
        This prepares a new agent from archived details. Review each section.
      </p>}
      <div className="grid gap-6 p-6 min-[801px]:grid-cols-[11rem_minmax(0,1fr)] max-[641px]:p-4">
        <nav aria-label="Hire sections" className="min-w-0">
          <ol className="m-0 flex list-none gap-1 p-0 min-[801px]:flex-col">
            {SECTIONS.map((entry, index) => {
              const current = entry.key === hire.section;
              return <li key={entry.key} className="min-w-0 max-[801px]:flex-1">
                <button type="button" aria-current={current ? 'step' : undefined}
                  aria-disabled={stuck || (index > at && stepProblems.some((problem) =>
                    SECTIONS.findIndex((section) => section.key === problem.section) < index))}
                  onClick={() => { if (!stuck) showSection(entry.key, floor); }}
                  className={cn('box-border grid w-full cursor-pointer grid-cols-[auto_minmax(0,1fr)] items-center gap-x-3 rounded-md border-0 p-2 text-left font-sans text-base text-ink',
                    'transition-colors hover:bg-surface-2 waiting:cursor-not-allowed waiting:opacity-50',
                    'outline-0 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-solid focus-visible:outline-focus',
                    current ? 'bg-accent-soft' : 'bg-transparent',
                    'max-[801px]:justify-items-center max-[801px]:gap-x-2 max-[801px]:text-center')}>
                  <span data-hire-step="number" className={cn('row-span-2 inline-flex size-6 items-center justify-center rounded-pill text-small font-semibold tabular-nums',
                    current ? 'bg-accent text-accent-fg' : 'bg-surface-2 text-muted', 'max-[801px]:row-span-1')}>{index + 1}</span>
                  <span data-hire-step="label" className={cn('font-medium', !current && 'max-[801px]:sr-only')}>{entry.label}</span>
                  <span data-hire-step="hint" className="text-small text-muted max-[801px]:sr-only">{entry.hint}</span>
                </button>
              </li>;
            })}
          </ol>
        </nav>
        <div className="flex min-w-0 flex-col gap-4" data-hire-section={hire.section} data-hire-engine={engine ?? undefined}>
          <h3 className="m-0 text-title font-semibold">{SECTIONS[at]?.label}</h3>
          {hire.prefill?.notes[hire.section] === undefined ? null : <p className={NOTE}>{hire.prefill.notes[hire.section]}</p>}
          {hire.section !== 'identity' ? null : (
            <>
              <Field id="hire-name" label="Name" hint={nameNote === null ? undefined : <span role="status">{nameNote}</span>}>
                <Input name="name" type="text" required autoComplete="off" spellCheck={false}
                  value={hire.draft.name} disabled={frozen}
                  aria-invalid={flagged('name')} aria-describedby={described('name')}
                  onChange={(event) => editDraft({ name: event.target.value })} />
              </Field>
              <Disclosure summary="Role and look" open={invalid('role')} data-hire-more="look">
                <div className="flex flex-col gap-4 pt-2">
                  <div className="flex items-start gap-4">
                    <Field id="hire-character" label="Character" className="min-w-0 flex-1">
                      <Select name="character" options={CHARACTER_OPTIONS} value={hire.draft.character ?? ''} disabled={frozen}
                        onValueChange={(value) => editDraft({ character: value })} />
                    </Field>
                    <HirePortrait draft={hire.draft} />
                  </div>
                  <AccentChoice name="accent" value={hire.draft.accent ?? null} disabled={frozen}
                    onChange={(accent) => editDraft({ accent: accent === null ? void 0 : accent as typeof hire.draft.accent })} />
                  <Field id="hire-role" label="Role" hint={<>{roleNote === null ? null : <span role="status">{roleNote} </span>}
                    A label shown to people and other agents, not instructions for the agent. Leave empty to use engineer.</>}>
                    <Textarea name="role" rows={2} autoComplete="off"
                      value={hire.draft.role} disabled={frozen}
                      aria-invalid={flagged('role')} aria-describedby={described('role')}
                      onChange={(event) => editDraft({ role: event.target.value })} />
                  </Field>
                </div>
              </Disclosure>
            </>
          )}
          {hire.section !== 'workspace' ? null : (
            <>
              {noProject ? <NoHireProjectHint floor={floor} /> : catalog === null ? <p className={NOTE}>{NO_PROJECT_LIST}</p> : (
                <fieldset className={FIELDSET} aria-invalid={flagged('workspace_id')}
                  aria-describedby={described('workspace_id')}>
                  <legend className={LEGEND}>Which project?</legend>
                  {catalog.workspaces.map((space) => (
                    <Choice key={space.workspaceId} type="radio" name="workspace_id" value={space.workspaceId}
                      checked={hire.draft.workspaceId === space.workspaceId} disabled={frozen}
                      onChange={() => editDraft({ workspaceId: space.workspaceId })}
                      label={space.label}
                      note={space.reloadMark?.kind !== 'changed' ? undefined : (
                        <span data-reload-mark={space.reloadMark.kind}>Updated while the daemon was running.</span>
                      )} />
                  ))}
                </fieldset>
              )}
              {catalog === null ? null : <>
                <p className={NOTE}>Choose the project where this agent will work.</p>
                <Disclosure summary="Project details"><dl className={cn(FACTS, 'pt-2')}>
                  {catalog.workspaces.map((space) => <Fact key={space.workspaceId} name={space.label}><code className={CODE}>{space.workspaceId}</code></Fact>)}
                </dl></Disclosure>
              </>}
              {catalog === null || !projectAddOn(floor) ? null : <Disclosure data-hire-add-project="true" summary="Add another project">
                <div className="pt-2"><AddProject floor={floor} /></div>
              </Disclosure>}
              {workspace === null || workspace.repositoryGroup !== undefined ? null : <SharedProjectNote names={sharing} copy={copy} />}
              {workspace?.repositoryGroup === undefined ? null : <div data-hire-repository-group="true" className="flex flex-col gap-3">
                <p className={NOTE}>This agent needs its own copy of every repository below. All repositories share one project queue.</p>
                <dl className={FACTS}>
                  {workspace.repositoryGroup.repositories.map((repo) => <Fact key={repo.repo_id} name={repo.label}>
                    <code className={CODE}>{repo.repo_id}</code>{repo.repo_id === workspace.repositoryGroup!.project.primary_repo ? ' (primary)' : ''}
                  </Fact>)}
                </dl>
                {workspace.repositoryGroup.providers.filter((offer) => !offer.enabled).map((offer) =>
                  <p key={offer.provider} className={NOTE}>{HIRE_ENGINES[offer.provider]}: {offer.refusal}</p>)}
              </div>}
            </>
          )}
          {hire.section !== 'engine' ? null : (
            <>
              {workspace === null ? <p className={NOTE}>Choose a project to see the engines it offers.</p> : (
                <fieldset className={FIELDSET} aria-invalid={flagged('engine')} aria-describedby={described('engine')}>
                  <legend className={LEGEND}>Which engine should do the work?</legend>
                  {workspace.engines.map((name) => (
                    <Choice key={name} type="radio" name="engine" value={name} checked={engine === name} disabled={frozen}
                      onChange={() => editDraft({ engine: name })} label={HIRE_ENGINES[name]}
                      note={workspace.repositoryGroup === undefined || !groupPinsModel(workspace.repositoryGroup, name) ? ENGINE_REACH[name]
                        : "Writes are limited to this project's copies and private work folders. The task's completion check runs with your account's full access."} />
                  ))}
                </fieldset>
              )}
              {engine === null ? null : workspace?.repositoryGroup === undefined || !groupPinsModel(workspace.repositoryGroup, engine)
                ? <ModelChoice engine={engine} items={modelItems(floor, engine)} draft={hire.draft} frozen={frozen}
                  configured={row?.configuredModel ?? null} fixed={row?.modelFixed === true} invalid={invalid('model')} />
                : <p className={NOTE}>This project uses {workspace.repositoryGroup.providers.find((offer) => offer.provider === engine)?.model} for multi-repository work.</p>}
              {workspace === null || workspace.repositoryGroup !== undefined ? null
                : <OwnCopyChoice copy={copy} workspace={workspace} names={sharing} frozen={frozen} />}
              {withdrawn && workspace?.repositoryGroup === undefined ? <p className={NOTE}>An engine missing here may be turned off, or this project may need a spending limit to use it. Check Settings.</p>
                : null}
              {engine === null || row === null ? null : <p className={NOTE} data-engine-found={row.found}>
                {row.found ? `Dudes found ${HIRE_ENGINES[engine]}’s command on this computer.`
                  : `Dudes did not find ${HIRE_ENGINES[engine]}’s command on this computer.`}
              </p>}
              {missing === null ? null : (
                <Notice tone="danger" role="alert" data-hire-missing="true">
                  <p className="m-0">{missing}</p>
                  {engine === null || row?.found !== false || INSTALL_COMMANDS[engine] === undefined ? null : <Disclosure summary="Installation details">
                    <p className="m-0 flex flex-wrap items-center gap-2 pt-1" data-hire-install="true">
                      <code className={CODE}>{INSTALL_COMMANDS[engine]}</code>
                      <Button size="sm" onClick={() => copyToClipboard(INSTALL_COMMANDS[engine]!)}>Copy</Button>
                    </p>
                  </Disclosure>}
                  <div><Button onClick={() => refreshFloorSnapshot()}>Check again</Button></div>
                </Notice>
              )}
              {engine === null ? null : <EngineSignIn engine={engine} row={row} terminal={floor.terminal} onSettings={onSettings} />}
            </>
          )}
          {hire.problem === null || (noProject && hire.problem.field === 'workspace_id') ? null : (
            <Notice tone="danger" id="hire-problem" role="alert">{hire.problem.message}</Notice>
          )}
        </div>
      </div>
      <Reconciliation floor={floor} />
      {/* Next is a plain button: a Button ignores presses while it is aria-disabled, and an incomplete step must still say what is missing when pressed. */}
      <Actions>
        <Button ref={handFocusOnLeave} data-focus-on-leave="hire-open"
          onClick={onClose}>Cancel</Button>
        <Button aria-disabled={at === 0 || frozen || stuck}
          onClick={() => { if (at !== 0 && !frozen && !stuck) moveSection(-1, floor); }}>Back</Button>
        {at === SECTIONS.length - 1
          ? <Button variant="primary" data-hire-action="submit" aria-disabled={blocked !== null || frozen || hire.sending}
            ref={handFocusOnLeave} data-focus-on-leave="hire-title" loading={hire.sending}
            onClick={() => { if (blocked === null && !frozen && !hire.sending) void submitHire(floor, browserHireDeps); }}>
            {hire.sending ? 'Hiring…' : 'Hire'}
          </Button>
          : <button type="button" data-hire-action="next" className={buttonClass('primary', 'md', false)} aria-disabled={frozen || stuck
            || stepProblems.some((problem) => problem.section === hire.section)}
            onClick={() => { if (!frozen && !stuck) moveSection(1, floor); }}>Next</button>}
      </Actions>
      {noProject && viewConfirmed(floor) ? (hire.section === 'workspace' ? null : <div className="px-6 pb-4 max-[641px]:px-4"><NoHireProjectHint floor={floor} /></div>)
        : blocked === null || stuck ? null : <p className={cn(NOTE, 'px-6 pb-4 max-[641px]:px-4')} role="status">{blocked}</p>}
    </>
  );
}

/** One row of a list of facts: a name and what it is. */
function Fact({ name, children }: { readonly name: ReactNode; readonly children: ReactNode }) {
  return <div className="contents">
    <dt className="text-small font-medium text-muted">{name}</dt>
    <dd className="m-0 wrap-anywhere">{children}</dd>
  </div>;
}

/** The hire is complete, but with tickets off nobody can give the agent a task: say so and offer the switch. */
function TicketsOffNote({ floor }: { readonly floor: FloorState }) {
  const switches = floor.settings?.switches;
  const needed = (['control', 'goal'] as const).filter((key) => switches?.[key] === false);
  return <Notice tone="warn" role="status" data-hire-tickets="off">
    <p className="m-0">{TICKETS_OFF_AFTER}</p>
    {switches?.board === false
      ? <div><Button onClick={() => { void changeSetting('floor.board', 'on'); }}>Turn on tickets</Button></div>
      : null}
    {needed.length === 0 ? null : <p className="m-0">{`Tickets also need ${needed.map((key) => key === 'control' ? 'Pause, resume and rename' : 'Change tasks').join(' and ')} on, under Floor switches in Settings.`}</p>}
  </Notice>;
}

function NoHireProjectHint({ floor }: { readonly floor: FloorState }) {
  return <div className="flex flex-col gap-3">
    {projectAddOn(floor)
      ? <p className={NOTE}>Agents work in a project, and none is set up yet. Add the folder of your project below.</p>
      : <p className={NOTE}>Agents work in a project, and none is set up yet. Turn on Add and remove projects under Floor switches in Settings, then add your project folder.</p>}
    <AddProject floor={floor} />
  </div>;
}

function HirePortrait({ draft }: { readonly draft: HireDraft }) {
  return <output aria-label="Portrait preview" data-character={draft.character || 'automatic'} data-accent={draft.accent ?? 'automatic'}
    className="mt-6 flex size-24 shrink-0 items-center justify-center overflow-hidden rounded-lg border border-line bg-surface-2 p-2 text-center text-small text-muted">
    {!draft.character ? <span>Assigned on hire</span> : <Portrait className="size-full object-contain"
      look={lookOf({ id: '', character: draft.character, accent: draft.accent })} accessory={accessoryOf(draft.role)}
      name={draft.name} />}
  </output>;
}

/** What the journal already holds, for an operator who lost track of a Hire. */
function Reconciliation({ floor }: { readonly floor: FloorState }) {
  return (
    <div className="border-t border-line px-6 py-3 max-[641px]:px-4">
      <Disclosure data-hire-more="roster" summary={<span id="hire-roster-title">Agents already on the floor</span>}>
        <div className="flex flex-col gap-2 pt-2">
          <p className={NOTE}>
            This page forgets a hire in progress if it reloads. Look here for that agent before hiring again: every
            hire makes a new agent.
          </p>
          {floor.phase !== 'live' ? <p className={NOTE}>Waiting for the floor.</p> : floor.agents.length === 0 ? (
            <p className={NOTE}>No current agents.</p>
          ) : (
            <ul className="m-0 flex list-none flex-col divide-y divide-line rounded-md border border-line p-0 text-base">
              {floor.agents.map((row) => (
                <li key={row.id} data-roster-agent-id={row.id} className="flex flex-wrap items-baseline gap-x-3 gap-y-0.5 px-3 py-2 wrap-anywhere">
                  <strong className="font-semibold">{text(row.name)}</strong>
                  <span className="text-muted">{text(row.role)}</span>
                  <code className={cn(CODE, 'text-muted')}>{row.id}</code>
                  <span className="basis-full text-small text-muted">{goalLine(row)}</span>
                </li>
              ))}
            </ul>
          )}
        </div>
      </Disclosure>
    </div>
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
    <div data-hire-outcome={outcome.kind} aria-live="polite">
      <div className="flex flex-col gap-4 p-6 max-[641px]:p-4">
        <AnswerBody outcome={outcome} hire={hire} floor={floor} />
        {!settled ? null : !fresh ? <p className={NOTE}>You can hire another once the Floor has refreshed.</p>
          : floor.onboarding === null ? <p className={NOTE}>This session offers no further hires.</p> : null}
      </div>
      <Actions>
        {settled ? null : (
          <Button aria-disabled={hire.sending} loading={hire.sending}
            ref={handFocusOnLeave} data-focus-on-leave="hire-title"
            onClick={() => { if (!hire.sending) void checkHireStatus(browserHireDeps); }}>
            {hire.sending ? 'Checking…' : 'Check status'}
          </Button>
        )}
        {noEffect ? <Button aria-disabled={hire.sending}
          ref={handFocusOnLeave} data-focus-on-leave="hire-title"
          onClick={() => { if (!hire.sending) releaseAttempt(); }}>Edit the form</Button> : null}
        {settled ? (
          <Button aria-disabled={!fresh || floor.onboarding === null}
            ref={handFocusOnLeave} data-focus-on-leave="hire-title"
            onClick={() => { if (fresh && floor.onboarding !== null) startAnotherHire(); }}>Hire another</Button>
        ) : null}
        {shown ? <Button variant="primary" data-hire-action="open" ref={handFocusOnLeave} data-focus-on-leave="hire-open"
          onClick={onClose}>Open {hiredName}</Button> : null}
      </Actions>
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
  return <Disclosure summary="Technical details" data-hire-more="ids">
    <dl className={cn(FACTS, 'pt-2')}>
      <Fact name="AgentId"><span data-hire-agent-id={agent} className="font-mono text-small">{agent}</span></Fact>
    </dl>
  </Disclosure>;
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
            <Notice tone="warn" role="status">
              Hired, then retired: this agent is no longer on the floor.
            </Notice>
          ) : (
            <Notice tone="ok" role="status">
              {`Hired ${agentName(hire, floor, outcome.agent)}.`}
            </Notice>
          )}
          {ticketsOff(floor) ? <TicketsOffNote floor={floor} /> : null}
          <dl className={FACTS}>
            {hire.attempt?.payload.project === undefined ? null : <Fact name="Works in">
              <span data-hire-repository-group-complete="true">Own copies of all {hire.attempt.payload.project.repo_ids.length} repositories</span>
            </Fact>}
            {hire.attempt?.payload.own_copy === true ? (
              <Fact name="Works in"><span data-hire-own-copy="true">Its own copy of the project, on its own branch</span></Fact>
            ) : null}
            <Fact name="On the floor">{presence === 'present' ? 'Yes, on the Floor'
              : presence === 'absent' ? 'No, retired since the Hire' : 'Waiting for the Floor to refresh'}</Fact>
          </dl>
          <RecordIds agent={outcome.agent} />
        </>
      );
    }
    case 'pending':
      return (
        <Notice tone="info" role="status">
          Not confirmed yet. The daemon has this Hire in progress, or cannot confirm it yet. No agent is shown
          until it does; Check status asks again with the same attempt.
        </Notice>
      );
    case 'unavailable':
      return (
        <Notice tone="warn" role="status">
          The daemon could not confirm this Hire, so no agent is shown. Check again later with the same attempt;
          this page does not retry by itself. If this keeps happening, the operator may need to restart the daemon.
        </Notice>
      );
    case 'unknown':
      return (
        <Notice tone="warn" role="status">
          The page lost the daemon&apos;s answer, so it cannot say whether an agent was hired. Check status asks
          again with the same attempt, which never hires twice.
        </Notice>
      );
    case 'refused': {
      const session = sessionAfterRefusal(floor, hire.askedAtEpoch);
      if (outcome.note !== undefined && !hire.uncertain) {
        return <><Notice tone="danger" role="alert">No agent was hired.</Notice>
          <p className={NOTE}>{outcome.note}</p></>;
      }
      if (hire.uncertain) {
        // This refusal answered a check. The request before it has no known
        // result, so nothing here may say that no agent was hired.
        return (
          <Notice tone="danger" role="alert" data-hire-session={session}>
            {session === 'checking'
              ? 'The daemon refused this check before any effect. Checking whether this Floor session is still live…'
              : `${session === 'ended' ? 'Your Floor session has ended or was revoked. ' : ''}The daemon refused `
                + 'this check before any effect, but an earlier request for this Hire has no known result, so an '
                + 'agent may already have been hired. Open Dudes to sign in again in '
                + 'this browser: this page keeps the Hire, and Check status asks again with the same '
                + 'attempt, which never hires twice.'}
          </Notice>
        );
      }
      const workspace = hire.attempt?.payload.project?.project_id ?? hire.attempt?.payload.workspace_id ?? '';
      const offered = floor.onboarding?.workspaces.some((space) => space.workspaceId === workspace) ?? false;
      return (
        <><Notice tone="danger" role="alert" data-hire-session={session}>
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
        </Notice>{workspace === '' ? null : <Disclosure summary="Details" data-hire-more="details">
          <p className={cn(NOTE, 'pt-2')}>Project: <code className={CODE}>{workspace}</code></p>
        </Disclosure>}</>
      );
    }
    case 'conflict':
      return (
        <Notice tone="danger" role="alert">
          The daemon holds a different request under this attempt, so it refused this one before any effect. This
          is a page error; nothing is reported as hired.
        </Notice>
      );
    case 'stale':
      return (
        <Notice tone="danger" role="alert">
          A reload changed this project in the daemon&apos;s configuration after this page was offered it, so
          the daemon refused this Hire before any effect. No agent was hired. Review the project in the fresh
          snapshot and hire again.
        </Notice>
      );
    case 'rejected':
      return (
        <><Notice tone="danger" role="alert">
          Dudes could not use this request. No agent was hired.
        </Notice><Disclosure summary="Details" data-hire-more="details"><p className={cn(NOTE, 'pt-2')}>HTTP status: {outcome.status}</p></Disclosure></>
      );
  }
}
