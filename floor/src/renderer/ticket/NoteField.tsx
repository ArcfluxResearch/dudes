// SPDX-License-Identifier: MIT OR Apache-2.0
//
// A ticket's note: shown as Markdown, and edited in a rich editor when the note survives it or in a plain Markdown
// text box with a preview when it does not. The rich editor is a lazy chunk, so a page that never edits a note never
// loads it, and a failed load leaves the text box.

import { useEffect, useState } from 'react';

import { lazyChunk } from '../../transport.ts';
import { Tab, TabList, Textarea, cn } from '../../ui/index.ts';
import { useFieldControl } from '../../ui/fieldContext.ts';
import { AgentMarkdown, MARKDOWN } from '../chat/markdown.tsx';
import { NOTE_LIMIT_BYTES, noteBytes, noteTooLong, toLf } from './noteText.ts';

type Editor = typeof import('./NoteRichEditor.tsx');

/** A note drawn as Markdown. Links are on here and nowhere else the renderer is used. */
export function NoteView({ text }: { readonly text: string }) {
  return <div className={cn('ticket-text md', MARKDOWN)}><AgentMarkdown text={text} links /></div>;
}

const UNFIT = 'This note uses Markdown the rich editor cannot keep as written, so it opens as text.';

type Mode =
  | { readonly kind: 'loading' }
  | { readonly kind: 'rich'; readonly editor: Editor }
  | { readonly kind: 'plain'; readonly why: string | null };

export function NoteField({ initial, value, onNote }: {
  /** The note as it was read; the editor starts from it and a save of an untouched note sends it back unchanged. */
  readonly initial: string;
  readonly value: string;
  readonly onNote: (value: string) => void;
}) {
  const field = useFieldControl();
  const [mode, setMode] = useState<Mode>({ kind: 'loading' });
  const [preview, setPreview] = useState(false);
  useEffect(() => {
    let live = true;
    lazyChunk(() => import('./NoteRichEditor.tsx')).then((editor) => {
      if (!live) return;
      setMode(editor.opensRich(initial) ? { kind: 'rich', editor } : { kind: 'plain', why: UNFIT });
    }, () => {
      if (live) setMode({ kind: 'plain', why: 'The rich editor did not load, so the note opens as text.' });
    });
    return () => { live = false; };
  }, [initial]);

  const tooLong = noteTooLong(value);
  const bytes = noteBytes(toLf(value));
  return <div className="flex flex-col gap-2">
    {mode.kind === 'loading' ? <p className="m-0 text-muted" role="status">Loading the editor…</p> : null}
    {mode.kind === 'rich' ? mode.editor.richEditor({ id: field.id, original: initial, onNote, onUnfit: () => { setMode({ kind: 'plain', why: UNFIT }); } }) : null}
    {mode.kind === 'plain' ? <>
      {mode.why === null ? null : <p className="m-0 text-small text-muted" role="note">{mode.why}</p>}
      <TabList label="Note view">
        <Tab selected={!preview} onClick={() => setPreview(false)}>Write</Tab>
        <Tab selected={preview} onClick={() => setPreview(true)}>Preview</Tab>
      </TabList>
      {preview
        ? (value.trim() === '' ? <p className="m-0 text-muted">Nothing to preview.</p> : <NoteView text={toLf(value)} />)
        : <Textarea rows={6} value={value} onChange={(event) => onNote(event.target.value)} />}
    </> : null}
    {tooLong === null
      ? <p className="m-0 text-small text-muted">{bytes.toLocaleString('en-US')} of {NOTE_LIMIT_BYTES.toLocaleString('en-US')} bytes</p>
      : <p className="m-0 text-small text-danger" role="alert">{tooLong}</p>}
  </div>;
}
