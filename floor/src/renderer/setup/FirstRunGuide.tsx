// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useRef, useState } from 'react';
import type { FloorState } from '../floor/store.ts';
import type { GuideStep } from './guide.ts';
import { guideSnapshotConfirmed, guideSteps } from './guide.ts';
import { SetupView } from './SetupView.tsx';
import { FloorSwitches, WorkspaceSettings } from '../settings/SettingsView.tsx';
import { refreshFloorSnapshot } from '../floor/stream.ts';
import { Button, cn } from '../../ui/index.ts';

const GUIDE = cn('first-run-guide absolute inset-y-0 right-0 z-(--layer-hire-wizard) box-border flex w-[min(var(--hire-wizard-width),100%)] flex-col gap-4 border-l max-[800px]:pb-14 border-line bg-surface p-4 font-sans text-ink shadow-sheet [grid-area:auto] group-has-[[data-terminal-expanded=true]]/stage:relative group-has-[[data-terminal-expanded=true]]/stage:inset-auto group-has-[[data-terminal-expanded=true]]/stage:row-2 group-has-[[data-terminal-expanded=true]]/stage:col-[1/-1] group-has-[[data-terminal-expanded=true]]/stage:w-auto');

function FloorActions({ floor }: { readonly floor: FloorState }) {
  const report = floor.settings;
  if (report === null) return <p role="status">Settings are unavailable. Recheck to try again.</p>;
  if (!guideSnapshotConfirmed(floor)) return null;
  return <>
    <p>These actions are optional. Turn on the ones you want to use from this page.</p>
    <p>This step is done once you have seen it, whichever switches are on.</p>
    <div className="settings-view"><FloorSwitches report={report} headingId="guide-switches" /></div>
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
  return <aside className={GUIDE}
    role="dialog" aria-labelledby="first-run-guide-title"
    onKeyDown={(event) => { if (event.key === 'Escape') { event.stopPropagation(); onClose(); } }}>
    <header className="guide-head flex items-center justify-between gap-2">
      <h2 id="first-run-guide-title" ref={heading} tabIndex={-1} className="m-0 text-title font-semibold focus:outline-0">First-run guide</h2>
      <Button size="sm" onClick={onClose}>Close</Button></header>
    <ol className="guide-index m-0 flex list-none flex-wrap gap-2 p-0" aria-label="Guide steps">{steps.map((entry) => <li key={entry.key}>
      <Button aria-current={entry.key === step ? 'step' : undefined} onClick={() => onStep(entry.key)}
        className={cn('h-auto flex-col items-start gap-0 py-1.5 text-left', entry.key === step && 'border-accent bg-accent-soft hover:bg-accent-soft')}>
        {entry.label}<span className="text-caption font-normal text-muted">{entry.status}</span></Button>
    </li>)}</ol>
    <div className="guide-body grid min-h-0 flex-1 content-start gap-3 overflow-auto text-body leading-reading [overflow-wrap:anywhere] [&_p]:m-0">
      <h3 className="m-0 text-title font-semibold">{current.label}</h3>
      {step === 'welcome' ? null : <p className="guide-status font-medium" role="status">{current.status}</p>}
      {!confirmed ? <p role="status">Waiting for current information from Dudes. Recheck to try again.</p> : null}
      {step === 'welcome' ? <p>Dudes supervises coding agents while they work towards a goal.
        The Floor shows their current state and offers the actions you enable locally.</p> : null}
      {step === 'engines' ? <>
        <p>This step is complete when Dudes finds an app. Sign in when you hire an agent.</p>
        <p>Hire offers Claude, plus Codex and Pi when they are enabled in Settings. Other apps are listed
          so you can check their setup.</p>
        <SetupView floor={floor} />
      </> : null}
      {step === 'workspaces' ? floor.settings === null ? <p>Settings are unavailable. Recheck to try again.</p> : <>
        <p>Add a project. The list updates after Dudes loads the change.</p>
        <WorkspaceSettings report={floor.settings} floor={floor} headingId="guide-workspaces" />
      </> : null}
      {step === 'actions' ? <FloorActions floor={floor} /> : null}
    </div>
    <footer className="guide-navigation flex justify-between gap-2 border-t border-line pt-3">
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
