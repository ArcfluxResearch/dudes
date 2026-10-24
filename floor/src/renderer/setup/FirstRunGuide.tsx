// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useRef, useState } from 'react';
import type { FloorState } from '../floor/store.ts';
import type { GuideStep } from './guide.ts';
import { guideSnapshotConfirmed, guideSteps } from './guide.ts';
import { SetupView } from './SetupView.tsx';
import { FloorSwitches, WorkspaceSettings } from '../settings/SettingsView.tsx';
import { refreshFloorSnapshot } from '../floor/stream.ts';

function FloorActions({ floor }: { readonly floor: FloorState }) {
  const report = floor.settings;
  if (report === null) return <p role="status">Settings are not reported. Recheck to ask for a fresh snapshot.</p>;
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
  return <aside className="first-run-guide" role="dialog" aria-labelledby="first-run-guide-title"
    onKeyDown={(event) => { if (event.key === 'Escape') { event.stopPropagation(); onClose(); } }}>
    <header className="guide-head"><h2 id="first-run-guide-title" ref={heading} tabIndex={-1}>First-run guide</h2>
      <button type="button" onClick={onClose}>Close</button></header>
    <ol className="guide-index" aria-label="Guide steps">{steps.map((entry) => <li key={entry.key}>
      <button type="button" aria-current={entry.key === step ? 'step' : undefined} onClick={() => onStep(entry.key)}>
        {entry.label}<span>{entry.status}</span></button>
    </li>)}</ol>
    <div className="guide-body">
      <h3>{current.label}</h3>
      {step === 'welcome' ? null : <p className="guide-status" role="status">{current.status}</p>}
      {!confirmed ? <p role="status">Waiting for a confirmed snapshot from the daemon. Recheck to try again.</p> : null}
      {step === 'welcome' ? <p>Dudes supervises coding agents while they work towards a goal.
        The Floor shows their current state and offers the actions you enable locally.</p> : null}
      {step === 'engines' ? <>
        <p>This step is complete when Dudes finds an app. Sign in when you hire an agent.</p>
        <p>The browser Hire wizard offers Claude, and Codex and Pi when the daemon turns them on. The other
          registered engines are shown for inspection.</p>
        <SetupView floor={floor} />
      </> : null}
      {step === 'workspaces' ? floor.settings === null ? <p>Settings are not reported. Recheck to ask again.</p> : <>
        <p>Enroll a repository locally. The list updates when the running daemon adopts the enrollment.</p>
        <WorkspaceSettings report={floor.settings} floor={floor} headingId="guide-workspaces" />
      </> : null}
      {step === 'actions' ? <FloorActions floor={floor} /> : null}
    </div>
    <footer className="guide-navigation">
      <button type="button" disabled={at === 0} onClick={() => onStep(steps[at - 1].key)}>Back</button>
      <button type="button" onClick={() => refreshFloorSnapshot()}>Recheck</button>
      <button type="button" disabled={at === steps.length - 1} onClick={() => onStep(steps[at + 1].key)}>Next</button>
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
