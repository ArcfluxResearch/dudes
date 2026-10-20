// SPDX-License-Identifier: MIT OR Apache-2.0

import { useId, useState, useSyncExternalStore } from 'react';

import {
  cancelSetting, changeSetting, confirmSetting, editLine, getSettingsEditState, limitChange,
  subscribeToSettingsEdit,
} from './edit.ts';
import type { LimitUnit } from './edit.ts';

import type { FloorState } from '../floor/store.ts';
import type { SettingsReport } from './report.ts';
import { OFFICES } from '../office3d/offices.ts';
import { formatUsdMicros } from '../onboarding/catalog.ts';
import { ReadNotice, snapshotReadState } from '../ReadError.tsx';
import { getOfficePin, setOfficePin, subscribeToOfficePin } from '../view.ts';
import { desktopNotificationPreference } from '../desktopNotifications.ts';
import { releaseNotesLabel, releaseNotesUrl } from '../releaseLinks.ts';
import { AddProject, RemoveNotice, RemoveProject } from '../project/AddProject.tsx';

const SWITCHES = [
  ['answer', 'Answer questions'], ['control', 'Pause, resume and rename'], ['recovery', 'Recover interrupted goals'], ['goal', 'Change goals'],
  ['retire', 'Retire agents'], ['terminal', 'Attach terminals'], ['git', 'Read checkouts in the Git tab'],
  ['hire', 'Hire agents'], ['renew', 'Start new goals'],
  ['board', 'Make tickets'], ['diff', 'Read ticket changes'], ['project', 'Add and remove projects'],
] as const;
const OFFICE_CHOICES = [{ id: null, label: 'Automatic' },
  ...OFFICES.map((office) => ({ id: office.id, label: `${office.definition.label}, ${office.capacity} desks` }))] as const;
const ENGINES = [['codex', 'Codex'], ['pi', 'Pi']] as const;

function ceiling(value: string | null, dollars = true): string {
  return value === null ? 'No ceiling' : dollars ? formatUsdMicros(value) : `${value} tokens`;
}

/** Why this session is offered no Hire, or `null` when it is offered one. */
export function hireUnavailable(floor: FloorState): string | null {
  if (floor.settings?.switches.hire === false) {
    return 'Hire is off. Turn on Hire agents under Floor switches in Settings.';
  }
  if (floor.onboarding !== null) return null;
  return 'Hire needs a project (add its folder in Settings), at least one engine offered for it, and a free Hire '
    + 'slot in this session. An engine is withdrawn when the daemon\'s configuration turns it off, or when the '
    + 'daemon holds a billing key for it and the project has no ceiling that key requires.';
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
  return <p className="settings-guidance">Terminal is off. Turn on Attach terminals under Floor switches in Settings.</p>;
}

export function SettingsView({ floor, onSetup }: {
  readonly floor: FloorState;
  readonly onSetup: () => void;
}) {
  const report = floor.settings;
  const state = snapshotReadState(floor, report !== null);
  if (state !== 'ready' || report === null) return <div className="settings-view"><DesktopNotificationChoice />
    <ReadNotice state={state} label="Settings"
      message="Settings are not reported by this daemon, or this page could not read them." /></div>;
  return <div className="settings-view">
    <DesktopNotificationChoice />
    <p>Turn a switch or an engine on or off here, and set the spending limits.</p>
    {floor.stale ? <p role="status">View unconfirmed. These values are from the last snapshot.</p> : null}
    <p>After a config edit, restart with <code>dudes quickstart --restart</code>, which ends any running turn.</p>
    <section aria-labelledby="settings-switches"><h3 id="settings-switches">Floor switches</h3>
      <dl><div><dt>Chat messages</dt><dd>Always on, with no setting</dd></div>
      {SWITCHES.map(([key, label]) => {
        const on = report.switches[key];
        return <div key={key}><dt>{label} <code>floor.{key}</code></dt>
          <dd>{key === 'diff' && on && !report.switches.git ? 'On, but needs Git' : on ? 'On' : 'Off'}
            {key === 'renew' && !on ? ' It needs goal change and Hire.' : null}
            <SwitchButton settingKey={`floor.${key}`} next={on ? 'off' : 'on'} /></dd></div>;
      })}</dl>
      <p>Recovery needs both <code>floor.recovery</code> and <code>floor.control</code>. It keeps existing budgets and admission gates.
        Receipt lookup remains read-only when recovery is off.</p>
    </section>
    <section aria-labelledby="settings-budget"><h3 id="settings-budget">Budget ceilings and per-turn reserves</h3>
      <dl>
        <div><dt>Floor daily dollars</dt><dd>{ceiling(report.budget.floor_usd_per_day_micros)}
          <LimitEditor settingKey="budget.floor_usd_per_day" unit="usd" label="Floor daily dollars"
            current={report.budget.floor_usd_per_day_micros} /></dd></div>
        <div><dt>Per-turn dollar reserve</dt><dd>{formatUsdMicros(report.budget.per_turn_usd_micros)}
          <LimitEditor settingKey="budget.per_turn_floor.usd" unit="usd" label="Per-turn dollar reserve"
            current={report.budget.per_turn_usd_micros} /></dd></div>
        <div><dt>Per-turn token reserve</dt><dd>{report.budget.per_turn_tokens} tokens
          <LimitEditor settingKey="budget.per_turn_floor.tokens" unit="tokens" label="Per-turn token reserve"
            current={report.budget.per_turn_tokens} /></dd></div>
      </dl>
      <p>When unset, the per-turn reserves default to $0.25 and 20000 tokens. Raising a limit, or setting one where
        there was none, asks first. This page cannot remove a limit once it is set, and a reserve has to stay above zero.</p>
    </section>
    <section aria-labelledby="settings-providers"><h3 id="settings-providers">Registered providers</h3>
      <p>Registration does not confirm that the program is installed. <button type="button" onClick={onSetup}>Open Setup</button></p>
      <ul>{report.providers.filter((provider) => !ENGINES.some(([id]) => id === provider)).map((provider) =>
        <li key={provider}><button type="button" onClick={onSetup} aria-label={`Check ${provider} in Setup`}>{provider}</button>
          <span> Always registered.</span></li>)}
        {ENGINES.map(([id, label]) => {
          const on = report.providers.includes(id);
          return <li key={id}><button type="button" onClick={onSetup} aria-label={`Check ${id} in Setup`}>{id}</button>
            <span> {label} is {on ? 'on' : 'off'}.</span>
            <SwitchButton settingKey={`providers.${id}`} next={on ? 'off' : 'on'} /></li>;
        })}</ul>
    </section>
    <WorkspaceSettings report={report} floor={floor} />
    <UnavailableControls floor={floor} control="hire" />
    <OfficeLayoutChoice />
    <section className="settings-about" aria-labelledby="settings-about"><h3 id="settings-about">About</h3>
      {report.about.daemon_version === __FLOOR_BUILD_VERSION__ ? null : <p role="status">
        This page and daemon have different builds. The page is {__FLOOR_BUILD_VERSION__} and the daemon is {report.about.daemon_version}.
        {' '}Open Dudes to load the matching page. Keep any draft text before leaving this page.
      </p>}
      <dl><div><dt>Daemon</dt><dd>{report.about.daemon_version}</dd></div>
        <div><dt>Floor build</dt><dd>{__FLOOR_BUILD_VERSION__}</dd></div>
        <div><dt>Browser wire</dt><dd>browser-wire-v{report.about.browser_wire_version}</dd></div></dl>
      <ul>
        <li><a href={releaseNotesUrl(report.about.daemon_version)} target="_blank" rel="noopener noreferrer">
          {releaseNotesLabel(report.about.daemon_version)}</a></li>
        <li><a href="https://github.com/ArcfluxResearch/dudes" target="_blank" rel="noopener noreferrer">Source repository</a></li>
        <li><a href="https://github.com/ArcfluxResearch/dudes/issues" target="_blank" rel="noopener noreferrer">Issue tracker</a></li>
      </ul>
      <p>This page never checks for updates. To look for a newer version, run <code>dudes upgrade --check</code>.</p>
    </section>
  </div>;
}

/** Turns one switch or engine on or off through the daemon, which keeps the file's start rules. */
function SwitchButton({ settingKey, next }: { readonly settingKey: string; readonly next: 'on' | 'off' }) {
  const edit = useSyncExternalStore(subscribeToSettingsEdit, getSettingsEditState, getSettingsEditState);
  return <>
    <button type="button" aria-disabled={edit.sending !== null || undefined}
      onClick={() => { void changeSetting(settingKey, next); }}>Turn {next}</button>
    <EditOutcome settingKey={settingKey} label={`Turn ${next} ${settingKey}`} />
  </>;
}

/** The question the page is asking about this setting, and the sentence after its last request. */
function EditOutcome({ settingKey, label }: { readonly settingKey: string; readonly label: string }) {
  const edit = useSyncExternalStore(subscribeToSettingsEdit, getSettingsEditState, getSettingsEditState);
  return <>
    {edit.asking?.key === settingKey ? <span role="group" aria-label={label}> {edit.asking.question}
      {' '}<button type="button" onClick={() => { void confirmSetting(); }}>{edit.asking.confirm}</button>
      {' '}<button type="button" onClick={() => cancelSetting()}>Cancel</button></span> : null}
    {edit.last?.key === settingKey ? <span role="status"> {editLine(edit.last.result)}</span> : null}
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
    <button type="submit" aria-disabled={edit.sending !== null || typed.trim() === '' || undefined}>Save</button>
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
  return <section aria-labelledby={headingId}><h3 id={headingId}>Enrolled projects</h3>
      {report.workspaces.length === 0 ? <p>No enrolled projects.</p> : <ul>{report.workspaces.map((workspace) =>
        <li key={workspace.workspace_id}><h4>{workspace.label}</h4><p>Project <code>{workspace.workspace_id}</code></p>
          <dl>
            <div><dt>Agent daily dollar ceiling</dt><dd>{ceiling(workspace.agent_usd_per_day_micros)}</dd></div>
            <div><dt>Agent daily token ceiling</dt><dd>{ceiling(workspace.agent_tokens_per_day, false)}</dd></div>
            <div><dt>Initial goal dollar ceiling</dt><dd>{ceiling(workspace.goal_usd_micros)}</dd></div>
            <div><dt>Initial goal token ceiling</dt><dd>{ceiling(workspace.goal_tokens, false)}</dd></div>
          </dl>
          {floor === undefined ? null
            : <RemoveProject floor={floor} projectId={workspace.workspace_id} label={workspace.label} />}
        </li>)}</ul>}
      <RemoveNotice />
      {floor === undefined ? null : <AddProject floor={floor} off={<p>Adding a project from this page is off. Turn on the project switch under Floor switches.</p>} />}
      <p>For a config-file edit, use <code>dudes project reload</code>, then <code>dudes project reload --apply &lt;sha256&gt;</code>.</p>
    </section>;
}
