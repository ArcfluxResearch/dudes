// SPDX-License-Identifier: MIT OR Apache-2.0

import { useId, useState, useSyncExternalStore } from 'react';

import {
  cancelSetting, changeSetting, confirmSetting, editLine, getSettingsEditState, limitChange,
  pageDrawsTerminal, RELOAD_FOR_TERMINAL, subscribeToSettingsEdit,
} from './edit.ts';
import type { LimitUnit } from './edit.ts';

import type { FloorState } from '../floor/store.ts';
import type { SettingsReport } from './report.ts';
import { engineName } from '../engines.ts';
import { OFFICES } from '../office3d/offices.ts';
import { formatUsdMicros } from '../onboarding/catalog.ts';
import { ReadNotice, snapshotReadState } from '../ReadError.tsx';
import { getOfficePin, setOfficePin, subscribeToOfficePin } from '../view.ts';
import { desktopNotificationPreference } from '../desktopNotifications.ts';
import { releaseNotesLabel, releaseNotesUrl } from '../releaseLinks.ts';
import { Disclosure, Panel, Switch } from '../../ui/index.ts';
import { DevicesSection } from '../devices/DevicesSection.tsx';
import { AddProject, RemoveNotice, RemoveProject } from '../project/AddProject.tsx';

const SWITCHES = [
  ['answer', 'Answer questions', "Lets this page answer an agent's question for you."],
  ['control', 'Pause, resume and rename', 'Lets this page pause an agent, resume it, or change its name.'],
  ['recovery', 'Recover interrupted tasks', 'Lets this page pick up work an agent was doing when it was interrupted.'],
  ['goal', 'Change tasks', "Lets this page change what an agent is working toward."],
  ['retire', 'Retire agents', 'Lets this page retire an agent, which takes it off the Floor.'],
  ['terminal', 'Attach terminals', "Lets this page open a terminal in an agent's project folder."],
  ['git', 'Show changed files', 'Lets this page read which files an agent has changed in its project.'],
  ['hire', 'Hire agents', 'Lets this page hire a new agent.'],
  ['renew', 'Start new tasks', 'Lets this page give an agent a new task after the last one ends.'],
  ['board', 'Make tickets', 'Lets this page create tickets and give them to agents.'],
  ['diff', 'Read ticket changes', "Lets a ticket show the changes the agent made."],
  ['project', 'Add and remove projects', 'Lets this page add a project folder or take one away.'],
] as const;
const OFFICE_CHOICES = [{ id: null, label: 'Automatic' },
  ...OFFICES.map((office) => ({ id: office.id, label: `${office.definition.label}, ${office.capacity} desks` }))] as const;
const ENGINES = [['codex', 'Codex'], ['pi', 'Pi']] as const;

function spendingLimit(value: string | null, dollars = true): string {
  return value === null ? 'No limit' : dollars ? formatUsdMicros(value) : `${value} tokens`;
}

/** Why this session is offered no Hire, or `null` when it is offered one. */
export function hireUnavailable(floor: FloorState): string | null {
  if (floor.settings?.switches.hire === false) {
    return 'Hire is off. Turn on Hire agents under Floor switches in Settings.';
  }
  if (floor.onboarding !== null) return null;
  return 'Hire is unavailable. Check Engines and Projects in Settings.';
}

/** The drawer's controls whose switch the daemon reports off, so the drawer says how to turn each on. */
export function switchedOffControls(floor: FloorState): readonly ('retire' | 'terminal')[] {
  if (!floor.settings) return [];
  return (['retire', 'terminal'] as const).filter((control) => !floor[control]);
}

export function UnavailableControls({ floor, control }: {
  readonly floor: FloorState;
  readonly control: 'hire' | 'retire' | 'terminal';
}) {
  if (control === 'hire') {
    const reason = hireUnavailable(floor);
    return reason === null ? null : <p className="settings-guidance">{reason}</p>;
  }
  if (!switchedOffControls(floor).includes(control)) return null;
  if (control === 'retire') {
    return <p className="settings-guidance">Retire is off. Turn on Pause, resume and rename and Retire agents under
      Floor switches in Settings.</p>;
  }
  return <p className="settings-guidance">To open the real CLI, turn on Attach terminals under Floor switches in Settings.</p>;
}

export function SettingsView({ floor, onSetup }: {
  readonly floor: FloorState;
  readonly onSetup: () => void;
}) {
  const edit = useSyncExternalStore(subscribeToSettingsEdit, getSettingsEditState, getSettingsEditState);
  const report = floor.settings;
  const state = snapshotReadState(floor, report !== null);
  if (state !== 'ready' || report === null) return <div className="settings-view"><DesktopNotificationChoice />
    <ReadNotice state={state} label="Settings"
      message="Settings are not reported by Dudes, or this page could not read them." /></div>;
  return <div className="settings-view">
    <p>Manage engines, projects and spending. Choices for this browser are below.</p>
    {floor.stale ? <p role="status">Connection interrupted. These settings may be out of date.</p> : null}
    <section aria-labelledby="settings-providers"><h3 id="settings-providers">Engines</h3>
      <p>Check which engines Dudes can find. Sign in when you hire an agent. <button type="button" onClick={onSetup}>Open Setup</button></p>
      <ul className="settings-engines">{report.providers.filter((provider) => !ENGINES.some(([id]) => id === provider)).map((provider) =>
        <li key={provider}><button type="button" onClick={onSetup} aria-label={`Check ${engineName(provider)} in Setup`}>{engineName(provider)}</button>
          <span>Available in Setup.</span></li>)}
        {ENGINES.map(([id, label]) => {
          const on = report.providers.includes(id);
          return <li key={id}><button type="button" onClick={onSetup} aria-label={`Check ${label} in Setup`}>{label}</button>
            <span> {label} is {on ? 'on' : 'off'}.</span>
            <SwitchButton settingKey={`providers.${id}`} label={label} next={on ? 'off' : 'on'} /></li>;
        })}</ul>
    </section>
    <WorkspaceSettings report={report} floor={floor} />
    <section aria-labelledby="settings-budget"><h3 id="settings-budget">Spending limits</h3>
      <p className="settings-spending-line">Daily spending limit: {spendingLimit(report.budget.floor_usd_per_day_micros)}.
        {report.budget.floor_usd_per_day_micros !== null && edit.pendingDailyCeilingClear
          ? <span role="status"> Saved from this page. No daily dollar limit after restart.</span> : null}</p>
      <details className="settings-spending"><summary>Change</summary>
        <dl>
          <div><dt>Daily dollar limit</dt><dd>{spendingLimit(report.budget.floor_usd_per_day_micros)}
            <LimitEditor settingKey="budget.floor_usd_per_day" unit="usd" label="Daily dollar limit"
              current={report.budget.floor_usd_per_day_micros} /></dd></div>
          <div><dt>Available dollars needed to start work</dt><dd>{formatUsdMicros(report.budget.per_turn_usd_micros)}
            <LimitEditor settingKey="budget.per_turn_floor.usd" unit="usd" label="Available dollars needed to start work"
              current={report.budget.per_turn_usd_micros} /></dd></div>
          <div><dt>Available tokens needed to start work</dt><dd>{report.budget.per_turn_tokens} tokens
            <LimitEditor settingKey="budget.per_turn_floor.tokens" unit="tokens" label="Available tokens needed to start work"
              current={report.budget.per_turn_tokens} /></dd></div>
        </dl>
        <p>Raising a limit or clearing the daily limit asks you to confirm. The minimum for each work attempt must stay above zero.
          The defaults are $0.25 and 20,000 tokens.</p>
      </details>
    </section>
    <DesktopNotificationChoice />
    <DevicesSection />
    <FloorSwitches report={report} />
    <UnavailableControls floor={floor} control="hire" />
    <OfficeLayoutChoice />
    <section className="settings-about" aria-labelledby="settings-about"><h3 id="settings-about">About</h3>
      {report.about.daemon_version === __FLOOR_BUILD_VERSION__ ? null : <p role="status">
        This page is version {__FLOOR_BUILD_VERSION__}, but Dudes is version {report.about.daemon_version}.
        {' '}Open Dudes to load the matching page. Keep any draft text before leaving this page.
      </p>}
      <details><summary>Build details</summary><dl><div><dt>Daemon</dt><dd>{report.about.daemon_version}</dd></div>
        <div><dt>Floor build</dt><dd>{__FLOOR_BUILD_VERSION__}</dd></div>
        <div><dt>Browser wire</dt><dd>browser-wire-v{report.about.browser_wire_version}</dd></div></dl></details>
      <ul>
        <li><a href={releaseNotesUrl(report.about.daemon_version)} target="_blank" rel="noopener noreferrer">
          {releaseNotesLabel(report.about.daemon_version)}</a></li>
        <li><a href="https://github.com/ArcfluxResearch/dudes" target="_blank" rel="noopener noreferrer">Source repository</a></li>
        <li><a href="https://github.com/ArcfluxResearch/dudes/issues" target="_blank" rel="noopener noreferrer">Issue tracker</a></li>
      </ul>
      <p>This page does not check for updates automatically.</p>
      <details><summary>Update and restart details</summary>
        <p>To check for a newer version, run <code>dudes upgrade --check</code>.</p>
        <p>To restart after a configuration edit, run <code>dudes quickstart --restart</code>. A restart ends any running turn.</p>
      </details>
    </section>
  </div>;
}

export function FloorSwitches({ report, headingId = 'settings-switches' }: {
  readonly report: SettingsReport;
  readonly headingId?: string;
}) {
  const edit = useSyncExternalStore(subscribeToSettingsEdit, getSettingsEditState, getSettingsEditState);
  return <Panel title="Floor switches" headingId={headingId} className="floor-switches">
    <ul className="ui m-0 grid list-none divide-y divide-line p-0">
      <li className="pb-3">
        <div className="grid gap-0.5">
          <span className="text-base font-medium text-ink">Chat messages</span>
          <span className="text-small text-muted">Always on, with no setting</span>
        </div>
      </li>
      {SWITCHES.map(([key, label, says]) => {
        const on = report.switches[key];
        const needs = key === 'diff' && on && !report.switches.git ? 'On, but it needs Show changed files.'
          : key === 'renew' && !on ? 'It needs Change tasks and Hire agents.' : null;
        return <li key={key} className="py-3 last:pb-0" data-switch={key}>
          <Switch checked={on} label={label} aria-disabled={edit.sending !== null || undefined}
            title={edit.sending === null ? undefined : 'Wait for the current setting to be saved.'}
            description={needs === null ? says : <>{says} {needs}</>}
            onCheckedChange={(next) => { if (edit.sending === null) void changeSetting(`floor.${key}`, next ? 'on' : 'off'); }} />
          <div className="mt-2 text-small text-muted empty:hidden"><EditOutcome settingKey={`floor.${key}`} label={`Turn ${on ? 'off' : 'on'} ${label}`} /></div>
        </li>;
      })}
    </ul>
    <p className="mt-4 mb-0 text-small text-muted">Recovery also needs Pause, resume and rename. Spending limits still apply.</p>
    <Disclosure className="mt-2" summary="Configuration details">
      <ul className="ui m-0 grid list-none gap-1 p-0">{SWITCHES.map(([key, label]) =>
        <li key={key} className="flex flex-wrap items-baseline justify-between gap-2 text-small text-muted"><span>{label}</span><code className="font-mono">floor.{key}</code></li>)}</ul>
    </Disclosure>
  </Panel>;
}

/** Turns one switch or engine on or off through the daemon, which keeps the file's start rules. */
function SwitchButton({ settingKey, label, next }: { readonly settingKey: string; readonly label: string; readonly next: 'on' | 'off' }) {
  const edit = useSyncExternalStore(subscribeToSettingsEdit, getSettingsEditState, getSettingsEditState);
  return <>
    <button type="button" aria-disabled={edit.sending !== null || undefined}
      title={edit.sending === null ? undefined : 'Wait for the current setting to be saved.'}
      onClick={() => { void changeSetting(settingKey, next); }}>Turn {next}</button>
    <EditOutcome settingKey={settingKey} label={`Turn ${next} ${label}`} />
  </>;
}

/** The question the page is asking about this setting, and the sentence after its last request. */
function EditOutcome({ settingKey, label }: { readonly settingKey: string; readonly label: string }) {
  const edit = useSyncExternalStore(subscribeToSettingsEdit, getSettingsEditState, getSettingsEditState);
  return <>
    {edit.asking?.key === settingKey ? <span role="group" aria-label={label}> {edit.asking.question}
      {' '}<button type="button" onClick={() => { void confirmSetting(); }}>{edit.asking.confirm}</button>
      {' '}<button type="button" onClick={() => cancelSetting()}>Cancel</button></span> : null}
    {edit.last?.key === settingKey ? <>
      <span role="status"> {editLine(edit.last.result)}</span>
      {settingKey === 'floor.terminal' && edit.last.value === 'on' && edit.last.result.kind === 'saved' && !pageDrawsTerminal()
        ? <span role="status"> {RELOAD_FOR_TERMINAL}</span> : null}
      {edit.last.result.kind === 'refused' ? <details><summary>Details</summary>
        <p>Reason: <code>{edit.last.result.reason}</code></p>
        {edit.last.result.detail === null ? null : <p>{edit.last.result.detail}</p>}
      </details> : edit.last.result.kind === 'saved' && edit.last.result.restart.length > 0
        ? <details><summary>Details</summary><p>Restart needed for <code>{edit.last.result.restart.join(', ')}</code>.</p></details> : null}
    </> : null}
  </>;
}

const LIMIT_HINT = { usd: 'Enter dollars, such as 5 or 2.50.', tokens: 'Enter a whole number of tokens.' } as const;

/** A box for one limit. It asks before the limit goes up and says nothing when it goes down. */
function LimitEditor({ settingKey, unit, label, current }: {
  readonly settingKey: string; readonly unit: LimitUnit; readonly label: string; readonly current: string | null;
}) {
  const edit = useSyncExternalStore(subscribeToSettingsEdit, getSettingsEditState, getSettingsEditState);
  const [typed, setTyped] = useState('');
  const [problem, setProblem] = useState<string | null>(null);
  const id = useId();
  const dailyCeiling = settingKey === 'budget.floor_usd_per_day';
  return <form className="settings-limit" onSubmit={(event) => {
    event.preventDefault();
    if (edit.sending !== null) return;
    const change = limitChange(unit, typed, current);
    if (change.kind === 'invalid') { setProblem(LIMIT_HINT[unit]); return; }
    setProblem(null);
    if (change.kind === 'same') { setProblem('That is already the limit.'); return; }
    void changeSetting(settingKey, change.value, change.question, current === null ? 'Set limit' : 'Raise limit');
  }}>
    <label htmlFor={id}>New {label.toLowerCase()}</label>
    <input id={id} type="text" inputMode="decimal" autoComplete="off" spellCheck={false} value={typed}
      onChange={(event) => { setTyped(event.target.value); setProblem(null); }} />
    <button type="submit" aria-disabled={edit.sending !== null || typed.trim() === '' || undefined}
      title={edit.sending !== null ? 'Wait for the current setting to be saved.' : typed.trim() === '' ? 'Enter a new limit first.' : undefined}>Save</button>
    {dailyCeiling && current !== null ? <button type="button" className="settings-clear-limit"
      aria-disabled={edit.sending !== null || undefined}
      title={edit.sending === null ? undefined : 'Wait for the current setting to be saved.'}
      onClick={() => { void changeSetting(settingKey, 'unset',
        `This removes the daily dollar limit of ${formatUsdMicros(current)}. After a restart, there will be no daily dollar limit.`, 'Clear limit'); }}>Clear limit</button> : null}
    {problem === null ? null : <span role="alert"> {problem}</span>}
    <EditOutcome settingKey={settingKey} label={`Change ${label}`} />
  </form>;
}

export function DesktopNotificationChoice() {
  const notifications = useSyncExternalStore(desktopNotificationPreference.subscribe,
    desktopNotificationPreference.getState, desktopNotificationPreference.getState);
  const on = notifications.kind === 'on';
  return <section aria-labelledby="settings-notifications"><h3 id="settings-notifications">Desktop notifications</h3>
    <p>Get a desktop notification when a ticket is ready, needs you, or an agent asks a question.
      This browser keeps the choice. Keep the Floor tab open.</p>
    <button type="button" aria-pressed={on} disabled={notifications.kind === 'unsupported' || notifications.kind === 'asking'}
      title={notifications.kind === 'unsupported' ? 'This browser does not support desktop notifications.'
        : notifications.kind === 'asking' ? 'Answer the notification permission request in your browser.' : undefined}
      onClick={() => { void desktopNotificationPreference.setEnabled(!on); }}>
      {on ? 'Turn notifications off' : notifications.kind === 'asking' ? 'Waiting for browser permission' : 'Turn notifications on'}
    </button>
    {notifications.kind === 'denied' ? <p role="status">Notifications are blocked. Allow notifications for this page in your
      browser's site settings, then turn them on here. The banner still shows what needs you.</p>
      : notifications.kind === 'unsupported' || notifications.kind === 'unavailable' ? <p role="status">
        Desktop notifications are unavailable in this browser. The banner still shows what needs you.</p> : null}
    {!notifications.remembered ? <p role="status">This choice lasts only for this page because browser storage is unavailable.</p> : null}
  </section>;
}

/** A viewer preference kept in this browser, like the theme: the daemon never sees it. */
function OfficeLayoutChoice() {
  const pin = useSyncExternalStore(subscribeToOfficePin, getOfficePin, getOfficePin);
  return <section aria-labelledby="settings-office"><h3 id="settings-office">Office layout</h3>
    <p>Automatic draws the smallest office that seats the whole crew. Pin an office to keep it whatever the crew
      size. This browser keeps the choice.</p>
    <div className="settings-office" role="group" aria-labelledby="settings-office">
      {OFFICE_CHOICES.map(({ id, label }) => <button key={label} type="button" aria-pressed={pin === id}
        onClick={() => setOfficePin(id)}>{label}</button>)}
    </div>
  </section>;
}

export function WorkspaceSettings({ report, floor, headingId = 'settings-workspaces' }: {
  readonly report: SettingsReport;
  readonly floor?: FloorState;
  readonly headingId?: string;
}) {
  return <section aria-labelledby={headingId}><h3 id={headingId}>Projects</h3>
      {report.workspaces.length === 0 ? <p>No projects yet.</p> : <ul>{report.workspaces.map((workspace) =>
        <li key={workspace.workspace_id}><h4>{workspace.label}</h4><details><summary>Project details</summary><p>Project <code>{workspace.workspace_id}</code></p></details>
          <details><summary>Spending details</summary><dl>
            <div><dt>Daily dollar limit per agent</dt><dd>{spendingLimit(workspace.agent_usd_per_day_micros)}</dd></div>
            <div><dt>Daily token limit per agent</dt><dd>{spendingLimit(workspace.agent_tokens_per_day, false)}</dd></div>
            <div><dt>Dollar limit for a new task</dt><dd>{spendingLimit(workspace.goal_usd_micros)}</dd></div>
            <div><dt>Token limit for a new task</dt><dd>{spendingLimit(workspace.goal_tokens, false)}</dd></div>
          </dl></details>
          {floor === undefined ? null
            : <RemoveProject floor={floor} projectId={workspace.workspace_id} label={workspace.label} />}
        </li>)}</ul>}
      <RemoveNotice />
      {floor === undefined ? null : <AddProject floor={floor} off={<p>Adding a project from this page is off. Turn on the project switch under Floor switches.</p>} />}
      <details><summary>Configuration details</summary><p>For a config-file edit, use <code>dudes project reload</code>, then <code>dudes project reload --apply &lt;sha256&gt;</code>.</p></details>
    </section>;
}
