// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The office: a canvas the voxel renderer draws on, a native button over
// each figure and over the goal board, and the mirror list that tells
// anything not looking at the canvas who is on it.
//
// Where each agent goes is decided in `scene/`, from the projection; this
// component feeds that plan to the renderer once a frame and lays the
// buttons and name tags over the figures it drew. The renderer and three.js
// load on demand, so the rest of the page never waits for them.

import { Fragment, useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore } from 'react';
import { createPortal } from 'react-dom';

import { accessoryOf, lookOf } from './appearance.ts';
import { BoardNotes } from './BoardNotes.tsx';
import { shelfCaption, shelfParcels, shelfView } from './finished-today.ts';
import type { FloorState } from './floor/store.ts';
import { getFloorState, subscribeToFloor } from './floor/store.ts';
import { subscribeToProjection } from './floor/projection.ts';
import { HireEntry, hireSwitchOn } from './onboarding/HireEntry.tsx';
import { watchCover } from './office3d/cover.ts';
import { furnish, noteAnchors } from './office3d/furnish.ts';
import type { Furnished } from './office3d/furnish.ts';
import type { OfficeLayoutDefinition } from './office3d/layout.ts';
import { chooseOffice, followFull, fullNotice, officeOf } from './office3d/offices.ts';
import type { LayoutId, OfficeChoice } from './office3d/offices.ts';
import type { CrewMember, OfficeRenderer } from './office3d/OfficeRenderer.ts';
import { officePlan } from './office3d/plan.ts';
import type { OfficePlan } from './office3d/plan.ts';
import { TURN_STEP, keepsWhole, layTags, onCanvas, showOverlay, turnKey, wheelZoom } from './office3d/view.ts';
import type { Size } from './office3d/view.ts';
import type { OfficeSupport } from './office3d/support.ts';
import { OFFICE_FALLBACK, startRenderer } from './office3d/support.ts';
import { ReadError } from './ReadError.tsx';
import type { AgentRow, Desk } from './scene/cast.ts';
import { castFrom } from './scene/cast.ts';
import { cardNotesAt, placeCardNotes, planCardNotes } from './scene/cardNotes.ts';
import type { CardNoteMotion, NoteAnchors } from './scene/cardNotes.ts';
import { fieldsOf } from './scene/label.ts';
import { NEED_LABEL, needsOf } from './needs.ts';
import type { CastMotion } from './scene/motion.ts';
import { activeEnvelopes, envelopesAt, planEnvelopes } from './scene/mailEnvelopes.ts';
import type { EnvelopeMotion } from './scene/mailEnvelopes.ts';
import { charactersAt, planCast, planExit } from './scene/motion.ts';
import { followMode, officeModeOf } from './scene/officeMode.ts';
import type { OfficeModeView } from './scene/officeMode.ts';
import { applyTransitions, arrivalTransitions, inOffice, rosterTransitions } from './scene/presence.ts';
import type { Roster, Transition } from './scene/presence.ts';
import { STATIONS, asksOpenQuestion, officeStateOf } from './scene/stations.ts';
import { activeHuddles, huddlePosed, huddled, memberOf, nextHuddleChange, planHuddles } from './scene/huddles.ts';
import type { Huddle } from './scene/huddles.ts';
import { activeVisits, approachPoint, planVisits, visitPosed } from './scene/visits.ts';
import type { Visit } from './scene/visits.ts';
import { useHeard } from './useHeard.ts';
import { useLiveActivityExpiry } from './useLiveActivityExpiry.ts';
import { getOfficePin, subscribeToOfficePin } from './view.ts';
import { lazyChunk } from '../transport.ts';

const TURN_PER_PIXEL = 0.008;
const DRAG_THRESHOLD = 4;
const ZOOM_STEP = 1.25;

/** A retired agent on its way out: its last row and look, and its walk to the door. */
interface Exit {
  readonly row: AgentRow;
  readonly member: CrewMember;
  readonly motion: CastMotion;
}

type RendererModule = typeof import('./office3d/OfficeRenderer.ts');
let rendererModule: Promise<RendererModule> | null = null;

function loadRenderer(): Promise<RendererModule> {
  rendererModule ??= lazyChunk(() => import('./office3d/OfficeRenderer.ts'));
  return rendererModule;
}

/** The page's own colour scheme, as its stylesheet resolved it from the theme toggle and the system. */
function readNight(): boolean {
  return getComputedStyle(document.documentElement).colorScheme === 'dark';
}

function nameOf(row: AgentRow | undefined, id: string): string {
  return typeof row?.name === 'string' && row.name !== '' ? row.name : id;
}

/** One office, with what the planner and the card notes read from it. */
interface Built {
  readonly layout: OfficeLayoutDefinition;
  readonly plan: OfficePlan;
  readonly furnished: Furnished;
  readonly goalSlots: number;
}

const BUILT = new Map<LayoutId, Built>();

function built(id: LayoutId): Built {
  let office = BUILT.get(id);
  if (office === undefined) {
    const layout = officeOf(id).definition;
    const furnished = furnish(layout);
    office = { layout, plan: officePlan(layout), furnished,
      goalSlots: furnished.fixtures.filter((fixture) => fixture.kind === 'goal-note').length };
    BUILT.set(id, office);
  }
  return office;
}

function anchorsFor(office: Built, desks: readonly Desk[]): NoteAnchors {
  const deskOf = new Map(desks.flatMap((desk) => {
    const index = desk.seat === null ? -1 : office.plan.desks.indexOf(desk.seat);
    return index < 0 ? [] : [[desk.agentId, index] as const];
  }));
  return noteAnchors(office.furnished, deskOf);
}

function goalColours(rows: readonly AgentRow[]): readonly string[] {
  return rows.filter((row) => typeof row.goal === 'object' && row.goal !== null
    && (row.goal as Record<string, unknown>).status !== 'abandoned').map((row) => lookOf(row).shirt);
}

/** The goals past an office board's goal notes, which the board button counts but the board does not draw. */
export function goalsPastBoard(rows: readonly AgentRow[], id: LayoutId): number {
  return Math.max(0, goalColours(rows).length - built(id).goalSlots);
}

/** An agent's name and engine. The name is isolated, so a name written right to left cannot reorder the engine after it. */
export function NameTag({ row, id }: { readonly row: AgentRow | undefined; readonly id: string }) {
  return <><bdi>{nameOf(row, id)}</bdi>{typeof row?.provider === 'string'
    ? <span className="office-tag-engine"> · {row.provider}</span> : null}</>;
}

export function OfficeCanvas({ floor, selectedId, onSelectAgent, onOpenQuestion, onOpenTasks, onAddAgent, bar = null }: {
  readonly floor: FloorState;
  readonly selectedId: string | null;
  readonly onSelectAgent: (id: string) => void;
  /** Open an agent's waiting question in the drawer. */
  readonly onOpenQuestion: (id: string) => void;
  readonly onOpenTasks: () => void;
  readonly onAddAgent: (opener: HTMLButtonElement) => void;
  /** Where the office's lines and view buttons go when the panels leave too narrow a band of it to read them in. */
  readonly bar?: HTMLElement | null;
}) {
  const boxRef = useRef<HTMLDivElement | null>(null);
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const boardRef = useRef<HTMLButtonElement | null>(null);
  const zoomRef = useRef<HTMLDivElement | null>(null);
  const rendererRef = useRef<OfficeRenderer | null>(null);
  /** Whether a frame loop runs to finish walks; without one they finish at once. */
  const drawingRef = useRef(false);
  const motionsRef = useRef<readonly CastMotion[]>([]);
  const seatsRef = useRef<readonly Desk[]>([]);
  const rosterRef = useRef<Roster>(new Map());
  const exitsRef = useRef(new Map<string, Exit>());
  const envelopesRef = useRef<readonly EnvelopeMotion[]>([]);
  const visitsRef = useRef<readonly Visit[]>([]);
  const huddlesRef = useRef<readonly Huddle[]>([]);
  const reducedMotionRef = useRef(false);
  const notesRef = useRef<readonly CardNoteMotion[]>([]);
  const crewRef = useRef<{ members: readonly CrewMember[]; goals: readonly string[]; shipped: number }>(
    { members: [], goals: [], shipped: 0 });
  const buttonsRef = useRef(new Map<string, HTMLButtonElement>());
  const tagsRef = useRef(new Map<string, HTMLSpanElement>());
  const marksRef = useRef(new Map<string, HTMLButtonElement>());
  const selectedRef = useRef<string | null>(selectedId);
  const stillRef = useRef(false);
  const scheduleRef = useRef<() => void>(() => {});
  const planInputsRef = useRef<{ floor: FloorState; expiry: number; office: Built } | null>(null);
  const expiryRef = useRef(0);
  const confirmedCrewRef = useRef(0);
  /** Which view button had the keyboard when they last moved between the office and the top bar. */
  const refocusRef = useRef<number | null>(null);

  const [support, setSupport] = useState<OfficeSupport>('loading');
  const [attempt, setAttempt] = useState(0);
  const [leavers, setLeavers] = useState<readonly AgentRow[]>([]);
  const drawn = support === 'ready';
  const fallback = support === 'unavailable' || support === 'lost' ? OFFICE_FALLBACK[support] : null;
  const confirmedMode = officeModeOf(floor);
  const [mode, setMode] = useState<OfficeModeView | undefined>(() => confirmedMode ?? undefined);
  const [modeHeard, setModeHeard] = useHeard();
  const [narrow, setNarrow] = useState(false);

  const live = floor.phase === 'live' && !floor.stale;
  const activityExpiry = useLiveActivityExpiry(floor.agents, live);
  expiryRef.current = activityExpiry;
  const pin = useSyncExternalStore(subscribeToOfficePin, getOfficePin, getOfficePin);
  const choice = choiceFor(floor);
  const office = built(choice.id);
  // The office and what it says of its crew outlast a resync's empty view, but only rows a snapshot confirmed show the count.
  const confirmed = floor.snapshotEpoch === floor.epoch;
  const full = fullNotice(confirmedCrewRef.current, choice, pin !== null);
  const fullShownRef = useRef<string | null | undefined>(undefined);
  const [fullHeard, setFullHeard] = useHeard();
  const desks = useMemo(() => desksOf(floor.agents, office), [floor.agents, office]);
  const anchors = useMemo(() => anchorsFor(office, desks), [office, desks]);
  const goals = useMemo(() => goalColours(floor.agents), [floor.agents]);
  const board = useMemo(() => {
    const onBoard = placeCardNotes(floor.cards, anchors).filter((note) => note.place.kind === 'board');
    return { count: onBoard.length, hidden: onBoard.filter((note) => !note.shown).length };
  }, [floor.cards, anchors]);

  /** The office for the last crew a snapshot confirmed, so the empty view a resync leaves keeps the office it had. */
  function choiceFor(current: FloorState): OfficeChoice {
    if (current.snapshotEpoch === current.epoch) confirmedCrewRef.current = current.agents.length;
    return chooseOffice(confirmedCrewRef.current, getOfficePin());
  }

  /** Each agent keeps its desk while it stays, and a hire takes the first free one. */
  function desksOf(agents: readonly AgentRow[], office: Built): readonly Desk[] {
    const desks = castFrom(agents, office.plan, seatsRef.current);
    // A resync empties the view before its snapshot comes; the desks outlast that gap.
    if (agents.length > 0) seatsRef.current = desks;
    return desks;
  }

  function exitMotions(): readonly CastMotion[] {
    return [...exitsRef.current.values()].map((exit) => exit.motion);
  }

  function drawCrew(): void {
    rendererRef.current?.setCrew([...crewRef.current.members, ...[...exitsRef.current.values()].map((exit) => exit.member)]);
  }

  function showExits(): void {
    setLeavers([...exitsRef.current.values()].map((exit) => exit.row));
    drawCrew();
  }

  /** A new office is a new room: a walk out of the old one ends there. */
  function dropExits(): void {
    if (exitsRef.current.size === 0) return;
    rosterRef.current = applyTransitions(rosterRef.current, [...exitsRef.current.keys()].map((id) => ({ id, from: 'leaving', to: 'gone' })));
    exitsRef.current.clear();
    showExits();
  }

  /** Start each retired agent's walk out from where it stood in the last plan, and drop the figures that are gone. */
  function walkOut(changes: readonly Transition[], before: readonly CastMotion[], rows: readonly AgentRow[], plan: OfficePlan, now: number): void {
    const posed = charactersAt(before, now, false);
    for (const { id, to } of changes) {
      if (to !== 'leaving') {
        exitsRef.current.delete(id);
        continue;
      }
      const from = posed.find((character) => character.agentId === id);
      const row = rows.find((candidate) => candidate.id === id);
      const member = crewRef.current.members.find((candidate) => candidate.id === id);
      if (from === undefined || row === undefined || member === undefined) {
        rosterRef.current = applyTransitions(rosterRef.current, [{ id, from: to, to: 'gone' }]);
        continue;
      }
      exitsRef.current.set(id, { row, member: { ...member, badge: null },
        motion: planExit(from, plan, now, [...motionsRef.current, ...exitMotions()]) });
    }
    showExits();
  }

  function planMotion(current: FloorState, update: 'snapshot' | 'delta', force = false): void {
    // Rows no snapshot has confirmed yet, such as the empty view a resync leaves, are not a roster change:
    // hold the last plan until the snapshot comes.
    if (current.snapshotEpoch !== current.epoch) return;
    const office = built(choiceFor(current).id);
    const previous = planInputsRef.current;
    const currentLive = current.phase === 'live' && !current.stale;
    // A new office is a new room: everyone starts at their place in it rather than walking from the old one.
    const moved = previous !== null && previous.office !== office;
    if (!force && !moved && previous !== null && previous.floor.epoch === current.epoch
      && previous.floor.agents === current.agents && previous.expiry === expiryRef.current
      && (previous.floor.phase === 'live' && !previous.floor.stale) === currentLive) return;
    planInputsRef.current = { floor: current, expiry: expiryRef.current, office };
    const now = Date.now();
    const before = motionsRef.current;
    const changes = rosterTransitions(rosterRef.current, current.agents.map((row) => row.id), update);
    rosterRef.current = applyTransitions(rosterRef.current, changes);
    if (moved) dropExits();
    motionsRef.current = planCast(current.agents, desksOf(current.agents, office), office.plan, {
      activity: { live: currentLive, now: performance.now(), wallNow: now },
      previous: moved ? [] : motionsRef.current,
      update,
      now,
      entering: new Set([...rosterRef.current].flatMap(([id, presence]) => (presence === 'entering' ? [id] : []))),
      passing: exitMotions(),
    });
    if (changes.some(({ from, to }) => to === 'leaving' || from === 'leaving')) {
      walkOut(changes, moved ? [] : before, previous?.floor.agents ?? [], office.plan, now);
    }
    if (!drawingRef.current) settle(now, true);
    scheduleRef.current();
  }

  function settle(now: number, reduced: boolean): void {
    const walked = (motion: CastMotion | undefined) => reduced || motion === undefined || now >= motion.arrivalAt;
    const changes = arrivalTransitions(rosterRef.current, (id) => walked(exitsRef.current.get(id)?.motion
      ?? motionsRef.current.find((motion) => motion.desk.agentId === id)));
    if (changes.length === 0) return;
    rosterRef.current = applyTransitions(rosterRef.current, changes);
    if (!changes.some(({ to }) => to === 'gone')) return;
    for (const { id, to } of changes) if (to === 'gone') exitsRef.current.delete(id);
    showExits();
  }

  function placeNotes(current: FloorState): void {
    notesRef.current = current.phase !== 'live' ? [] : planCardNotes([], placeCardNotes(current.cards, anchors), 'snapshot', Date.now());
  }

  useEffect(() => {
    placeNotes(floor);
    planMotion(floor, 'snapshot');
  }, [floor.epoch]);

  useEffect(() => {
    envelopesRef.current = [];
    visitsRef.current = [];
    huddlesRef.current = [];
    placeNotes(floor);
    rendererRef.current?.setLayout(office.layout);
    planMotion(floor, 'delta');
  }, [office]);

  useEffect(() => {
    envelopesRef.current = [];
    visitsRef.current = [];
    huddlesRef.current = [];
    const stopFloor = subscribeToFloor(() => {
      const current = getFloorState();
      if (current.phase !== 'live' || current.snapshotEpoch !== current.epoch || current.messagesLsn === null) {
        envelopesRef.current = [];
        visitsRef.current = [];
    huddlesRef.current = [];
        scheduleRef.current();
      }
    });
    const stopProjection = subscribeToProjection((event) => {
      const office = built(choiceFor(event.floor).id);
      const desks = desksOf(event.floor.agents, office);
      const deskFeet = new Map(desks.map((desk) => [desk.agentId, desk.foot]));
      envelopesRef.current = planEnvelopes(envelopesRef.current, event, deskFeet,
        performance.now(), reducedMotionRef.current);
      const rows = new Map(event.floor.agents.map((row) => [row.id, row]));
      const activity = { live: event.floor.phase === 'live' && !event.floor.stale, now: performance.now(), wallNow: Date.now() };
      huddlesRef.current = planHuddles(huddlesRef.current, event, deskFeet,
        new Map(charactersAt(motionsRef.current, Date.now(), reducedMotionRef.current).map((posed) => [posed.agentId, posed.foot])),
        office.plan, Date.now(), rows, activity,
        new Set(visitsRef.current.map((visit) => visit.senderId)));
      visitsRef.current = planVisits(visitsRef.current, event, deskFeet,
        new Map(desks.map((desk) => [desk.agentId, approachPoint(desk)])), office.plan, Date.now(), rows, activity,
        huddled(huddlesRef.current));
      notesRef.current = planCardNotes(notesRef.current, placeCardNotes(event.floor.cards, anchorsFor(office, desks)), event.kind, Date.now());
      planMotion(event.floor, event.kind, true);
    });
    return () => { stopFloor(); stopProjection(); };
  }, []);

  useEffect(() => {
    planMotion(floor, 'delta');
  }, [floor.agents, live, activityExpiry]);

  useEffect(() => {
    const next = followFull(fullShownRef.current, full, fallback === null, floor);
    fullShownRef.current = next.shown;
    if (next.heard !== null) setFullHeard(next.heard);
  }, [full, fallback, confirmedMode !== null]);

  useEffect(() => {
    const next = followMode(mode, confirmedMode, floor);
    setMode(next.shown);
    if (next.heard !== null) setModeHeard(next.heard);
  }, [confirmedMode?.mode, confirmedMode?.line, floor.phase]);

  // The shelf shows the daemon's count of goals first finished on its local day, never the agents that are Done now.
  const shelf = useMemo(() => shelfView(floor), [floor.phase, floor.stale, floor.epoch, floor.snapshotEpoch, floor.finishedToday]);
  const caption = shelfCaption(shelf);
  // One reading of the rows, for the mirror list and the tags below. It cannot disagree with the panels, because there is one source.
  const fields = useMemo(() => {
    const now = performance.now();
    return new Map(floor.agents.map((row) => [row.id, fieldsOf(row, { live, now, wallNow: Date.now() })] as const));
  }, [floor.agents, live, activityExpiry]);
  const states = useMemo(() => {
    const activity = { live, now: performance.now(), wallNow: Date.now() };
    return new Map(floor.agents.map((row) => [row.id, officeStateOf(row, activity)] as const));
  }, [floor.agents, live, activityExpiry]);
  const asking = useMemo(() => {
    const activity = { live, now: performance.now(), wallNow: Date.now() };
    return new Set(floor.agents.filter((row) => asksOpenQuestion(row, activity)).map((row) => row.id));
  }, [floor.agents, live, activityExpiry]);

  const needWords = useMemo(() => new Map(needsOf(floor.agents, floor.cards, { live, now: performance.now(), wallNow: Date.now() })
    .filter((need) => need.state === 'needs-you' || need.state === 'ready')
    .map((need) => [need.id, NEED_LABEL[need.state]] as const)), [floor.agents, floor.cards, live, activityExpiry]);

  useEffect(() => {
    const members = floor.agents.map((row) => {
      const station = STATIONS[states.get(row.id) ?? 'idle'].badge;
      return { id: row.id, look: lookOf(row), accessory: accessoryOf(row.role),
        badge: station === null && needWords.has(row.id) ? 'review' as const : station };
    });
    crewRef.current = { members, goals, shipped: shelfParcels(shelf) };
    drawCrew();
    scheduleRef.current();
  }, [floor.agents, states, goals, shelf, needWords]);

  useEffect(() => {
    selectedRef.current = selectedId;
    scheduleRef.current();
  }, [selectedId]);

  useEffect(() => {
    stillRef.current = floor.stale || floor.phase !== 'live';
    scheduleRef.current();
  }, [floor.stale, floor.phase]);

  useEffect(() => {
    const box = boxRef.current;
    const canvas = canvasRef.current;
    if (box === null || canvas === null) return;
    const activeBox = box;
    const motionPreference = window.matchMedia('(prefers-reduced-motion: reduce)');
    reducedMotionRef.current = motionPreference.matches;
    const colourPreference = window.matchMedia('(prefers-color-scheme: dark)');
    let cancelled = false;
    let lost = false;
    let frame: number | null = null;
    let huddleTimer: ReturnType<typeof setTimeout> | null = null;
    let drag: { id: number; x: number; moved: number } | null = null;
    let css = { width: box.clientWidth, height: box.clientHeight };
    let narrowShown: boolean | null = null;
    const rem = Number.parseFloat(getComputedStyle(document.documentElement).fontSize) || 16;
    const stage = activeBox.closest('.floor-stage') ?? document.body;
    const tagSizes = new WeakMap<HTMLElement, { key: string; full: Size; compact: Size }>();
    // An overlay that hides while it has the keyboard hands it to the view buttons.
    const viewButtons = { focus: () => zoomRef.current?.querySelector('button')?.focus() };
    setSupport('loading');

    function schedule() {
      if (frame === null && !lost && rendererRef.current !== null && document.visibilityState !== 'hidden') {
        frame = requestAnimationFrame(tick);
      }
    }

    function place(element: HTMLElement, rect: { left: number; top: number; width?: number; height?: number }) {
      element.style.left = `${rect.left}px`;
      element.style.top = `${rect.top}px`;
      if (rect.width !== undefined) element.style.width = `${rect.width}px`;
      if (rect.height !== undefined) element.style.height = `${rect.height}px`;
    }

    /** A tag's size whole and with the name alone, measured again only when what it says changes. The frame sets its form after. */
    function sizesOf(tag: HTMLElement): { full: Size; compact: Size } {
      const key = `${tag.dataset.selected}|${tag.textContent}`;
      let known = tagSizes.get(tag);
      if (known?.key !== key) {
        tag.dataset.compact = 'false';
        const full = { width: tag.offsetWidth, height: tag.offsetHeight };
        tag.dataset.compact = 'true';
        known = { key, full, compact: { width: tag.offsetWidth, height: tag.offsetHeight } };
        tagSizes.set(tag, known);
      }
      return known;
    }

    // A panel opening, closing or resized over the office moves the view and what is laid over it, drawn or not.
    const covering = watchCover(activeBox, stage, rem, ({ narrow }) => {
      if (narrow !== narrowShown) {
        // The view buttons move between the office and the top bar, so the one with the keyboard is found again after.
        const at = [...(zoomRef.current?.querySelectorAll('button') ?? [])].findIndex((button) => button === document.activeElement);
        refocusRef.current = at < 0 ? null : at;
        narrowShown = narrow;
        setNarrow(narrow);
      }
      schedule();
    });

    function tick() {
      frame = null;
      const renderer = rendererRef.current;
      if (renderer === null || document.visibilityState === 'hidden') return;
      const reduced = motionPreference.matches || stillRef.current;
      renderer.setInsets(covering.measure().camera);
      const now = Date.now();
      const clock = performance.now();
      const crew = crewRef.current;
      settle(now, reduced);
      envelopesRef.current = activeEnvelopes(envelopesRef.current, clock, reduced);
      if (visitsRef.current.length > 0 || huddlesRef.current.length > 0) {
        const plannedFloor = planInputsRef.current?.floor;
        const rows = new Map(plannedFloor?.agents.map((row) => [row.id, row]) ?? []);
        const seated = new Set(motionsRef.current.map((motion) => motion.desk.agentId));
        const activity = { live: plannedFloor !== undefined && plannedFloor.phase === 'live' && !plannedFloor.stale,
          now: clock, wallNow: now };
        visitsRef.current = activeVisits(visitsRef.current, now, rows, seated, activity);
        huddlesRef.current = activeHuddles(huddlesRef.current, now, rows, seated, activity);
      }
      const door = planInputsRef.current?.office.plan.door.spot;
      const walks = door === undefined ? [] : [...motionsRef.current, ...exitMotions()]
        .filter((motion) => inOffice(rosterRef.current.get(motion.desk.agentId), motion, door, now));
      const basePosed = charactersAt(walks, now, reduced);
      const posed = visitsRef.current.length === 0 && huddlesRef.current.length === 0 ? basePosed : basePosed.map((character) => {
        const visit = visitsRef.current.find((candidate) => candidate.senderId === character.agentId);
        if (visit !== undefined) return visitPosed(visit, character, now, reduced);
        const gathered = memberOf(huddlesRef.current, character.agentId);
        return gathered === undefined ? character : huddlePosed(gathered.huddle, gathered.member, character, now, reduced);
      });
      renderer.render({
        posed,
        selectedId: selectedRef.current,
        notes: cardNotesAt(notesRef.current, now, reduced).filter((note) => note.shown),
        goals: crew.goals,
        shipped: crew.shipped,
        envelopes: envelopesAt(envelopesRef.current, clock),
        animate: !reduced,
      }, clock);
      for (const [id, button] of buttonsRef.current) {
        const rect = renderer.figureRect(id);
        showOverlay(button, rect !== null && onCanvas({ x: rect.left + rect.width / 2, y: rect.top + rect.height }, css), viewButtons);
        if (rect !== null) place(button, rect);
      }
      for (const [id, mark] of marksRef.current) {
        const rect = renderer.badgeRect(id);
        showOverlay(mark, rect !== null && onCanvas({ x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 }, css), viewButtons);
        if (rect !== null) place(mark, rect);
      }
      const tags = [...tagsRef.current].flatMap(([id, tag]) => {
        const presence = rosterRef.current.get(id) ?? 'gone';
        if (tag.dataset.presence !== presence) tag.dataset.presence = presence;
        const point = renderer.tagPoint(id);
        tag.hidden = point === null || !onCanvas(point, css);
        return point === null || tag.hidden ? [] : [{ id, element: tag, x: point.x, y: point.y, ...sizesOf(tag), whole: keepsWhole(tag, buttonsRef.current.get(id)) }];
      });
      layTags(tags);
      if (boardRef.current !== null) place(boardRef.current, renderer.boardRect());
      const { zoom, azimuth } = renderer.view;
      activeBox.dataset.zoom = zoom.toFixed(2);
      activeBox.dataset.azimuth = azimuth.toFixed(3);
      if (!reduced) schedule();
      else {
        // Reduced motion draws no frames of its own, so a huddle's end is drawn by a timer.
        const change = nextHuddleChange(huddlesRef.current, now);
        if (huddleTimer !== null) clearTimeout(huddleTimer);
        huddleTimer = change === null ? null : setTimeout(schedule, change - now + 1);
      }
    }

    function resize() {
      rendererRef.current?.resize(css.width, css.height, window.devicePixelRatio);
      schedule();
    }

    function onTheme() {
      const night = readNight();
      activeBox.dataset.night = String(night);
      rendererRef.current?.setNight(night);
      schedule();
    }

    function onVisibilityChange() {
      if (document.visibilityState === 'hidden') {
        if (frame !== null) cancelAnimationFrame(frame);
        frame = null;
      } else {
        schedule();
      }
    }

    function onMotionChange() {
      reducedMotionRef.current = motionPreference.matches;
      if (motionPreference.matches) envelopesRef.current = [];
      schedule();
    }

    function pressStart(event: PointerEvent) {
      if (event.button === 0) drag = { id: event.pointerId, x: event.clientX, moved: 0 };
    }

    function pressMove(event: PointerEvent) {
      if (drag === null || event.pointerId !== drag.id) return;
      const dx = event.clientX - drag.x;
      drag.x = event.clientX;
      drag.moved += Math.abs(dx);
      if (drag.moved <= DRAG_THRESHOLD) return;
      // Captured only once it is a drag, so a press that does not move still clicks the button under it.
      if (!activeBox.hasPointerCapture(event.pointerId)) activeBox.setPointerCapture(event.pointerId);
      activeBox.dataset.dragging = 'true';
      rendererRef.current?.turnBy(-dx * TURN_PER_PIXEL);
      schedule();
    }

    function pressEnd(event: PointerEvent) {
      if (drag === null || event.pointerId !== drag.id) return;
      drag = null;
      delete activeBox.dataset.dragging;
    }

    function onWheel(event: WheelEvent) {
      const factor = wheelZoom(event);
      if (factor === null) return;
      event.preventDefault();
      rendererRef.current?.zoomBy(factor);
      schedule();
    }

    function pressKey(event: KeyboardEvent) {
      const turn = turnKey(event);
      if (turn === null) return;
      event.preventDefault();
      rendererRef.current?.turnBy(turn);
      schedule();
    }

    function onContextLost() {
      lost = true;
      drawingRef.current = false;
      if (frame !== null) cancelAnimationFrame(frame);
      frame = null;
      settle(Date.now(), true);
      setSupport('lost');
    }

    scheduleRef.current = schedule;
    document.addEventListener('visibilitychange', onVisibilityChange);
    motionPreference.addEventListener('change', onMotionChange);
    colourPreference.addEventListener('change', onTheme);
    box.addEventListener('pointerdown', pressStart);
    box.addEventListener('pointermove', pressMove);
    box.addEventListener('pointerup', pressEnd);
    box.addEventListener('pointercancel', pressEnd);
    box.addEventListener('wheel', onWheel, { passive: false });
    box.addEventListener('keydown', pressKey);
    // A tag stays whole while its Inspect button has the keyboard, so a still office lays its tags again when that moves.
    box.addEventListener('focusin', schedule);
    box.addEventListener('focusout', schedule);
    canvas.addEventListener('webglcontextlost', onContextLost);
    const themeWatch = new MutationObserver(onTheme);
    themeWatch.observe(document.body, { subtree: true, attributes: true, attributeFilter: ['data-theme'] });
    const observer = new ResizeObserver(([entry]) => {
      css = { width: entry.contentRect.width, height: entry.contentRect.height };
      resize();
    });
    observer.observe(box);

    loadRenderer().then((module) => {
      if (cancelled) return;
      const started = startRenderer(() => new module.OfficeRenderer(canvas, built(choiceFor(getFloorState()).id).layout));
      if (started.renderer === null) {
        setSupport('unavailable');
        return;
      }
      rendererRef.current = started.renderer;
      drawingRef.current = true;
      drawCrew();
      onTheme();
      resize();
      setSupport('ready');
    }, () => {
      rendererModule = null;
      if (!cancelled) setSupport('unavailable');
    });

    return () => {
      cancelled = true;
      drawingRef.current = false;
      scheduleRef.current = () => {};
      if (frame !== null) cancelAnimationFrame(frame);
      if (huddleTimer !== null) clearTimeout(huddleTimer);
      covering.stop();
      document.removeEventListener('visibilitychange', onVisibilityChange);
      motionPreference.removeEventListener('change', onMotionChange);
      colourPreference.removeEventListener('change', onTheme);
      box.removeEventListener('pointerdown', pressStart);
      box.removeEventListener('pointermove', pressMove);
      box.removeEventListener('pointerup', pressEnd);
      box.removeEventListener('pointercancel', pressEnd);
      box.removeEventListener('wheel', onWheel);
      box.removeEventListener('keydown', pressKey);
      box.removeEventListener('focusin', schedule);
      box.removeEventListener('focusout', schedule);
      canvas.removeEventListener('webglcontextlost', onContextLost);
      themeWatch.disconnect();
      observer.disconnect();
      rendererRef.current?.dispose();
      rendererRef.current = null;
    };
  }, [attempt]);

  // The buttons and tags laid over the figures appear with the drawing; place them at once, even with motion reduced.
  useEffect(() => scheduleRef.current(), [support, desks, narrow]);

  useLayoutEffect(() => {
    const at = refocusRef.current;
    refocusRef.current = null;
    if (at !== null) zoomRef.current?.querySelectorAll('button')[at]?.focus();
  }, [narrow]);

  function zoom(factor: number): void {
    rendererRef.current?.zoomBy(factor);
    scheduleRef.current();
  }

  function turn(radians: number): void {
    rendererRef.current?.turnBy(radians);
    scheduleRef.current();
  }

  const rows = new Map([...leavers, ...floor.agents].map((row) => [row.id, row]));
  const selectable = floor.phase !== 'closed' && !floor.stale;
  const inBar = narrow && bar !== null;
  const fullLine = confirmed && fallback === null && full !== null ? <p className="office-full">{full}</p> : null;
  const pausedOrHalted = mode?.mode === 'live' ? undefined : mode;
  const modeLine = pausedOrHalted === undefined ? null : <p className="office-mode">{pausedOrHalted.line}</p>;
  // The arrow keys turn the view from inside the office, so the buttons name them only there.
  const viewButtons = drawn ? <div className="office-zoom" role="group" aria-label="Office view" ref={zoomRef}>
    <button type="button" aria-label="Turn left" aria-keyshortcuts={inBar ? undefined : 'ArrowLeft'} onClick={() => turn(TURN_STEP)}>↺</button>
    <button type="button" aria-label="Turn right" aria-keyshortcuts={inBar ? undefined : 'ArrowRight'} onClick={() => turn(-TURN_STEP)}>↻</button>
    <button type="button" aria-label="Zoom in" onClick={() => zoom(ZOOM_STEP)}>+</button>
    <button type="button" aria-label="Zoom out" onClick={() => zoom(1 / ZOOM_STEP)}>−</button>
  </div> : null;

  return (
    <>
      <div className="office" ref={boxRef} data-office={support} data-mode={pausedOrHalted?.mode}
        data-layout={choice.id} data-full={confirmed ? choice.full : undefined}>
        <canvas className="office-canvas" ref={canvasRef} key={attempt} hidden={fallback !== null}>
          {/* The canvas is opaque to anything that is not looking at it. This
              list keeps the cast in stable desk order while characters move,
              with each agent's fields whole: for assistive technology, and for
              anything else reading the page.

              `data-stale`, `data-epoch` and `data-phase` sit on the list
              rather than on each row because that is the truth's own shape:
              neither silence, nor a subscription, nor where the transport is
              says anything about one agent. An empty list looks identical
              whether the floor is connecting, resyncing or given up on, and
              the epoch tells two identical-looking floors either side of a
              recovery apart. */}
          <ul
            className="cast-mirror"
            aria-label="Crew in the office"
            data-stale={floor.stale ? 'true' : 'false'}
            data-epoch={floor.epoch}
            data-phase={floor.phase}
          >
            {desks.map(({ agentId: id }) => {
              const said = fields.get(id);
              return (
                <li
                  key={id}
                  data-agent-id={id}
                  data-role={said?.role ?? ''}
                  data-status={said?.status ?? ''}
                  data-goal={said?.goal ?? ''}
                  data-goal-status={said?.goalStatus ?? ''}
                  data-cost={said?.cost ?? ''}
                  data-question={said?.question ?? ''}
                  data-live-activity={said?.liveActivity ?? ''}
                >
                  {id}
                </li>
              );
            })}
          </ul>
        </canvas>
        {drawn ? <BoardNotes count={board.count} hidden={board.hidden} goalsHidden={goalsPastBoard(floor.agents, choice.id)}
          buttonRef={boardRef} onOpenTasks={onOpenTasks} /> : null}
        {drawn ? [...desks.map(({ agentId }) => agentId), ...leavers.map((row) => row.id)].map((id) => (
          <span key={id} className="office-tag" aria-hidden="true" data-tag-agent-id={id}
            data-selected={selectedId === id ? 'true' : 'false'} data-office-state={states.get(id)}
            data-badge={STATIONS[states.get(id) ?? 'idle'].badge ?? ''}
            ref={(tag) => { if (tag === null) tagsRef.current.delete(id); else tagsRef.current.set(id, tag); }}>
            <NameTag row={rows.get(id)} id={id} />
            {needWords.get(id) ?? fields.get(id)?.liveActivity ? <small className="office-tag-need" data-need={needWords.has(id)}>{needWords.get(id) ?? fields.get(id)?.liveActivity}</small> : null}
          </span>
        )) : null}
        {drawn && selectable ? desks.map(({ agentId: id }) => (
          <Fragment key={id}>
            <button
              ref={(button) => { if (button === null) buttonsRef.current.delete(id); else buttonsRef.current.set(id, button); }}
              type="button"
              className="agent-select"
              data-select-agent-id={id}
              aria-label={`Inspect ${typeof rows.get(id)?.name === 'string' ? String(rows.get(id)?.name) : id}`}
              aria-pressed={selectedId === id}
              aria-controls={selectedId === id ? 'selected-agent-detail' : undefined}
              onClick={() => onSelectAgent(id)}
            />
            {asking.has(id) ? <button
              ref={(mark) => { if (mark === null) marksRef.current.delete(id); else marksRef.current.set(id, mark); }}
              type="button"
              className="office-question"
              data-question-agent-id={id}
              aria-label={`Open ${nameOf(rows.get(id), id)}'s question`}
              onClick={() => onOpenQuestion(id)}
            /> : null}
          </Fragment>
        )) : null}
        {inBar ? null : fullLine}
        <p className="office-shelf" data-shelf={shelf.state}>
          <span className="office-shelf-label">{caption.label}</span>
          <strong className="office-shelf-value">{caption.value}</strong>
          {caption.day === null ? null : <small className="office-shelf-day">{caption.day}</small>}
        </p>
        <p className="office-full-heard" role="status">{fullHeard}</p>
        {inBar ? createPortal(<>{modeLine}{fullLine}{viewButtons}</>, bar) : viewButtons}
        {inBar ? null : modeLine}
        <p className="office-mode-heard" role="status">{modeHeard}</p>
        {floor.agents.length !== 0 || floor.phase !== 'live' || floor.stale ? null : (
          <div className="office-empty">
            <p>No current agents.</p>
            {hireSwitchOn(floor) ? null : <HireEntry floor={floor} id="hire-open-office" onAddAgent={onAddAgent} />}
          </div>
        )}
      </div>
      {support === 'loading' ? <p role="status">Loading the office.</p> : null}
      {fallback === null ? null
        : <ReadError className="office-failure" message={fallback} onRetry={() => setAttempt((count) => count + 1)} />}
    </>
  );
}
