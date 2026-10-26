// SPDX-License-Identifier: MIT OR Apache-2.0

import { Ellipsis } from 'lucide-react';
import { useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore } from 'react';
import type { CSSProperties, ReactNode } from 'react';

import { Badge, Button, Disclosure, ICON, Menu, MenuSeparator, RING, Tab, TabList } from '../ui/index.ts';
import { accentOf, accessoryOf, lookOf } from './appearance.ts';
import { Portrait } from './Portrait.tsx';
import { AgentOverview } from './AgentOverview.tsx';
import { detailTabSelection, detailTabTarget, revealedTabScroll } from './detailTabs.ts';
import type { DetailTab } from './detailTabs.ts';
import type { CardRow } from './board.ts';
import { stateSentenceOf } from './agentState.ts';
import { needsOf } from './needs.ts';
import type { NeedAction } from './needs.ts';
import { useLiveActivityExpiry } from './useLiveActivityExpiry.ts';
import { HIRE_ENGINES } from './onboarding/catalog.ts';
import { NameEditor } from './rename/NameEditor.tsx';
import { LookEditor, LookMenuItem } from './look/LookEditor.tsx';
import { lookAvailable } from './look/look.ts';
import { getFloorState, subscribeToFloor } from './floor/store.ts';
import { ModelEditor } from './model/ModelEditor.tsx';
import { beginRename } from './rename/rename.ts';
import { automaticHoldOf } from './automaticHold.ts';
import { AGENT_DETAIL_HEADING_ID, handFocusOnLeave } from './focus.ts';
import type { AgentRow } from './scene/cast.ts';
import { fieldsOf } from './scene/label.ts';

function providerLabel(provider: string): string {
  return (HIRE_ENGINES as Readonly<Record<string, string>>)[provider] ?? provider;
}

function value(value: unknown): string {
  return typeof value === 'string' && value !== '' ? value : '—';
}

export function AgentDetail({
  row, cards = [], selection = null, streamState = null, control = null, recovery = null, retire = null, terminal = null, enable = null, attachNote = null, answer = null, goal: goalForm = null, task: taskForm = null, onOpenTicket,
  messages = null, chat = null, liveOutput = null, git = null, memory = null, tab = 'overview', onTabChange = () => {}, live = false,
}: {
  readonly row: AgentRow;
  readonly cards?: readonly CardRow[];
  readonly selection?: ReactNode;
  /** The output stream's state, drawn as the last part of the header's status line. */
  readonly streamState?: string | null;
  readonly control?: ReactNode;
  readonly recovery?: ReactNode;
  readonly retire?: ReactNode;
  readonly terminal?: ReactNode;
  /** How to turn on the controls the daemon reports off. */
  readonly enable?: ReactNode;
  /** One short line beside the model, for a control the agent's engine needs and the Floor has off. */
  readonly attachNote?: ReactNode;
  readonly answer?: ReactNode;
  readonly goal?: ReactNode;
  readonly task?: ReactNode;
  readonly onOpenTicket?: (id: string) => void;
  readonly messages?: ReactNode;
  readonly chat?: ReactNode;
  /** Overview's collapsed live output, for an agent that has some. */
  readonly liveOutput?: ReactNode;
  readonly git?: ReactNode;
  readonly memory?: ReactNode;
  readonly tab?: DetailTab;
  readonly onTabChange?: (tab: DetailTab) => void;
  readonly live?: boolean;
}) {
  const detail = useRef<HTMLElement>(null);
  const tabRow = useRef<HTMLDivElement>(null);
  const [focusedSection, setFocusedSection] = useState<DetailTab | null>(null);
  const rows = useMemo(() => [row], [row]);
  useLiveActivityExpiry(rows, live);
  const fields = fieldsOf(row, { live, now: performance.now(), wallNow: Date.now() });
  const need = needsOf([row], cards, { live, now: performance.now(), wallNow: Date.now() })[0]!;
  const sentence = stateSentenceOf(row, cards, fields);
  const automaticHold = automaticHoldOf(row);
  const resumeInOverview = automaticHold !== null || (row.paused === true && row.halted !== true)
    || (row.halted === true && row.halted_by_floor === true);
  const panels: readonly { readonly id: DetailTab; readonly label: string; readonly content: ReactNode }[] = [
    { id: 'overview', label: 'Overview', content: <AgentOverview row={row} cards={cards} fields={fields}
      recovery={recovery} goalForm={goalForm}
      liveOutput={liveOutput} onOpenTicket={onOpenTicket} /> },
    { id: 'chat', label: 'Chat', content: chat },
    { id: 'git', label: 'Git', content: git },
    { id: 'memory', label: 'Memory', content: memory },
    { id: 'messages', label: 'Messages', content: messages },
  ];
  function act(action: NeedAction): ReactNode {
    if (action.kind === 'question') return <Button variant="primary" onClick={(event) => {
      const question = event.currentTarget.closest('.agent-detail')?.querySelector<HTMLElement>('.agent-detail-question');
      question?.focus();
      question?.scrollIntoView({ block: 'nearest' });
    }}>{action.label}</Button>;
    if (action.kind === 'ticket') return onOpenTicket === undefined ? null
      : <Button variant="primary" onClick={() => onOpenTicket(action.card)}>{action.label}</Button>;
    if (action.label !== 'Talk') return null;
    return <>
      {tab !== 'chat' && panels.some((panel) => panel.id === 'chat' && panel.content !== null)
        ? <Button variant="primary" onClick={() => onTabChange('chat')}>{action.label}</Button> : null}
      {taskForm === null ? null : <Disclosure className="agent-overview-task basis-full" summary="Give a task">{taskForm}</Disclosure>}
    </>;
  }
  const available = panels.filter((panel) => panel.content !== null);
  const tabs = available.map((panel) => panel.id);
  const selected = detailTabSelection(tabs, tab, focusedSection);
  useLayoutEffect(() => {
    const root = detail.current;
    const section = root?.ownerDocument.activeElement?.closest<HTMLElement>('[data-detail-section]');
    const focused = available.find((panel) => panel.id === section?.dataset.detailSection);
    setFocusedSection(root?.contains(section ?? null) ? focused?.id ?? null : null);
  });
  useLayoutEffect(() => {
    const strip = tabRow.current;
    const tab = strip?.querySelector<HTMLElement>('[aria-selected="true"]');
    if (!strip || !tab) return;
    const left = tab.getBoundingClientRect().left - strip.getBoundingClientRect().left + strip.scrollLeft;
    strip.scrollLeft = revealedTabScroll(strip.scrollLeft, left, tab.offsetWidth, strip.clientWidth);
  }, [selected, tabs.join()]);
  useLayoutEffect(() => {
    const strip = tabRow.current;
    if (!strip) return;
    const wheel = (event: WheelEvent) => {
      const limit = strip.scrollWidth - strip.clientWidth;
      if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey
        || event.deltaX !== 0 || event.deltaY === 0 || limit <= 0) return;
      const unit = event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? strip.clientWidth : 1;
      const next = Math.max(0, Math.min(limit, strip.scrollLeft + event.deltaY * unit));
      if (next === strip.scrollLeft) return;
      strip.scrollLeft = next;
      event.preventDefault();
    };
    strip.addEventListener('wheel', wheel, { passive: false });
    return () => strip.removeEventListener('wheel', wheel);
  }, []);

  return (
    <aside ref={detail} className="agent-detail" aria-label="Selected agent details" data-selected-agent-id={row.id} data-detail-tab={selected}
      style={{ '--agent': accentOf(row) } as CSSProperties}>
      <div className="agent-detail-context">
      <header className="agent-detail-head grid grid-cols-1 gap-4 font-sans text-ink">
        <div className="flex items-start gap-3 pr-8">
          <Portrait className="agent-portrait agent-detail-portrait shrink-0" look={lookOf(row)} accessory={accessoryOf(row.role)}
            name={row.name} />
          <div className="agent-detail-identity grid min-w-0 flex-1 gap-0.5">
            <div className="agent-detail-name flex min-w-0 flex-wrap items-center gap-1.5">
              <h2 id={AGENT_DETAIL_HEADING_ID} tabIndex={-1} onDoubleClick={() => beginRename(`detail:${row.id}`, row)}
                className={`m-0 min-w-0 rounded-sm font-sans text-page font-semibold text-ink [overflow-wrap:anywhere] ${RING}`}>{value(row.name)}</h2>
              <NameEditor row={row} surface="detail" showName={false} />
            </div>
            <p className="agent-detail-status m-0 text-base text-muted [overflow-wrap:anywhere]">
              {value(row.role)}
              {typeof row.provider === 'string' && row.provider !== '' ? ` · ${providerLabel(row.provider)}` : ''}
              {fields.liveActivity === null ? null : <>{' · '}<span data-live-activity={fields.liveActivity}>{fields.liveActivity}</span></>}
              {streamState === null ? null : <>{' · '}<span className="stream-state" role="status">{streamState}</span></>}
            </p>
          </div>
          <MoreActions row={row} retire={retire} />
        </div>
        {selection}
        <section className="agent-detail-state grid gap-3" aria-label="State" data-state={need.state}>
          <p className="agent-detail-state-line m-0 flex flex-wrap items-center gap-x-2 gap-y-1 text-body"><Badge state={need.state} />
            {' '}<span className="agent-detail-state-text text-ink">{sentence}</span></p>
          <div className="agent-detail-primary flex flex-wrap items-center gap-2 empty:hidden">
            {resumeInOverview ? control : null}
            {act(need.action)}
            {need.second === null ? null : act(need.second)}
          </div>
        </section>
        {resumeInOverview ? null : control}
        <LookEditor row={row} />
        <div className="grid gap-2">
          <ModelEditor row={row} />
          {terminal}
          {attachNote}
        </div>
        {enable}
        {fields.question === '' ? null : (
          <section className={`agent-detail-question m-0 grid gap-2 rounded-lg border border-line bg-warn-soft p-3 shadow-none ${RING}`} aria-label="Waiting question" tabIndex={-1}>
            <h3 className="m-0 font-sans text-title font-semibold text-ink">Waiting question</h3>
            <p className="m-0 text-body text-ink [overflow-wrap:anywhere]">{fields.question}</p>
            {answer}
          </section>
        )}
      </header>
      <TabList ref={tabRow} label="Agent detail sections" className="agent-detail-tabs mt-4 mb-3 [scrollbar-width:none]"
        aria-describedby={tabs.length > 1 ? 'agent-detail-tab-hint' : undefined}>
        {available.map(({ id, label }) => <Tab key={id} selected={selected === id} className="agent-detail-tab mb-0"
          id={`agent-detail-tab-${id}`} data-detail-section={id} aria-controls={`agent-detail-panel-${id}`}
          ref={handFocusOnLeave} data-focus-on-leave="agent-detail-tab-overview"
          onFocus={() => setFocusedSection(id)} onBlur={() => setFocusedSection(null)}
          onClick={() => onTabChange(id)} onKeyDown={(event) => {
            if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
            const target = detailTabTarget(tabs, id, event.key);
            if (target === null) return;
            event.preventDefault();
            event.stopPropagation();
            onTabChange(target);
            event.currentTarget.ownerDocument.getElementById(`agent-detail-tab-${target}`)?.focus();
          }}>{label}</Tab>)}
      </TabList>
      {tabs.length > 1 ? <p id="agent-detail-tab-hint" className="agent-detail-tab-hint m-0 mb-3 text-caption text-subtle">Scroll for more sections · Use arrow keys</p> : null}
      </div>
      {available.map(({ id, content }) => <section key={id} id={`agent-detail-panel-${id}`}
        role="tabpanel" data-detail-section={id} aria-labelledby={`agent-detail-tab-${id}`} tabIndex={0} hidden={selected !== id}
        ref={handFocusOnLeave} data-focus-on-leave="agent-detail-tab-overview"
        onFocus={() => setFocusedSection(id)} onBlur={(event) => {
          if (!event.currentTarget.contains(event.relatedTarget)) setFocusedSection(null);
        }}>
        {content}
      </section>)}
    </aside>
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
