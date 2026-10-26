// SPDX-License-Identifier: MIT OR Apache-2.0

import { useId, useState, useSyncExternalStore } from 'react';
import type { ReactNode } from 'react';

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
import { Bell, BellOff } from 'lucide-react';

import { Button, Disclosure, Field, ICON, Input, Switch, cn } from '../../ui/index.ts';
import { Code, Fact, Facts, Group, HINT, LINK, Notice } from '../PageParts.tsx';
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
    return reason === null ? null : <Notice>{reason}</Notice>;
  }
  if (!switchedOffControls(floor).includes(control)) return null;
  if (control === 'retire') {
    return <Notice>Retire is off. Turn on Pause, resume and rename and Retire agents under
      Floor switches in Settings.</Notice>;
  }
  return <Notice>To open the real CLI, turn on Attach terminals under Floor switches in Settings.</Notice>;
}

const TEXT_LINK_LIST = 'm-0 grid list-none gap-1 p-0 text-base';

export function SettingsView({ floor, onSetup }: {
  readonly floor: FloorState;
  readonly onSetup: () => void;
}) {
  const edit = useSyncExternalStore(subscribeToSettingsEdit, getSettingsEditState, getSettingsEditState);
  const report = floor.settings;
  const state = snapshotReadState(floor, report !== null);
  if (state !== 'ready' || report === null) return <div data-ui="settings" className="grid gap-4 font-sans text-ink"><DesktopNotificationChoice />
    <ReadNotice state={state} label="Settings"
      message="Settings are not reported by Dudes, or this page could not read them." /></div>;
  return <div data-ui="settings" className="grid gap-4 font-sans text-ink">
    <p className={HINT}>Manage engines, projects and spending. Choices for this browser are below.</p>
    {floor.stale ? <p role="status" className={HINT}>Connection interrupted. These settings may be out of date.</p> : null}
    <Group title="Engines" headingId="settings-providers"
      description="Check which engines Dudes can find. Sign in when you hire an agent."
      action={<Button size="sm" onClick={onSetup}>Open Setup</Button>}>
      <ul className="m-0 grid list-none divide-y divide-line p-0">
        {report.providers.filter((provider) => !ENGINES.some(([id]) => id === provider)).map((provider) =>
          <li key={provider} className="flex flex-wrap items-center justify-between gap-2 py-3 first:pt-0 last:pb-0">
            <div className="grid gap-0.5">
              <span className="text-base font-medium">{engineName(provider)}</span>
              <span className="text-small text-muted">Available in Setup.</span>
            </div>
            <Button variant="ghost" size="sm" onClick={onSetup} aria-label={`Check ${engineName(provider)} in Setup`}>Check in Setup</Button>
          </li>)}
        {ENGINES.map(([id, label]) => {
          const on = report.providers.includes(id);
          return <li key={id} className="grid gap-2 py-3 first:pt-0 last:pb-0">
            <SettingSwitch settingKey={`providers.${id}`} label={label} checked={on} description={`${label} is ${on ? 'on' : 'off'}.`}
              extra={<Button variant="ghost" size="sm" onClick={onSetup} aria-label={`Check ${label} in Setup`}>Check in Setup</Button>} />
          </li>;
        })}
      </ul>
    </Group>
    <WorkspaceSettings report={report} floor={floor} />
    <Group title="Spending limits" headingId="settings-budget">
      <p className={HINT} data-part="spending-line">Daily spending limit: {spendingLimit(report.budget.floor_usd_per_day_micros)}.
        {report.budget.floor_usd_per_day_micros !== null && edit.pendingDailyCeilingClear
          ? <span role="status"> Saved from this page. No daily dollar limit after restart.</span> : null}</p>
      <Disclosure summary="Change" data-part="spending">
        <div className="grid gap-4 pt-2">
          <LimitEditor settingKey="budget.floor_usd_per_day" unit="usd" title="Daily dollar limit" label="Daily dollar limit"
            shown={spendingLimit(report.budget.floor_usd_per_day_micros)} current={report.budget.floor_usd_per_day_micros} />
          <LimitEditor settingKey="budget.per_turn_floor.usd" unit="usd" title="Available dollars needed to start work"
            label="Available dollars needed to start work" shown={formatUsdMicros(report.budget.per_turn_usd_micros)}
            current={report.budget.per_turn_usd_micros} />
          <LimitEditor settingKey="budget.per_turn_floor.tokens" unit="tokens" title="Available tokens needed to start work"
            label="Available tokens needed to start work" shown={`${report.budget.per_turn_tokens} tokens`}
            current={report.budget.per_turn_tokens} />
          <p className={HINT}>Raising a limit or clearing the daily limit asks you to confirm. The minimum for each work attempt must stay above zero.
            The defaults are $0.25 and 20,000 tokens.</p>
        </div>
      </Disclosure>
    </Group>
    <DesktopNotificationChoice />
    <DevicesSection />
    <FloorSwitches report={report} />
    <UnavailableControls floor={floor} control="hire" />
    <OfficeLayoutChoice />
    <Group title="About" headingId="settings-about">
      {report.about.daemon_version === __FLOOR_BUILD_VERSION__ ? null : <Notice role="status">
        This page is version {__FLOOR_BUILD_VERSION__}, but Dudes is version {report.about.daemon_version}.
        {' '}Open Dudes to load the matching page. Keep any draft text before leaving this page.
      </Notice>}
      <ul className={TEXT_LINK_LIST}>
        <li><a className={LINK} href={releaseNotesUrl(report.about.daemon_version)} target="_blank" rel="noopener noreferrer">
          {releaseNotesLabel(report.about.daemon_version)}</a></li>
        <li><a className={LINK} href="https://github.com/ArcfluxResearch/dudes" target="_blank" rel="noopener noreferrer">Source repository</a></li>
        <li><a className={LINK} href="https://github.com/ArcfluxResearch/dudes/issues" target="_blank" rel="noopener noreferrer">Issue tracker</a></li>
      </ul>
      <p className={HINT}>This page does not check for updates automatically.</p>
      <Disclosure summary="Build details">
        <Facts className="pt-1">
          <Fact label="Daemon">{report.about.daemon_version}</Fact>
          <Fact label="Floor build">{__FLOOR_BUILD_VERSION__}</Fact>
          <Fact label="Browser wire">browser-wire-v{report.about.browser_wire_version}</Fact>
        </Facts>
      </Disclosure>
      <Disclosure summary="Update and restart details">
        <div className="grid gap-2 pt-1">
          <p className={HINT}>To check for a newer version, run <Code>dudes upgrade --check</Code>.</p>
          <p className={HINT}>To restart after a configuration edit, run <Code>dudes quickstart --restart</Code>. A restart ends any running turn.</p>
        </div>
      </Disclosure>
    </Group>
  </div>;
}

export function FloorSwitches({ report, headingId = 'settings-switches' }: {
  readonly report: SettingsReport;
  readonly headingId?: string;
}) {
  return <Group title="Floor switches" headingId={headingId}
    description="What this page may do for you. Each switch is saved by Dudes, and most apply at once.">
    <ul className="m-0 grid list-none divide-y divide-line p-0">
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
        return <li key={key} data-switch={key} className="py-3 last:pb-0">
          <SettingSwitch settingKey={`floor.${key}`} label={label} checked={on}
            description={needs === null ? says : <>{says} <span className="font-medium text-ink">{needs}</span></>} />
        </li>;
      })}
    </ul>
    <p className={HINT}>Recovery also needs Pause, resume and rename. Spending limits still apply.</p>
    <Disclosure summary="Configuration details">
      <ul className="m-0 grid list-none gap-1 p-0 pt-1">{SWITCHES.map(([key, label]) =>
        <li key={key} className="flex flex-wrap items-baseline justify-between gap-2 text-small text-muted">
          <span>{label}</span><Code>floor.{key}</Code></li>)}</ul>
    </Disclosure>
  </Group>;
}

/** One setting the daemon keeps as on or off, drawn as a switch that shows only what the daemon last reported. */
function SettingSwitch({ settingKey, label, checked, description, extra }: {
  readonly settingKey: string;
  readonly label: string;
  readonly checked: boolean;
  readonly description: ReactNode;
  readonly extra?: ReactNode;
}) {
  const edit = useSyncExternalStore(subscribeToSettingsEdit, getSettingsEditState, getSettingsEditState);
  const next = checked ? 'off' : 'on';
  return <div className="grid gap-2">
    <Switch checked={checked} label={label} description={description} busy={edit.sending === settingKey}
      onCheckedChange={() => { if (edit.sending === null) void changeSetting(settingKey, next); }} />
    {extra === undefined ? null : <div className="flex">{extra}</div>}
    <EditOutcome settingKey={settingKey} label={`Turn ${next} ${label}`} />
  </div>;
}

/** The question the page is asking about this setting, and the sentence after its last request. */
function EditOutcome({ settingKey, label }: { readonly settingKey: string; readonly label: string }) {
  const edit = useSyncExternalStore(subscribeToSettingsEdit, getSettingsEditState, getSettingsEditState);
  const asking = edit.asking?.key === settingKey ? edit.asking : null;
  const last = edit.last?.key === settingKey ? edit.last : null;
  if (asking === null && last === null) return null;
  return <div className="grid gap-1.5 text-small text-muted">
    {asking === null ? null : <div role="group" aria-label={label}
      className="flex flex-wrap items-center gap-x-3 gap-y-2 rounded-md border border-line bg-surface-2 p-3 text-base text-ink">
      <span className="min-w-0 flex-1 basis-60 leading-reading">{asking.question}</span>
      <span className="flex gap-2">
        <Button variant="primary" size="sm" onClick={() => { void confirmSetting(); }}>{asking.confirm}</Button>
        <Button variant="ghost" size="sm" onClick={() => cancelSetting()}>Cancel</Button>
      </span>
    </div>}
    {last === null ? null : <>
      <p role="status" className="m-0 text-base text-muted">{editLine(last.result)}</p>
      {settingKey === 'floor.terminal' && last.value === 'on' && last.result.kind === 'saved' && !pageDrawsTerminal()
        ? <p role="status" className="m-0 text-base text-muted">{RELOAD_FOR_TERMINAL}</p> : null}
      {last.result.kind === 'refused' ? <Disclosure summary="Details" className="text-small">
        <div className="grid gap-1 text-muted">
          <p className="m-0">Reason: <Code>{last.result.reason}</Code></p>
          {last.result.detail === null ? null : <p className="m-0">{last.result.detail}</p>}
        </div>
      </Disclosure> : last.result.kind === 'saved' && last.result.restart.length > 0
        ? <Disclosure summary="Details" className="text-small"><p className="m-0 text-muted">Restart needed for <Code>{last.result.restart.join(', ')}</Code>.</p></Disclosure> : null}
    </>}
  </div>;
}

const LIMIT_HINT = { usd: 'Enter dollars, such as 5 or 2.50.', tokens: 'Enter a whole number of tokens.' } as const;

/** A box for one limit. It asks before the limit goes up and says nothing when it goes down. */
function LimitEditor({ settingKey, unit, title, label, shown, current }: {
  readonly settingKey: string; readonly unit: LimitUnit; readonly title: string; readonly label: string;
  readonly shown: string; readonly current: string | null;
}) {
  const edit = useSyncExternalStore(subscribeToSettingsEdit, getSettingsEditState, getSettingsEditState);
  const [typed, setTyped] = useState('');
  const [problem, setProblem] = useState<string | null>(null);
  const id = useId();
  const dailyCeiling = settingKey === 'budget.floor_usd_per_day';
  return <form data-ui="limit-form" className="grid gap-2 border-t border-line pt-3 first:border-t-0 first:pt-0" onSubmit={(event) => {
    event.preventDefault();
    if (edit.sending !== null) return;
    const change = limitChange(unit, typed, current);
    if (change.kind === 'invalid') { setProblem(LIMIT_HINT[unit]); return; }
    setProblem(null);
    if (change.kind === 'same') { setProblem('That is already the limit.'); return; }
    void changeSetting(settingKey, change.value, change.question, current === null ? 'Set limit' : 'Raise limit');
  }}>
    <div className="flex flex-wrap items-baseline justify-between gap-x-3">
      <span className="text-base font-medium text-ink">{title}</span>
      <span className="text-base text-muted">{shown}</span>
    </div>
    <Field id={id} label={`New ${label.toLowerCase()}`} error={problem ?? undefined}>
      <div className="flex flex-wrap items-center gap-2">
        <Input type="text" inputMode="decimal" autoComplete="off" spellCheck={false} value={typed} className="w-40"
          onChange={(event) => { setTyped(event.target.value); setProblem(null); }} />
        <Button type="submit" variant="primary" aria-disabled={edit.sending !== null || undefined}
          title={edit.sending !== null ? 'Wait for the current setting to be saved.' : typed.trim() === '' ? 'Enter a new limit first.' : undefined}>Save</Button>
        {dailyCeiling && current !== null ? <Button
          aria-disabled={edit.sending !== null || undefined}
          title={edit.sending === null ? undefined : 'Wait for the current setting to be saved.'}
          onClick={() => { void changeSetting(settingKey, 'unset',
        `This removes the daily dollar limit of ${formatUsdMicros(current)}. After a restart, there will be no daily dollar limit.`, 'Clear limit'); }}>Clear limit</Button> : null}
      </div>
    </Field>
    <EditOutcome settingKey={settingKey} label={`Change ${label}`} />
  </form>;
}

export function DesktopNotificationChoice() {
  const notifications = useSyncExternalStore(desktopNotificationPreference.subscribe,
    desktopNotificationPreference.getState, desktopNotificationPreference.getState);
  const on = notifications.kind === 'on';
  return <Group title="Desktop notifications" headingId="settings-notifications"
    description="Get a desktop notification when a ticket is ready, needs you, or an agent asks a question. This browser keeps the choice. Keep the Floor tab open.">
    <div className="flex">
      <Button aria-pressed={on} disabled={notifications.kind === 'unsupported' || notifications.kind === 'asking'}
        icon={on ? <BellOff {...ICON} /> : <Bell {...ICON} />}
        title={notifications.kind === 'unsupported' ? 'This browser does not support desktop notifications.'
          : notifications.kind === 'asking' ? 'Answer the notification permission request in your browser.' : undefined}
        onClick={() => { void desktopNotificationPreference.setEnabled(!on); }}>
        {on ? 'Turn notifications off' : notifications.kind === 'asking' ? 'Waiting for browser permission' : 'Turn notifications on'}
      </Button>
    </div>
    {notifications.kind === 'denied' ? <Notice role="status">Notifications are blocked. Allow notifications for this page in your
      browser's site settings, then turn them on here. The banner still shows what needs you.</Notice>
      : notifications.kind === 'unsupported' || notifications.kind === 'unavailable' ? <Notice role="status">
        Desktop notifications are unavailable in this browser. The banner still shows what needs you.</Notice> : null}
    {!notifications.remembered ? <Notice role="status">This choice lasts only for this page because browser storage is unavailable.</Notice> : null}
  </Group>;
}

/** A viewer preference kept in this browser, like the theme: the daemon never sees it. */
function OfficeLayoutChoice() {
  const pin = useSyncExternalStore(subscribeToOfficePin, getOfficePin, getOfficePin);
  return <Group title="Office layout" headingId="settings-office"
    description="Automatic draws the smallest office that seats the whole crew. Pin an office to keep it whatever the crew size. This browser keeps the choice.">
    <div role="group" aria-labelledby="settings-office" className="flex flex-wrap gap-2">
      {OFFICE_CHOICES.map(({ id, label }) => <Button key={label} aria-pressed={pin === id}
        className={cn(pin === id && 'border-accent bg-accent-soft hover:bg-accent-soft')}
        onClick={() => setOfficePin(id)}>{label}</Button>)}
    </div>
  </Group>;
}

export function WorkspaceSettings({ report, floor, headingId = 'settings-workspaces' }: {
  readonly report: SettingsReport;
  readonly floor?: FloorState;
  readonly headingId?: string;
}) {
  return <Group title="Projects" headingId={headingId}>
    {report.workspaces.length === 0 ? <p className={HINT}>No projects yet.</p> : <ul className="m-0 grid list-none gap-3 p-0">{report.workspaces.map((workspace) =>
      <li key={workspace.workspace_id} className="grid gap-2 rounded-md border border-line bg-surface-2 p-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h4 className="m-0 text-body font-semibold text-ink [overflow-wrap:anywhere]">{workspace.label}</h4>
          {floor === undefined ? null
            : <RemoveProject floor={floor} projectId={workspace.workspace_id} label={workspace.label} />}
        </div>
        <div className="flex flex-wrap gap-x-6 gap-y-1">
          <Disclosure summary="Project details"><p className={HINT}>Project <Code>{workspace.workspace_id}</Code></p></Disclosure>
          <Disclosure summary="Spending details">
            <Facts className="pt-1">
              <Fact label="Daily dollar limit per agent">{spendingLimit(workspace.agent_usd_per_day_micros)}</Fact>
              <Fact label="Daily token limit per agent">{spendingLimit(workspace.agent_tokens_per_day, false)}</Fact>
              <Fact label="Dollar limit for a new task">{spendingLimit(workspace.goal_usd_micros)}</Fact>
              <Fact label="Token limit for a new task">{spendingLimit(workspace.goal_tokens, false)}</Fact>
            </Facts>
          </Disclosure>
        </div>
      </li>)}</ul>}
    <RemoveNotice />
    {floor === undefined ? null : <AddProject floor={floor} off={<p className={HINT}>Adding a project from this page is off. Turn on the project switch under Floor switches.</p>} />}
    <Disclosure summary="Configuration details"><p className={HINT}>For a config-file edit, use <Code>dudes project reload</Code>, then <Code>dudes project reload --apply &lt;sha256&gt;</Code>.</p></Disclosure>
  </Group>;
}
