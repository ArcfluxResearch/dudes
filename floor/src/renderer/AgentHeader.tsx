// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The selected agent's header, in bands: who it is, what state it is in, what the person can do, and the small
// facts (its model, what ran). The status band is the only place the state is told.

import { Ellipsis } from 'lucide-react';
import { useState, useSyncExternalStore } from 'react';
import type { ReactNode } from 'react';

import { Badge, Button, ICON, Menu, MenuSeparator, RING, cn } from '../ui/index.ts';
import { accessoryOf, lookOf } from './appearance.ts';
import { PORTRAIT_FRAME, Portrait } from './Portrait.tsx';
import type { AgentStatus } from './agentState.ts';
import type { ControlNote, ControlPart } from './control/ControlPanel.tsx';
import type { NeedAction, NeedRow } from './needs.ts';
import { HIRE_ENGINES } from './onboarding/catalog.ts';
import { NameEditor } from './rename/NameEditor.tsx';
import { LookEditor, LookMenuItem } from './look/LookEditor.tsx';
import { lookAvailable } from './look/look.ts';
import { getFloorState, subscribeToFloor } from './floor/store.ts';
import { ModelEditor } from './model/ModelEditor.tsx';
import { beginRename } from './rename/rename.ts';
import { AGENT_DETAIL_HEADING_ID } from './focus.ts';
import type { AgentRow } from './scene/cast.ts';

function providerLabel(provider: string): string {
  return (HIRE_ENGINES as Readonly<Record<string, string>>)[provider] ?? provider;
}

function value(value: unknown): string {
  return typeof value === 'string' && value !== '' ? value : '—';
}

/** What the header draws around the facts it is given; each slot is a part of the page that owns its own state. */
export interface AgentHeaderProps {
  readonly row: AgentRow;
  readonly need: NeedRow;
  readonly status: AgentStatus;
  readonly note: ControlNote | null;
  readonly liveActivity: string | null;
  readonly streamState: string | null;
  /** The close button. */
  readonly selection: ReactNode;
  readonly retire: ReactNode;
  readonly question: ReactNode;
  /** Pause, Resume and Stop this turn, or the notice that says Resume is off. */
  readonly control: ((part: ControlPart, resumeFirst: boolean) => ReactNode) | null;
  readonly resumeOffered: boolean;
  readonly resumeOff: ReactNode;
  readonly terminal: ReactNode;
  readonly terminalNote: ReactNode;
  readonly attachNote: ReactNode;
  readonly enable: ReactNode;
  readonly task: ReactNode;
  /** True when Talk has somewhere to go: the Chat tab exists and is not the one open. */
  readonly showTalk: boolean;
  readonly onTalk: () => void;
  readonly onOpenTicket?: (id: string) => void;
}

export function AgentHeader(props: AgentHeaderProps) {
  const { row, status, note, liveActivity, streamState } = props;
  return (
    <header className="agent-detail-head grid max-h-[50dvh] grid-cols-1 gap-3 overflow-y-auto font-sans text-ink">
      <div className="agent-detail-bar flex items-start gap-3">
        <Portrait className={cn('agent-portrait agent-detail-portrait shrink-0 size-10', PORTRAIT_FRAME)} look={lookOf(row)} accessory={accessoryOf(row.role)}
          name={row.name} />
        <div className="agent-detail-identity flex min-w-0 flex-1 flex-wrap items-baseline gap-x-2 gap-y-0.5 self-center">
          <div className="agent-detail-name flex min-w-0 items-center gap-1.5">
            <h2 id={AGENT_DETAIL_HEADING_ID} tabIndex={-1} onDoubleClick={() => beginRename(`detail:${row.id}`, row)}
              className={`m-0 min-w-0 rounded-sm font-sans text-page font-semibold text-ink [overflow-wrap:anywhere] ${RING}`}>{value(row.name)}</h2>
            <NameEditor row={row} surface="detail" showName={false} />
          </div>
          <p className="agent-detail-status m-0 text-base text-muted [overflow-wrap:anywhere]">
            {value(row.role)}
            {typeof row.provider === 'string' && row.provider !== '' ? ` · ${providerLabel(row.provider)}` : ''}
            {liveActivity === null && streamState === null ? null : <span className="sr-only">
              {liveActivity === null ? null : <>{' · '}<span data-live-activity={liveActivity}>{liveActivity}</span></>}
              {streamState === null ? null : <>{' · '}<span className="stream-state font-normal" role="status">{streamState}</span></>}
            </span>}
          </p>
        </div>
        <MoreActions row={row} retire={props.retire} />
        {props.selection}
      </div>
      <LookEditor row={row} />
      <StatusBand status={status} note={note} />
      {props.question}
      <ActionBar {...props} />
      <MetaLine {...props} />
    </header>
  );
}

function StatusBand({ status, note }: { readonly status: AgentStatus; readonly note: ControlNote | null }) {
  const detail = note?.text ?? status.detail;
  const control = note === null ? status.control : null;
  return (
    <section className="agent-detail-state flex flex-wrap items-center gap-x-2 gap-y-1" aria-label="State" data-state={status.tone}>
      <Badge state={status.tone}>{status.label}</Badge>
      {detail === '' ? null : <p
        className={cn('agent-detail-state-text m-0 min-w-0 flex-1 basis-48 text-body text-ink [overflow-wrap:anywhere]', control !== null && 'agent-detail-pause',
          note?.kind === 'problem' && 'control-problem text-danger')}
        role={note?.kind === 'problem' ? 'alert' : control !== null || note !== null ? 'status' : undefined}
        data-pause-state={control?.kind === 'pause' ? control.state : undefined}
        data-stop-state={control?.kind === 'stop' ? control.state : undefined}>{detail}</p>}
    </section>
  );
}

function ActionBar({ need, control, resumeOffered, resumeOff, showTalk, onTalk, task, terminal, onOpenTicket }: AgentHeaderProps) {
  const [taskOpen, setTaskOpen] = useState(false);
  const offered = (action: NeedAction | null): ReactNode => {
    if (action === null) return null;
    if (action.kind === 'question') return <Button size="sm" variant="primary" onClick={(event) => {
      const question = event.currentTarget.closest('.agent-detail')?.querySelector<HTMLElement>('.agent-detail-question');
      question?.focus();
      question?.scrollIntoView({ block: 'nearest' });
    }}>{action.label}</Button>;
    if (action.kind === 'ticket') return onOpenTicket === undefined ? null
      : <Button size="sm" variant="primary" onClick={() => onOpenTicket(action.card)}>{action.label}</Button>;
    return null;
  };
  const lead = offered(need.action) ?? offered(need.second);
  const resumeFirst = lead === null && resumeOffered;
  const talk = need.action.kind === 'agent' && need.action.label === 'Talk';
  const askForTask = talk && task !== null;
  return (
    <div className="agent-detail-primary grid gap-2 empty:hidden">
      <div className="agent-detail-actions flex flex-wrap items-center gap-2 empty:hidden">
        {offered(need.action)}
        {offered(need.second)}
        {resumeFirst ? control?.('primary', true) : null}
        {!talk || !showTalk ? null : <Button size="sm" variant={lead === null && !resumeFirst ? 'primary' : 'secondary'} onClick={onTalk}>{need.action.label}</Button>}
        {!askForTask ? null : <Button size="sm" className="agent-task-toggle" aria-expanded={taskOpen} aria-controls="agent-task-panel"
          onClick={() => setTaskOpen(!taskOpen)}>Give a task</Button>}
        {control?.('rest', resumeFirst)}
        {resumeOff}
        {terminal}
      </div>
      {!askForTask ? null : <div id="agent-task-panel" className="agent-overview-task rounded-lg border border-line bg-surface p-3" hidden={!taskOpen}>{task}</div>}
    </div>
  );
}

function MetaLine({ row, terminalNote, attachNote, enable }: AgentHeaderProps) {
  return (
    <ModelEditor row={row}>
      {attachNote}
      {terminalNote}
      {enable}
    </ModelEditor>
  );
}

/** What is not the main action, in one menu. It is not drawn when it would hold nothing. */
function MoreActions({ row, retire }: { readonly row: AgentRow; readonly retire: ReactNode }) {
  const floor = useSyncExternalStore(subscribeToFloor, getFloorState, getFloorState);
  const look = lookAvailable(floor, row);
  if (!look && retire === null) return null;
  return <Menu label="More actions" trigger={<Ellipsis {...ICON} />} className="agent-more">
    <LookMenuItem row={row} />
    {look && retire !== null ? <MenuSeparator /> : null}
    {retire}
  </Menu>;
}
