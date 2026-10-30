// SPDX-License-Identifier: MIT OR Apache-2.0

import { useLayoutEffect, useMemo, useRef, useState } from 'react';
import type { CSSProperties, ReactNode } from 'react';

import { RING, RING_INSET, Tab, TabList } from '../ui/index.ts';
import { accentOf } from './appearance.ts';
import { AgentOverview } from './AgentOverview.tsx';
import { detailTabSelection, detailTabTarget, revealedTabScroll } from './detailTabs.ts';
import type { DetailTab } from './detailTabs.ts';
import type { CardRow } from './board.ts';
import { agentStatusOf } from './agentState.ts';
import { AgentHeader } from './AgentHeader.tsx';
import type { ControlNote, ControlPart } from './control/ControlPanel.tsx';
import { needsOf } from './needs.ts';
import { useLiveActivityExpiry } from './useLiveActivityExpiry.ts';
import { handFocusOnLeave } from './focus.ts';
import type { AgentRow } from './scene/cast.ts';
import { fieldsOf } from './scene/label.ts';

export function AgentDetail({
  row, cards = [], selection = null, streamState = null, control = null, controlNote = null, resumeOffered = false, resumeOff = null, terminalNote = null, recovery = null, retire = null, terminal = null, enable = null, attachNote = null, answer = null, goal: goalForm = null, task: taskForm = null, onOpenTicket,
  messages = null, chat = null, liveOutput = null, git = null, memory = null, tab = 'overview', onTabChange = () => {}, live = false,
}: {
  readonly row: AgentRow;
  readonly cards?: readonly CardRow[];
  readonly selection?: ReactNode;
  /** The output stream's state, drawn as the last part of the header's status line. */
  readonly streamState?: string | null;
  /** Pause, Resume and Stop this turn, drawn into the action bar a part at a time. */
  readonly control?: ((part: ControlPart, resumeFirst: boolean) => ReactNode) | null;
  /** A problem or a wait on the controls, said in the status band. */
  readonly controlNote?: ControlNote | null;
  /** True when Resume is what the state calls for, so the action bar leads with it. */
  readonly resumeOffered?: boolean;
  /** Stands in the action bar for Resume while the Floor's control switch is off. */
  readonly resumeOff?: ReactNode;
  readonly recovery?: ReactNode;
  readonly retire?: ReactNode;
  readonly terminal?: ReactNode;
  /** One muted line for the engine that has no terminal of its own. */
  readonly terminalNote?: ReactNode;
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
  const status = agentStatusOf(row, cards, fields, need.state);
  const panels: readonly { readonly id: DetailTab; readonly label: string; readonly content: ReactNode }[] = [
    { id: 'overview', label: 'Overview', content: <AgentOverview row={row} cards={cards} fields={fields}
      recovery={recovery} goalForm={goalForm}
      liveOutput={liveOutput} onOpenTicket={onOpenTicket} /> },
    { id: 'chat', label: 'Chat', content: chat },
    { id: 'git', label: 'Git', content: git },
    { id: 'memory', label: 'Memory', content: memory },
    { id: 'messages', label: 'Messages', content: messages },
  ];
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
    <aside ref={detail} className="agent-detail flex min-h-0 flex-1 flex-col" aria-label="Selected agent details" data-selected-agent-id={row.id} data-detail-tab={selected}
      style={{ '--agent': accentOf(row) } as CSSProperties}>
      <div className="agent-detail-context flex-none">
      <AgentHeader row={row} need={need} status={status} note={controlNote} liveActivity={fields.liveActivity} streamState={streamState}
        selection={selection} retire={retire} control={control} resumeOffered={resumeOffered} resumeOff={resumeOff}
        terminal={terminal} terminalNote={terminalNote} attachNote={attachNote} enable={enable} task={taskForm}
        showTalk={tab !== 'chat' && panels.some((panel) => panel.id === 'chat' && panel.content !== null)} onTalk={() => onTabChange('chat')}
        question={fields.question === '' ? null : (
          <section className={`agent-detail-question m-0 grid max-h-64 gap-2 overflow-y-auto rounded-lg border border-line bg-warn-soft p-3 shadow-none ${RING}`} aria-label="Waiting question" tabIndex={-1}>
            <h3 className="m-0 font-sans text-title font-semibold text-ink">Waiting question</h3>
            <p className="m-0 text-body text-ink [overflow-wrap:anywhere]">{fields.question}</p>
            {answer}
          </section>
        )}
        onOpenTicket={onOpenTicket} />
      <TabList ref={tabRow} label="Agent detail sections" className="agent-detail-tabs mt-3 mb-2 [scrollbar-width:none]"
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
      {tabs.length > 1 ? <p id="agent-detail-tab-hint" className="agent-detail-tab-hint sr-only">Scroll for more sections · Use arrow keys</p> : null}
      </div>
      {available.map(({ id, content }) => <section key={id} id={`agent-detail-panel-${id}`}
        role="tabpanel" className={`min-h-0 flex-1 overflow-y-auto rounded-md ${RING_INSET}`} data-detail-section={id} aria-labelledby={`agent-detail-tab-${id}`} tabIndex={0} hidden={selected !== id}
        ref={handFocusOnLeave} data-focus-on-leave="agent-detail-tab-overview"
        onFocus={() => setFocusedSection(id)} onBlur={(event) => {
          if (!event.currentTarget.contains(event.relatedTarget)) setFocusedSection(null);
        }}>
        {content}
      </section>)}
    </aside>
  );
}
