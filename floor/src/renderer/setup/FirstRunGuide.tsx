// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useRef, useState } from 'react';
import type { FloorState } from '../floor/store.ts';
import type { GuideStep } from './guide.ts';
import { guideSnapshotConfirmed, guideSteps } from './guide.ts';
import { SetupView, copyToClipboard } from './SetupView.tsx';
import { ADD_WORKSPACE, WorkspaceSettings, hireUnavailable } from '../settings/SettingsView.tsx';
import { refreshFloorSnapshot } from '../floor/stream.ts';

function Command({ text }: { readonly text: string }) {
  return <p className="guide-command"><code>{text}</code>
    <button type="button" onClick={() => copyToClipboard(text)}>Copy</button></p>;
}

function Switch({ label, on, command }: { readonly label: string; readonly on: boolean; readonly command: string }) {
  return <div><dt>{label}</dt><dd>{on ? 'On' : 'Off'}</dd><dd className="guide-switch-command"><Command text={command} /></dd></div>;
}

function FloorActions({ floor }: { readonly floor: FloorState }) {
  const report = floor.settings;
  if (report === null) return <p role="status">Settings are not reported. Recheck to ask for a fresh snapshot.</p>;
  const { hire, control, terminal } = report.switches;
  const reason = hireUnavailable(floor);
  return <>
    <p>These actions are optional. You can continue with Control and Terminal off.</p>
    <p>This step is done once you have seen it, whichever switches are on.</p>
    <dl className="guide-actions">
      <div><dt>Hire</dt><dd>{floor.onboarding === null ? 'Unavailable' : 'Available'}</dd></div>
      <Switch label="Hire switch" on={hire} command={`dudes config set floor.hire ${hire ? 'off' : 'on'}`} />
      <Switch label="Control" on={control}
        command={`dudes config set floor.control ${control ? 'off floor.goal off floor.retire off' : 'on'}`} />
      <Switch label="Terminal" on={terminal} command={`dudes config set floor.terminal ${terminal ? 'off' : 'on'}`} />
    </dl>
    {reason === null ? null : <p>{reason}</p>}
    <p>After a config edit, restart the daemon so it reads the file. A restart ends any running turn.</p>
    <Command text="dudes quickstart --restart" />
  </>;
}

export function FirstRunGuideView({ floor, step, onStep, onClose, onHire }: {
  readonly floor: FloorState;
  readonly step: GuideStep;
  readonly onStep: (step: GuideStep) => void;
  readonly onClose: () => void;
  readonly onHire: (opener: HTMLButtonElement) => void;
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
      <h3>{current.label}</h3><p className="guide-status" role="status">{current.status}</p>
      {!confirmed ? <p role="status">Waiting for a confirmed snapshot from the daemon. Recheck to try again.</p> : null}
      {step === 'welcome' ? <p>Dudes supervises coding agents while they work towards a goal.
        The Floor shows their current state and offers the actions you enable locally.</p> : null}
      {step === 'engines' ? <>
        <p>The browser Hire wizard offers Claude, and Codex and Pi when the daemon turns them on. The other
          registered engines are shown for inspection.</p>
        <SetupView floor={floor} />
      </> : null}
      {step === 'workspaces' ? floor.settings === null ? <p>Settings are not reported. Recheck to ask again.</p> : <>
        <p>Enroll a repository locally. The list updates when the running daemon adopts the enrollment.</p>
        <WorkspaceSettings report={floor.settings} headingId="guide-workspaces" onCopyAdd={() => copyToClipboard(ADD_WORKSPACE)} />
      </> : null}
      {step === 'actions' ? <FloorActions floor={floor} /> : null}
      {step === 'hire' ? <>
        <p>{current.status === 'Done' ? 'An agent is on the Floor.' : 'Open the Hire wizard, review its choices, and hire your first agent.'}</p>
        {floor.onboarding === null ? <p>Hire is unavailable. Check Projects and Floor actions, then Recheck.
          If the session has used its Hire allowance, open Dudes to start a new Floor session.</p> : null}
        <button type="button" disabled={!confirmed || floor.onboarding === null}
          onClick={(event) => onHire(event.currentTarget)}>Open Hire wizard</button>
      </> : null}
    </div>
    <footer className="guide-navigation">
      <button type="button" disabled={at === 0} onClick={() => onStep(steps[at - 1].key)}>Back</button>
      <button type="button" onClick={() => refreshFloorSnapshot()}>Recheck</button>
      <button type="button" disabled={at === steps.length - 1} onClick={() => onStep(steps[at + 1].key)}>Next</button>
    </footer>
  </aside>;
}

export function FirstRunGuide({ floor, onClose, onHire }: {
  readonly floor: FloorState;
  readonly onClose: () => void;
  readonly onHire: (opener: HTMLButtonElement) => void;
}) {
  const [step, setStep] = useState<GuideStep>('welcome');
  return <FirstRunGuideView floor={floor} step={step} onStep={setStep} onClose={onClose} onHire={onHire} />;
}
