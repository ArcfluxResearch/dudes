// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useRef, useState } from 'react';
import type { FloorState } from '../floor/store.ts';
import type { GuideStep } from './guide.ts';
import { guideSnapshotConfirmed, guideSteps } from './guide.ts';
import { SetupView } from './SetupView.tsx';
import { FloorSwitches, WorkspaceSettings } from '../settings/SettingsView.tsx';
import { refreshFloorSnapshot } from '../floor/stream.ts';
import { Button, ICON, RING, cn } from '../../ui/index.ts';
import { HINT } from '../PageParts.tsx';
import { X } from 'lucide-react';

/**
 * Docked over the right edge of the stage, full height. With an expanded terminal it joins the stage's rows instead,
 * above the terminal, the way the stage lays out when both are open.
 */
const EXPANDED = "[.floor-stage:has(.terminal-panel[data-terminal-expanded='true']):not(:has(.control-panel[data-view='board']))>&]";
const DOCK = cn(
  'absolute inset-y-0 right-0 z-3 row-[1/-1] box-border flex w-[min(608px,100%)] flex-col border-l border-line bg-surface font-sans text-ink shadow-sheet',
  `${EXPANDED}:relative ${EXPANDED}:inset-auto ${EXPANDED}:row-[2] ${EXPANDED}:col-[1/-1] ${EXPANDED}:w-auto`,
);

function FloorActions({ floor }: { readonly floor: FloorState }) {
  const report = floor.settings;
  if (report === null) return <p role="status" className={HINT}>Settings are unavailable. Recheck to try again.</p>;
  if (!guideSnapshotConfirmed(floor)) return null;
  return <>
    <p className={HINT}>These actions are optional. Turn on the ones you want to use from this page.</p>
    <p className={HINT}>This step is done once you have seen it, whichever switches are on.</p>
    <div data-ui="settings"><FloorSwitches report={report} headingId="guide-switches" /></div>
  </>;
}

export function FirstRunGuideView({ floor, step, onStep, onClose }: {
  readonly floor: FloorState;
  readonly step: GuideStep;
  readonly onStep: (step: GuideStep) => void;
  readonly onClose: () => void;
}) {
  const confirmed = guideSnapshotConfirmed(floor);
  const [actionsSeen, setActionsSeen] = useState(false);
  if (!actionsSeen && step === 'actions' && confirmed && floor.settings !== null) setActionsSeen(true);
  const steps = guideSteps(floor, actionsSeen);
  const at = steps.findIndex((entry) => entry.key === step);
  const current = steps[at];
  const heading = useRef<HTMLHeadingElement>(null);
  useEffect(() => { heading.current?.focus(); }, [step]);
  return <aside className={cn('first-run-guide', DOCK)} role="dialog" aria-labelledby="first-run-guide-title"
    onKeyDown={(event) => { if (event.key === 'Escape') { event.stopPropagation(); onClose(); } }}>
    <header className="flex items-center justify-between gap-3 border-b border-line px-4 py-3">
      <h2 id="first-run-guide-title" ref={heading} tabIndex={-1}
        className="m-0 rounded-sm text-title font-semibold text-ink outline-0 focus-visible:outline-2 focus-visible:outline-solid focus-visible:outline-focus focus-visible:outline-offset-2">First-run guide</h2>
      <Button variant="ghost" size="sm" icon={<X {...ICON} />} onClick={onClose}>Close</Button>
    </header>
    <ol className="m-0 grid list-none grid-cols-2 gap-2 p-4 pb-0 min-[641px]:grid-cols-4" aria-label="Guide steps">{steps.map((entry) => <li key={entry.key} className="grid">
      <button type="button" aria-current={entry.key === step ? 'step' : undefined} data-state={entry.key === step ? 'current' : undefined} onClick={() => onStep(entry.key)}
        className={cn('grid cursor-pointer content-start gap-0.5 rounded-md border border-line-strong bg-surface p-2 text-left font-sans text-base font-medium text-ink',
          'transition-[color,background-color,border-color] hover:bg-surface-2 data-[state=current]:border-accent data-[state=current]:bg-accent-soft', RING)}>
        {entry.label}<span className="text-small font-normal text-muted">{entry.status}</span></button>
    </li>)}</ol>
    <div className="min-h-0 flex-1 overflow-auto p-4 [overflow-wrap:anywhere]">
      <div className="grid gap-3">
        <h3 className="m-0 text-title font-semibold text-ink">{current.label}</h3>
        {step === 'welcome' ? null : <p className="m-0 text-base font-medium text-ink" role="status">{current.status}</p>}
        {!confirmed ? <p role="status" className={HINT}>Waiting for current information from Dudes. Recheck to try again.</p> : null}
        {step === 'welcome' ? <p className={HINT}>Dudes supervises coding agents while they work towards a goal.
          The Floor shows their current state and offers the actions you enable locally.</p> : null}
        {step === 'engines' ? <>
          <p className={HINT}>This step is complete when Dudes finds an app. Sign in when you hire an agent.</p>
          <p className={HINT}>Hire offers Claude, plus Codex and Pi when they are enabled in Settings. Other apps are listed
            so you can check their setup.</p>
          <SetupView floor={floor} />
        </> : null}
        {step === 'workspaces' ? floor.settings === null ? <p className={HINT}>Settings are unavailable. Recheck to try again.</p> : <>
          <p className={HINT}>Add a project. The list updates after Dudes loads the change.</p>
          <div data-ui="settings"><WorkspaceSettings report={floor.settings} floor={floor} headingId="guide-workspaces" /></div>
        </> : null}
        {step === 'actions' ? <FloorActions floor={floor} /> : null}
      </div>
    </div>
    <footer className="flex items-center justify-between gap-2 border-t border-line px-4 py-3">
      <Button disabled={at === 0} onClick={() => onStep(steps[at - 1].key)}>Back</Button>
      <Button onClick={() => refreshFloorSnapshot()}>Recheck</Button>
      <Button variant="primary" disabled={at === steps.length - 1} onClick={() => onStep(steps[at + 1].key)}>Next</Button>
    </footer>
  </aside>;
}

export function FirstRunGuide({ floor, onClose }: {
  readonly floor: FloorState;
  readonly onClose: () => void;
}) {
  const [step, setStep] = useState<GuideStep>('welcome');
  return <FirstRunGuideView floor={floor} step={step} onStep={setStep} onClose={onClose} />;
}
