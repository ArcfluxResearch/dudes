import { Bell, Plus } from 'lucide-react';
import { StrictMode, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { createRoot } from 'react-dom/client';

import './lab.css';
import {
  Badge, Button, Card, Choice, Dialog, Disclosure, Field, IconButton, Input, Menu, MenuItem, MenuSeparator, Notice, Panel, Select, Switch, Tab,
  TabList, Textarea, Toast, cn,
} from '../src/ui/index.ts';

const CHARACTERS = [
  { value: '', label: 'Automatic' },
  { value: 'c', label: 'Character C' },
  { value: 'd', label: 'Character D', disabled: true },
  { value: 'e', label: 'Character E' },
  { value: 'dan', label: 'Dan' },
];
const TABS = ['Overview', 'Memory', 'Git'];

function Section({ id, title, children }: { readonly id: string; readonly title: string; readonly children: ReactNode }) {
  return <section id={id} aria-label={title} className="flex flex-col gap-3">
    <h2 className="m-0 text-title font-semibold">{title}</h2>
    <div className="flex flex-wrap items-start gap-3">{children}</div>
  </section>;
}

function ActionsMenu({ id, label, note }: { readonly id: string; readonly label: string; readonly note: (what: string) => void }) {
  return <Menu label={label}>
    <MenuItem onSelect={() => note(`${id}:change-model`)}>Change model</MenuItem>
    <MenuItem onSelect={() => note(`${id}:pause`)}>Pause</MenuItem>
    <MenuSeparator />
    <MenuItem onSelect={() => note(`${id}:open`)} disabled>Open the real CLI</MenuItem>
    <MenuItem onSelect={() => note(`${id}:retire`)} danger>Retire</MenuItem>
  </Menu>;
}

function Lab() {
  const [last, setLast] = useState('');
  const [page, setPage] = useState('');
  const [inDialog, setInDialog] = useState('c');
  const [pageEnabled, setPageEnabled] = useState(false);
  const [wired, setWired] = useState(true);
  const [engine, setEngine] = useState('claude');
  const [ownCopy, setOwnCopy] = useState(false);
  const [tab, setTab] = useState(0);
  const [toast, setToast] = useState(true);
  const [presses, setPresses] = useState(0);
  const dialog = useRef<HTMLDialogElement>(null);
  const tabs = TABS.map((name, at) => <Tab key={name} id={`tab-${name}`} selected={tab === at} onClick={() => setTab(at)}>{name}</Tab>);

  return <main id="lab" data-last={last} data-page-select={page} data-dialog-select={inDialog} data-presses={presses}
    className="flex min-h-screen flex-col gap-8 bg-bg p-8 font-sans text-base text-ink">
    <button id="start" type="button" className="self-start">Start of the page</button>

    <Section id="buttons" title="Buttons">
      {(['primary', 'secondary', 'ghost', 'danger'] as const).map((variant) => <Button key={variant} variant={variant}
        onClick={() => setPresses((count) => count + 1)} data-case={variant}>{variant}</Button>)}
      <Button variant="primary" size="sm" icon={<Plus size={16} />} data-case="small">Small with icon</Button>
      <Button loading data-case="loading" onClick={() => setPresses((count) => count + 100)}>Saving</Button>
      <Button disabled data-case="disabled">Disabled</Button>
      <IconButton label="Notifications" icon={<Bell size={16} />} data-case="icon" />
    </Section>

    <Section id="fields" title="Fields">
      <div className="w-72"><Field label="Name" hint="Shown on the office floor." id="field-name"><Input placeholder="Ada" /></Field></div>
      <div className="w-72"><Field label="Role" error="Say what this agent does." id="field-role"><Textarea /></Field></div>
      <div className="w-72"><Field label="Character" id="field-character">
        <Select options={CHARACTERS} value={page} onValueChange={setPage} placeholder="Choose one" />
      </Field></div>
    </Section>

    <Section id="choices" title="Choice">
      <div className="flex w-96 flex-col gap-2">
        <Choice type="radio" name="lab-engine" value="claude" checked={engine === 'claude'} onChange={() => setEngine('claude')}
          label="Claude" note="Does not ask before it acts." />
        <Choice type="radio" name="lab-engine" value="codex" checked={engine === 'codex'} onChange={() => setEngine('codex')}
          label="Codex" note="Can only write inside this project." />
        <Choice type="checkbox" name="lab-copy" checked={ownCopy} onChange={(event) => setOwnCopy(event.target.checked)}
          label="Work in its own copy of the project" />
        <Choice plain type="radio" name="lab-plain" value="a" defaultChecked label="A plain row" />
      </div>
      <output id="choice-now" className="text-muted">{engine}{ownCopy ? ' copy' : ''}</output>
    </Section>

    <Section id="notices" title="Notice">
      <div className="flex w-96 flex-col gap-2">
        <Notice tone="info" role="status">The model list is being asked.</Notice>
        <Notice tone="ok" role="status">Hired Ada.</Notice>
        <Notice tone="warn" role="status">Tickets are off.</Notice>
        <Notice tone="danger" role="alert">Name is required.</Notice>
      </div>
    </Section>

    <Section id="menus" title="Menu">
      <ActionsMenu id="page" label="More actions" note={setLast} />
    </Section>

    <Section id="switches" title="Switches">
      <div className="w-96"><Switch checked={pageEnabled} onCheckedChange={setPageEnabled} label="Hire from the page" description="Lets you hire agents without the command line." /></div>
      <div className="w-96"><Switch checked={wired} onCheckedChange={setWired} label="Notifications" /></div>
      <div className="w-96"><Switch checked={false} onCheckedChange={() => {}} label="Applying" busy /></div>
    </Section>

    <Section id="tabs" title="Tabs">
      <TabList label="Agent">{tabs}</TabList>
      <output id="tab-now" className="text-muted">{TABS[tab]}</output>
    </Section>

    <Section id="badges" title="Badges">
      {(['needs-you', 'ready', 'working', 'waiting', 'done', 'idle'] as const).map((state) => <Badge key={state} state={state} />)}
    </Section>

    <Section id="surfaces" title="Card and Panel">
      <Card className="w-72">A card holds a short body.</Card>
      <Panel title="Log" className="w-72" actions={<Button size="sm" variant="ghost">Clear</Button>}>A panel names itself.</Panel>
    </Section>

    <Section id="toasts" title="Toast">
      {toast ? <Toast tone="ok" onDismiss={() => setToast(false)}>Accepted. The ticket is Done.</Toast> : <span id="toast-gone">Dismissed</span>}
    </Section>

    <Section id="disclosure" title="Disclosure">
      <Disclosure summary="More about this agent" id="disclosure-one">Its role, its model and its last turn.</Disclosure>
    </Section>

    <Section id="dialogs" title="Dialog">
      <Button id="open-dialog" variant="primary" onClick={() => dialog.current?.showModal()}>Open the dialog</Button>
      <Dialog ref={dialog} id="the-dialog" size="sm" aria-labelledby="dialog-title">
        <div className="flex flex-col gap-4 p-6">
          <h2 id="dialog-title" className="m-0 text-title font-semibold">Hire an agent</h2>
          <Field label="Name" id="dialog-name"><Input /></Field>
          <Field label="Character" id="dialog-character">
            <Select options={CHARACTERS} value={inDialog} onValueChange={setInDialog} />
          </Field>
          <div className="flex items-center justify-between gap-2">
            <ActionsMenu id="dialog" label="Dialog actions" note={setLast} />
            <Button id="close-dialog" onClick={() => dialog.current?.close()}>Close</Button>
          </div>
        </div>
      </Dialog>
    </Section>

    <p className={cn('m-0 font-mono text-small text-muted')}>Monospace sample: duded 0.99.0</p>
    <button id="end" type="button" className="self-start">End of the page</button>
  </main>;
}

createRoot(document.getElementById('root')!).render(<StrictMode><Lab /></StrictMode>);
