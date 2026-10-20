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
import { copyToClipboard } from '../setup/SetupView.tsx';
import { decodeWorkspace } from '../workspace.ts';
import { noProjects, viewConfirmed } from '../ticket/model.ts';
import { AddProject, projectAddOn } from '../project/AddProject.tsx';
import type { ProviderRow } from '../setup/daemon.ts';
import { HIRE_ENGINES, formatUsdMicros, isHireEngine } from './catalog.ts';
import type { HireEngine } from './catalog.ts';
import { checkRefusalText, checksFor, choiceLabel } from './checks.ts';
import {
  LIMITS, NO_CHECK, lengthNote, PARTIAL_REASONS, SECTIONS, browserHireDeps, checkHireStatus, chosenCheck, chosenWorkspace,
  chosenEngine, editDraft, freshSince, getHireState, mayCheckRecorded, moveSection, presenceOf, releasable,
  releaseAttempt,
  sessionAfterRefusal, showSection, startAnotherHire, submitHire, subscribeToHire,
} from './hire.ts';
import type { FieldName, HireAttempt, HireDraft, HireOutcome, HireState } from './hire.ts';

/** What each engine may touch, as the operator must read it before choosing. */
const ENGINE_REACH: Readonly<Record<HireEngine, string>> = Object.freeze({
  claude: 'Runs with permission prompts skipped. It can change any file your account can.',
  codex: "Codex's sandbox limits its own writes to this project. The completion check runs this project's code "
    + 'with no sandbox.',
  pi: 'Has no sandbox. It can change any file your account can.',
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

function cap(value: string | null, unit: 'usd' | 'tokens', perDay = false): string {
  if (value === null) return 'No cap';
  const amount = unit === 'usd' ? formatUsdMicros(value) : `${value} tokens`;
  return perDay ? `${amount} per day` : amount;
}

/**
 * Why the engine blocks this Hire, or `null`. Only a report that the program
 * is not found blocks: an absent report, or no row for the engine, never does.
 */
function engineMissing(row: ProviderRow | null, engine: HireEngine | null): string | null {
  if (row?.found !== false) return null;
  const name = engine === null ? 'This app' : HIRE_ENGINES[engine];
  return `${name} is not installed on this computer, so this agent could not start. Install it, then check again.`;
}

/** The plain warning for a project that already has a live agent, or `null`. */
function sharedProject(floor: FloorState, workspaceId: string): readonly string[] {
  return floor.agents.filter((row) => decodeWorkspace(row.workspace)?.id === workspaceId)
    .map((row) => text(row.name));
}

function SharedProjectNote({ names }: { readonly names: readonly string[] }) {
  if (names.length === 0) return null;
  return <p className="hire-note hire-shared" role="note" data-hire-shared="true">
    {names.join(' and ')} already {names.length === 1 ? 'works' : 'work'} in this project. Two agents in one
    folder can overwrite each other, so Dudes holds their finished tickets for your review. To keep them apart,
    give a Claude Code agent its own copy with <code>dudes hire --worktree</code>, or choose another project.
  </p>;
}

/** Why Hire cannot be pressed right now, or `null` when it can. */
function unavailable(floor: FloorState): string | null {
  if (floor.phase === 'closed') return 'The Floor stream is disconnected, so Hire is unavailable.';
  if (floor.phase !== 'live' || floor.snapshotEpoch !== floor.epoch) return 'Waiting for a snapshot from the daemon.';
  if (floor.stale) return 'The view is unconfirmed, so Hire waits for the stream.';
  if (floor.onboarding === null) {
    return 'The daemon offers no project to this session, so Hire is unavailable.';
  }
  return null;
}

export function HireWizard({ floor, onClose }: {
  readonly floor: FloorState;
  readonly onClose: () => void;
}) {
  const hire = useSyncExternalStore(subscribeToHire, getHireState, getHireState);
  return <HireWizardView hire={hire} floor={floor} onClose={onClose} />;
}

export function HireWizardView({ hire, floor, onClose }: {
  readonly hire: HireState;
  readonly floor: FloorState;
  readonly onClose: () => void;
}) {
  return (
    <section className="hire-wizard" id="hire-wizard" aria-labelledby="hire-title">
      <header className="hire-head">
        <div>
          <p className="agent-detail-kicker">Onboarding</p>
          <h2 id="hire-title" tabIndex={-1}>Hire an agent</h2>
        </div>
        <button type="button" ref={handFocusOnLeave} data-focus-on-leave="hire-open" onClick={onClose}>Close</button>
      </header>
      {floor.onboarding === null && noProjects(floor) && floor.settings?.switches.hire !== false
        ? null : <UnavailableControls floor={floor} control="hire" />}
      {hire.outcome === null ? <HireForm hire={hire} floor={floor} onClose={onClose} />
        : <HireAnswer hire={hire} floor={floor} />}
    </section>
  );
}

function HireForm({ hire, floor, onClose }: {
  readonly hire: HireState;
  readonly floor: FloorState;
  readonly onClose: () => void;
}) {
  const catalog = floor.onboarding;
  const noProject = catalog === null && noProjects(floor);
  const at = SECTIONS.findIndex((entry) => entry.key === hire.section);
  const workspace = chosenWorkspace(hire.draft, catalog);
  const engine = chosenEngine(hire.draft, workspace);
  const checks = workspace === null ? null : checksFor(floor.checks, workspace.workspaceId);
  const check = chosenCheck(hire.draft, floor.checks);
  const row = engine === null ? null : rowFor(floor.daemon, engine);
  const missing = engineMissing(row, engine);
  const withdrawn = workspace !== null && (floor.settings?.providers
    .some((name) => isHireEngine(name) && !workspace.engines.includes(name)) ?? false);
  const blocked = unavailable(floor) ?? missing;
  // A missing engine holds the wizard on Engine, where its reason and Check again are.
  const stuck = hire.section === 'engine' && missing !== null;
  const frozen = hire.attempt !== null;
  const invalid = (field: FieldName) => hire.problem?.field === field;
  const described = (field: FieldName) => (invalid(field) ? 'hire-problem' : undefined);

  return (
    <>
      {hire.prefill === null ? null : <p className="hire-note">
        This prepares a new agent from archived details. Review each section and enter a new goal.
      </p>}
      <div className="hire-body">
        <nav className="hire-index" aria-label="Hire sections">
          <ol>
            {SECTIONS.map((entry, index) => (
              <li key={entry.key}>
                <button type="button" aria-current={entry.key === hire.section ? 'step' : undefined}
                  aria-disabled={stuck} onClick={() => { if (!stuck) showSection(entry.key); }}>
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
                <input id="hire-name" name="name" type="text" autoComplete="off" spellCheck={false}
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
                <p className="hire-count">Left empty, the role is engineer.</p>
              </div>
              </details>
            </>
          )}
          {hire.section !== 'workspace' ? null : (
            <>
              {noProject ? <NoHireProjectHint floor={floor} /> : catalog === null ? <p>No project is available for hiring in this session.</p> : (
                <fieldset className="hire-choices" aria-invalid={invalid('workspace_id')}
                  aria-describedby={described('workspace_id')}>
                  <legend>Which project?</legend>
                  {catalog.workspaces.map((space) => (
                    <label key={space.workspaceId} className="hire-choice">
                      <input type="radio" name="workspace_id" value={space.workspaceId}
                        checked={hire.draft.workspaceId === space.workspaceId} disabled={frozen}
                        onChange={() => editDraft({ workspaceId: space.workspaceId })} />
                      <span className="hire-choice-label">{space.label}</span>
                      <code>{space.workspaceId}</code>
                      {space.reloadMark?.kind !== 'changed' ? null : (
                        <span className="hire-note" data-reload-mark={space.reloadMark.kind}>
                          Updated while the daemon was running.
                        </span>
                      )}
                    </label>
                  ))}
                </fieldset>
              )}
              {catalog === null ? null : <p className="hire-note">Choose the project where this agent will work.</p>}
              {workspace === null ? null : <SharedProjectNote names={sharedProject(floor, workspace.workspaceId)} />}
            </>
          )}
          {hire.section !== 'engine' ? null : (
            <>
              {workspace === null ? <p className="hire-note">Choose a project to see the engines it offers.</p> : (
                <fieldset className="hire-choices" aria-invalid={invalid('engine')} aria-describedby={described('engine')}>
                  <legend>Which app should do the work?</legend>
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
              {withdrawn ? <p className="hire-note">{"An engine missing here is turned off in the daemon's "
                + 'configuration, or needs a ceiling on this project because the daemon holds a billing key for it.'}</p>
                : null}
              {engine === null || row === null ? null : <p className="hire-note" data-engine-found={row.found}>
                {row.found ? `${HIRE_ENGINES[engine]} is installed on this computer.`
                  : `${HIRE_ENGINES[engine]} is not installed on this computer.`}
              </p>}
              {missing === null ? null : (
                <div className="hire-problem" role="alert">
                  <p>{missing}</p>
                  {engine === null || INSTALL_COMMANDS[engine] === undefined ? null : <p className="hire-install">
                    <code>{INSTALL_COMMANDS[engine]}</code>{' '}
                    <button type="button" onClick={() => copyToClipboard(INSTALL_COMMANDS[engine]!)}>Copy</button>
                  </p>}
                  <button type="button" onClick={() => refreshFloorSnapshot()}>Check again</button>
                </div>
              )}
            </>
          )}
          {hire.section !== 'briefing' ? null : (
            <>
              <div className="hire-field">
                <label htmlFor="hire-goal">What should they do first?</label>
                <textarea id="hire-goal" name="goal_text" rows={4} autoComplete="off"
                  value={hire.draft.goalText} disabled={frozen}
                  aria-invalid={invalid('goal_text')} aria-describedby={described('goal_text')}
                  onChange={(event) => editDraft({ goalText: event.target.value })} />
                <LengthNote value={hire.draft.goalText} limit={LIMITS.goal_text} />
              </div>
              {workspace === null || catalog === null ? (
                <p className="hire-note">Choose a project to see how this task is checked.</p>
              ) : (
                <>
                  {checks === null ? <p className="hire-note">{NO_CHECK}</p> : (
                    <>
                      <fieldset className="hire-choices" data-check-choice="hire" aria-invalid={invalid('check_id')}
                        aria-describedby={described('check_id')}>
                        <legend>The task is done when</legend>
                        {checks.map((option) => (
                          <label key={option.id} className="hire-choice">
                            <input type="radio" name="check_id" value={option.id} data-check-id={option.id}
                              checked={check?.id === option.id} disabled={frozen}
                              onChange={() => editDraft({ checkId: option.id })} />
                            <span className="hire-choice-label">{choiceLabel(option)}</span>
                          </label>
                        ))}
                      </fieldset>
                      {check?.mayAlreadyPass === true
                        ? <p className="hire-note">This check may already pass, so Dudes hires without running it first.
                          After the agent&apos;s turn it runs the check again. A pass does not prove the task is done, so
                          review the change yourself.</p>
                        : <p className="hire-note">Dudes runs this check once before hiring. If it already passes, there is nothing to do, so nobody is hired.</p>}
                    </>
                  )}
                  <SharedProjectNote names={sharedProject(floor, workspace.workspaceId)} />
                  <details className="hire-more"><summary>Limits</summary>
                  <dl className="hire-facts" aria-label="Configured limits">
                    <div><dt>Hop cap</dt><dd>{workspace.hopCap} Managed turns</dd></div>
                    <div><dt>Goal ceiling</dt><dd>{cap(workspace.goalUsdMicros, 'usd')}</dd></div>
                    <div><dt>Goal token ceiling</dt><dd>{cap(workspace.goalTokens, 'tokens')}</dd></div>
                    <div><dt>Agent ceiling</dt><dd>{cap(workspace.agentUsdPerDayMicros, 'usd', true)}</dd></div>
                    <div><dt>Agent token ceiling</dt><dd>{cap(workspace.agentTokensPerDay, 'tokens', true)}</dd></div>
                    <div><dt>Floor ceiling</dt><dd>{cap(catalog.floorUsdPerDayMicros, 'usd', true)}</dd></div>
                  </dl>
                  {engine === null ? null : <p className="hire-note">
                    {`This agent runs at most ${workspace.hopCap} turns for this task. ${HIRE_ENGINES[engine]}'s own `
                      + 'plan limits apply, and so does any ceiling shown above.'}
                  </p>}
                  </details>
                </>
              )}
              <p className="hire-note">
                Dudes runs the check itself. An agent saying it is finished does not count.
              </p>
            </>
          )}
          {hire.problem === null || (noProject && hire.problem.field === 'workspace_id') ? null : (
            <p className="hire-problem" id="hire-problem" role="alert" data-check-refusal={hire.problem.refusal}>
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
        <button type="button" aria-disabled={at === SECTIONS.length - 1 || frozen || stuck}
          onClick={() => { if (at !== SECTIONS.length - 1 && !frozen && !stuck) moveSection(1, floor); }}>Next</button>
        <button type="button" className="hire-submit" aria-disabled={blocked !== null || frozen || hire.sending}
          ref={handFocusOnLeave} data-focus-on-leave="hire-title" aria-busy={hire.sending}
          onClick={() => { if (blocked === null && !frozen && !hire.sending) void submitHire(floor, browserHireDeps); }}>
          {hire.sending ? 'Hiring…' : 'Hire'}
        </button>
      </footer>
      {hire.sending && hire.attempt !== null ? (
        <p className="hire-note" role="status">{`Running “${hire.attempt.checkLabel}” once before hiring.`}</p>
      ) : null}
      {noProject && viewConfirmed(floor) ? (hire.section === 'workspace' ? null : <NoHireProjectHint floor={floor} />)
        : blocked === null || stuck ? null : <p className="hire-note" role="status">{blocked}</p>}
    </>
  );
}

function NoHireProjectHint({ floor }: { readonly floor: FloorState }) {
  return <>
    {projectAddOn(floor)
      ? <p className="hire-note">Agents work in a project, and none is set up yet. Add the folder of your project below.</p>
      : <p className="hire-note">Agents work in a project, and none is set up yet. In your project&apos;s folder run <code>dudes project add</code>.</p>}
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

function HireAnswer({ hire, floor }: { readonly hire: HireState; readonly floor: FloorState }) {
  const outcome = hire.outcome as HireOutcome;
  const settled = outcome.kind === 'complete' || outcome.kind === 'partial';
  const noEffect = releasable(hire);
  // A goal lost only because the session ended can still be set from a fresh
  // one: the same attempt appends the goal the Hire recorded, and nothing else.
  const retryable = outcome.kind === 'partial'
    && (outcome.reason === 'session_expired' || outcome.reason === 'session_revoked');
  const fresh = freshSince(floor, hire.askedAtEpoch);
  // An attempt that hired an agent checks only on a fresh session that offers hiring.
  const waiting = hire.recordedAgent !== null && !mayCheckRecorded(hire, floor);
  return (
    <div className="hire-answer" data-hire-outcome={outcome.kind} aria-live="polite">
      <AnswerBody outcome={outcome} hire={hire} floor={floor} />
      <footer className="hire-actions">
        {settled && !retryable ? null : (
          <button type="button" aria-disabled={hire.sending || waiting} aria-busy={hire.sending}
            ref={handFocusOnLeave} data-focus-on-leave="hire-title"
            onClick={() => { if (!hire.sending && !waiting) void checkHireStatus(browserHireDeps); }}>
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
      </footer>
      {!waiting || (settled && !retryable) ? null : (
        <p className="hire-note">Check status waits for a fresh Floor session that offers hiring.</p>
      )}
      {!settled ? null : (
        <p className="hire-note">
          {!fresh ? 'Another Hire is offered once a fresh snapshot lists an project.'
            : floor.onboarding === null ? 'This session offers no further hires.'
              : 'The fresh snapshot offers another Hire.'}
        </p>
      )}
    </div>
  );
}

/** Every reply a Check can get on an attempt that has already hired its agent, but the two that settle it. */
type RecordedReply = Exclude<HireOutcome, { readonly kind: 'complete' } | { readonly kind: 'partial' }>;

/**
 * A Check on an attempt that has already hired an agent. Whatever it got back,
 * that agent is in the journal from this Hire, so it stays on screen: nothing
 * here says no agent was hired, or that the page cannot tell. Once a fresh
 * session offers hiring, the next step named is Check status, not another link.
 */
function RecordedCheck({ outcome, agent, hire, floor }: {
  readonly outcome: RecordedReply;
  readonly agent: string;
  readonly hire: HireState;
  readonly floor: FloorState;
}) {
  const session = sessionAfterRefusal(floor, hire.askedAtEpoch);
  const noEffect = outcome.kind === 'refused' || outcome.kind === 'conflict' || outcome.kind === 'rejected'
    || outcome.kind === 'stale' || outcome.kind === 'check';
  const said = outcome.kind === 'pending'
    ? 'Not confirmed yet: the daemon has this check in progress, or cannot confirm it yet.'
    : outcome.kind === 'unavailable'
      ? 'The daemon could not confirm this check, and this page does not ask again by itself.'
      : outcome.kind === 'unknown'
        ? 'The page lost the daemon’s answer to this check.'
        : outcome.kind === 'conflict'
          ? 'The daemon holds a different request under this attempt, so it refused this check before any effect. '
            + 'This is a page error.'
          : outcome.kind === 'rejected'
            ? `The daemon refused this check itself (HTTP ${outcome.status}) before any effect.`
            : session === 'checking'
              ? 'The daemon refused this check before any effect. Checking whether this Floor session is still live…'
              : `${session === 'ended' ? 'Your Floor session has ended or was revoked. ' : ''}`
                + 'The daemon refused this check before any effect.';
  const next = mayCheckRecorded(hire, floor)
    ? ' Check status asks again with the same attempt, which never hires twice and sets only the goal this Hire '
      + 'recorded.'
    : floor.phase === 'closed'
      ? ' Paste a new link from dudes floor open into this tab’s address bar: this page keeps the Hire, '
        + 'and Check status then sets only the goal this Hire recorded.'
      : '';
  return (
    <>
      <p className={noEffect ? 'hire-warning' : 'hire-pending'} role={noEffect ? 'alert' : 'status'}
        data-hire-session={outcome.kind === 'refused' ? session : undefined}
        data-check-refusal={outcome.kind === 'check' ? outcome.reason : undefined}>
        {`${said} The agent below is in the journal from this Hire.${next}`}
      </p>
      <dl className="hire-facts">
        <div><dt>AgentId</dt><dd data-hire-agent-id={agent}>{agent}</dd></div>
      </dl>
    </>
  );
}

function AnswerBody({ outcome, hire, floor }: {
  readonly outcome: HireOutcome;
  readonly hire: HireState;
  readonly floor: FloorState;
}) {
  // An attempt that hired an agent keeps it on screen, whatever a later Check got back.
  if (hire.recordedAgent !== null && outcome.kind !== 'complete' && outcome.kind !== 'partial') {
    return <RecordedCheck outcome={outcome} agent={hire.recordedAgent} hire={hire} floor={floor} />;
  }
  switch (outcome.kind) {
    case 'complete': {
      const presence = presenceOf(outcome.agent, floor, hire.askedAtEpoch);
      const row = floor.agents.find((agent) => agent.id === outcome.agent) ?? null;
      const currentGoal = row === null ? null : goalOf(row)?.id ?? null;
      return (
        <>
          {presence === 'absent' ? (
            <p className="hire-warning" role="status">
              Hired, then retired: this agent is no longer on the floor, so its initial goal is not being worked.
            </p>
          ) : (
            <p className="hire-success" role="status">Hired. The agent and its initial goal are in the journal.</p>
          )}
          <dl className="hire-facts">
            <div><dt>AgentId</dt><dd data-hire-agent-id={outcome.agent}>{outcome.agent}</dd></div>
            <div><dt>GoalId</dt><dd data-hire-goal-id={outcome.goal}>{outcome.goal}</dd></div>
            <div><dt>On the floor</dt><dd>{presence === 'present' ? 'Yes, selected on the floor'
              : presence === 'absent' ? 'No, retired since the Hire' : 'Waiting for a fresh snapshot'}</dd></div>
            {row === null ? null : <div><dt>Current goal</dt><dd>{goalLine(row)}</dd></div>}
          </dl>
          {row === null || currentGoal === outcome.goal ? null : (
            <p className="hire-note">Its goal has changed since the Hire; the current goal is shown above.</p>
          )}
        </>
      );
    }
    case 'partial': {
      const presence = presenceOf(outcome.agent, floor, hire.askedAtEpoch);
      const row = floor.agents.find((agent) => agent.id === outcome.agent) ?? null;
      return (
        <>
          <p className="hire-warning" role="status">
            Hired without its goal. The agent is in the journal; its initial goal is not.
          </p>
          <dl className="hire-facts">
            <div><dt>AgentId</dt><dd data-hire-agent-id={outcome.agent}>{outcome.agent}</dd></div>
            <div><dt>Reason</dt><dd data-hire-reason={outcome.reason}>{PARTIAL_REASONS[outcome.reason]}</dd></div>
            <div><dt>Goal when answered</dt><dd>{outcome.currentGoal === null ? 'None'
              : `GoalId ${outcome.currentGoal}`}</dd></div>
            <div><dt>Retired when answered</dt><dd>{outcome.retired ? 'Yes' : 'No'}</dd></div>
            <div><dt>On the floor</dt><dd>{presence === 'present' ? 'Yes'
              : presence === 'absent' ? 'No, retired' : 'Waiting for a fresh snapshot'}</dd></div>
            {row === null ? null : <div><dt>Current goal</dt><dd>{goalLine(row)}</dd></div>}
          </dl>
          <p className="hire-note">
            {outcome.reason === 'session_expired' || outcome.reason === 'session_revoked'
              ? 'Paste a new link from dudes floor open into this tab’s address bar: this page keeps the '
                + 'Hire, and Check status then sets only the goal this Hire recorded.'
              : 'The Floor sets no other goal for it; an operator can give it one locally.'}
          </p>
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
        return <p className="hire-error" role="alert">{outcome.note}</p>;
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
                + 'agent may already have been hired. Paste a new link from dudes floor open into '
                + 'this tab’s address bar: this page keeps the Hire, and Check status asks again with the same '
                + 'attempt, which never hires twice.'}
          </p>
        );
      }
      const workspace = hire.attempt?.payload.workspace_id ?? '';
      const offered = floor.onboarding?.workspaces.some((space) => space.workspaceId === workspace) ?? false;
      return (
        <p className="hire-error" role="alert" data-hire-session={session}>
          {session === 'checking'
            ? 'The daemon refused this Hire before any effect. Checking whether this Floor session is still live…'
            : session === 'ended'
              ? 'Your Floor session has ended or was revoked. No agent was hired. Paste a new link from dudes floor '
                + 'open into this tab’s address bar: this page keeps the Hire, and Check status asks '
                + 'again with the same attempt.'
              : floor.onboarding === null
                ? 'Hiring is no longer available to this session. The daemon refused before any effect: the '
                  + 'session may have used its four hires, or its enrollment is no longer eligible. No agent was hired.'
                : !offered
                  ? `The project ${workspace} is no longer offered. No agent was hired.`
                  : 'The daemon refused this Hire before any effect. No agent was hired.'}
        </p>
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
        <p className="hire-error" role="alert">
          The daemon refused the request itself (HTTP {outcome.status}) before any effect. No agent was hired.
        </p>
      );
    case 'check':
      // Only an attempt an earlier reply left uncertain keeps a check refusal here; any other went back to the form.
      return (
        <p className="hire-error" role="alert" data-check-refusal={outcome.reason}>
          {checkRefusalText(outcome.reason, (hire.attempt as HireAttempt).checkLabel, 'hire')}
        </p>
      );
  }
}
