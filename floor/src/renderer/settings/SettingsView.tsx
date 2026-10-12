// SPDX-License-Identifier: MIT OR Apache-2.0

import { useSyncExternalStore } from 'react';

import type { FloorState } from '../floor/store.ts';
import type { SettingsReport } from './report.ts';
import { OFFICES } from '../office3d/offices.ts';
import { formatUsdMicros } from '../onboarding/catalog.ts';
import { ReadNotice, snapshotReadState } from '../ReadError.tsx';
import { getOfficePin, setOfficePin, subscribeToOfficePin } from '../view.ts';
import { desktopNotificationPreference } from '../desktopNotifications.ts';

const SWITCHES = [
  ['answer', 'Answer questions'], ['control', 'Pause, resume and rename'], ['goal', 'Change goals'],
  ['retire', 'Retire agents'], ['terminal', 'Attach terminals'], ['git', 'Read checkouts in the Git tab'],
  ['hire', 'Hire agents'], ['renew', 'Start new goals'],
] as const;
const OFFICE_CHOICES = [{ id: null, label: 'Automatic' },
  ...OFFICES.map((office) => ({ id: office.id, label: `${office.definition.label}, ${office.capacity} desks` }))] as const;
const REMOVE_RESERVED_IDS = new Set(['-h', '--help', '--json', '--']);
export const ADD_WORKSPACE = 'dudes workspace add <path> --id <id> --label <label> --completion-check-label <check> '
  + '--department <department> --hop-cap <n> --timeout-s <seconds> [--agent-usd-per-day <usd>] [--goal-usd <usd>] '
  + '[--tokens-per-day <tokens>] [--goal-tokens <tokens>] -- <program> [args...]';

function ceiling(value: string | null, dollars = true): string {
  return value === null ? 'No ceiling' : dollars ? formatUsdMicros(value) : `${value} tokens`;
}

/** Why this session is offered no Hire, or `null` when it is offered one. */
export function hireUnavailable(floor: FloorState): string | null {
  if (floor.settings?.switches.hire === false) {
    return 'Hire is off. Turn it on with dudes config set floor.hire on, then restart the daemon.';
  }
  if (floor.onboarding !== null) return null;
  return 'Hire needs a workspace enrolled with dudes workspace add, at least one engine offered for it, and a free Hire '
    + 'slot in this session. An engine is withdrawn when the daemon\'s configuration turns it off, or when the '
    + 'daemon holds a billing key for it and the workspace has no ceiling that key requires.';
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
    return <p className="settings-guidance">Retire needs <code>floor.control</code> and <code>floor.retire</code>.
      Run <code>dudes config set floor.control on floor.retire on</code>, then restart the daemon.</p>;
  }
  return <p className="settings-guidance">Terminal needs <code>floor.terminal</code>.
    Run <code>dudes config set floor.terminal on</code>, then restart the daemon.</p>;
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
    <p>The Floor only reads daemon settings. Run the commands locally to change them.</p>
    {floor.stale ? <p role="status">View unconfirmed. These values are from the last snapshot.</p> : null}
    <p>After a config edit, restart with <code>dudes quickstart --restart</code>, which ends any running turn.
      Workspace commands apply enrollment changes to the running daemon.</p>
    <section aria-labelledby="settings-switches"><h3 id="settings-switches">Floor switches</h3>
      <dl><div><dt>Chat messages</dt><dd>Always on, with no setting</dd></div>
      {SWITCHES.map(([key, label]) => {
        const next = report.switches[key] ? 'off' : 'on';
        const renewOff = report.switches.renew ? ' floor.renew off' : '';
        const companions = (key === 'goal' || key === 'retire') && next === 'on' ? ' floor.control on'
          : key === 'control' && next === 'off' ? ` floor.goal off floor.retire off${renewOff}`
            : (key === 'goal' || key === 'hire') && next === 'off' ? renewOff : '';
        if (key === 'renew' && !report.switches.renew) {
          return <div key={key}><dt>{label} <code>floor.{key}</code></dt>
            <dd>Renew is off. Turn it on with <code>dudes config set floor.renew on</code>, then restart the daemon.
              It needs goal change and Hire.</dd></div>;
        }
        return <div key={key}><dt>{label} <code>floor.{key}</code></dt>
          <dd>{report.switches[key] ? 'On' : 'Off'}<code>dudes config set floor.{key} {next}{companions}</code></dd></div>;
      })}</dl>
    </section>
    <section aria-labelledby="settings-budget"><h3 id="settings-budget">Budget ceilings and per-turn reserves</h3>
      <dl>
        <div><dt>Floor daily dollars</dt><dd>{ceiling(report.budget.floor_usd_per_day_micros)}
          <code>dudes config set budget.floor_usd_per_day &lt;usd&gt;</code>
          <code>dudes config unset budget.floor_usd_per_day</code></dd></div>
        <div><dt>Per-turn dollar reserve</dt><dd>{formatUsdMicros(report.budget.per_turn_usd_micros)}
          <code>dudes config set budget.per_turn_floor.usd &lt;usd&gt;</code></dd></div>
        <div><dt>Per-turn token reserve</dt><dd>{report.budget.per_turn_tokens} tokens
          <code>dudes config set budget.per_turn_floor.tokens &lt;tokens&gt;</code></dd></div>
      </dl>
      <p>When unset, the per-turn reserves default to $0.25 and 20000 tokens.</p>
    </section>
    <section aria-labelledby="settings-providers"><h3 id="settings-providers">Registered providers</h3>
      <p>Registration does not confirm that the program is installed. <button type="button" onClick={onSetup}>Open Setup</button></p>
      {report.providers.length === 0 ? <p>No providers registered.</p> : <ul>{report.providers.map((provider) =>
        <li key={provider}><button type="button" onClick={onSetup} aria-label={`Check ${provider} in Setup`}>{provider}</button>
          {provider === 'codex' || provider === 'pi' ? <code>dudes config set providers.{provider} off</code>
            : <span> Default registration. No <code>dudes config</code> switch.</span>}</li>)}</ul>}
      <p>Register optional providers with <code>dudes config set providers.codex on</code> or
        <code> dudes config set providers.pi on</code>, then restart.</p>
    </section>
    <WorkspaceSettings report={report} />
    <UnavailableControls floor={floor} control="hire" />
    <OfficeLayoutChoice />
    <section className="settings-about" aria-labelledby="settings-about"><h3 id="settings-about">About</h3>
      {report.about.daemon_version === __FLOOR_BUILD_VERSION__ ? null : <p role="status">
        This page and daemon have different builds. The page is {__FLOOR_BUILD_VERSION__} and the daemon is {report.about.daemon_version}.
        {' '}Run <code>dudes floor open</code> to load the matching page. Keep any draft text before leaving this page.
      </p>}
      <dl><div><dt>Daemon</dt><dd>{report.about.daemon_version}</dd></div>
        <div><dt>Floor build</dt><dd>{__FLOOR_BUILD_VERSION__}</dd></div>
        <div><dt>Browser wire</dt><dd>browser-wire-v{report.about.browser_wire_version}</dd></div></dl>
    </section>
  </div>;
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

export function WorkspaceSettings({ report, onCopyAdd, headingId = 'settings-workspaces' }: {
  readonly report: SettingsReport;
  readonly onCopyAdd?: () => void;
  readonly headingId?: string;
}) {
  return <section aria-labelledby={headingId}><h3 id={headingId}>Enrolled workspaces</h3>
      {report.workspaces.length === 0 ? <p>No enrolled workspaces.</p> : <ul>{report.workspaces.map((workspace) =>
        <li key={workspace.workspace_id}><h4>{workspace.label}</h4><p>Workspace <code>{workspace.workspace_id}</code></p>
          <dl>
            <div><dt>Agent daily dollar ceiling</dt><dd>{ceiling(workspace.agent_usd_per_day_micros)}</dd></div>
            <div><dt>Agent daily token ceiling</dt><dd>{ceiling(workspace.agent_tokens_per_day, false)}</dd></div>
            <div><dt>Initial goal dollar ceiling</dt><dd>{ceiling(workspace.goal_usd_micros)}</dd></div>
            <div><dt>Initial goal token ceiling</dt><dd>{ceiling(workspace.goal_tokens, false)}</dd></div>
          </dl>
          {REMOVE_RESERVED_IDS.has(workspace.workspace_id) ? <>
            <p>For this identifier, edit its entry in your local config, then check and apply the enrollment change.</p>
            <code>dudes workspace reload</code><code>dudes workspace reload --apply &lt;sha256&gt;</code>
          </> : <>
            <p>To replace this label or these ceilings, remove its enrollment, then add it with the new values.</p>
            <code>dudes workspace remove {workspace.workspace_id}</code>
          </>}
        </li>)}</ul>}
      <p>Choose your own values for the placeholders.</p><code>{ADD_WORKSPACE}</code>
      {onCopyAdd === undefined ? null : <button type="button" aria-label="Copy workspace add command" onClick={onCopyAdd}>Copy</button>}
      <p>Inspect enrollment with <code>dudes workspace list</code>. For a config-file edit, use
        <code> dudes workspace reload</code>, then <code>dudes workspace reload --apply &lt;sha256&gt;</code>.</p>
    </section>;
}
