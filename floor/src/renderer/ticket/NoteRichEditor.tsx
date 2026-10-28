// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The rich note editor. It loads as a lazy chunk, only when a ticket's note is being edited and the note survives the
// editor (see `opensRich`). Tiptap's own style element is off because the page policy allows only same-origin
// stylesheets; the few rules the editor needs ship in the Floor's stylesheet.

import { Markdown } from '@tiptap/markdown';
import { EditorContent, useEditor } from '@tiptap/react';
import type { Editor } from '@tiptap/react';
import { useRef, useState } from 'react';

import { Button, cn } from '../../ui/index.ts';
import { noteExtensions, noteShape } from './noteRich.ts';
import { nextNote } from './noteText.ts';

export { opensRich } from './noteRich.ts';

const EDITOR = cn(
  'box-border min-h-24 max-h-80 w-full overflow-y-auto rounded-md border border-line-strong bg-field px-3 py-2 font-sans text-base',
  'text-ink [overflow-wrap:anywhere] hover:border-muted focus-visible:border-focus',
  'outline-0 focus-visible:outline-2 focus-visible:outline-solid focus-visible:outline-focus focus-visible:outline-offset-2',
  '[&_:where(p,ul,ol,blockquote,pre,h1,h2,h3,h4,h5,h6)]:my-2 [&>:first-child]:mt-0 [&>:last-child]:mb-0',
  '[&_:where(h1,h2,h3,h4,h5,h6)]:text-base [&_:where(h1,h2,h3,h4,h5,h6)]:font-semibold [&_:where(ul,ol)]:ps-5',
  '[&_blockquote]:ms-0 [&_blockquote]:border-s-[3px] [&_blockquote]:border-line-strong [&_blockquote]:ps-3 [&_blockquote]:text-muted',
  '[&_code]:rounded-sm [&_code]:bg-surface-2 [&_code]:px-1 [&_code]:font-mono [&_code]:text-small',
  '[&_pre]:overflow-x-auto [&_pre]:rounded-md [&_pre]:border [&_pre]:border-line [&_pre]:bg-surface-2 [&_pre]:p-3',
  '[&_pre_code]:bg-transparent [&_pre_code]:p-0 [&_a]:text-accent [&_a]:underline',
);

type Tool = { readonly label: string; readonly active: (editor: Editor) => boolean; readonly run: (editor: Editor) => void };

const TOOLS: readonly Tool[] = [
  { label: 'Bold', active: (e) => e.isActive('bold'), run: (e) => { e.chain().focus().toggleBold().run(); } },
  { label: 'Italic', active: (e) => e.isActive('italic'), run: (e) => { e.chain().focus().toggleItalic().run(); } },
  { label: 'Code', active: (e) => e.isActive('code'), run: (e) => { e.chain().focus().toggleCode().run(); } },
  { label: 'Heading', active: (e) => e.isActive('heading'), run: (e) => { e.chain().focus().toggleHeading({ level: 2 }).run(); } },
  { label: 'Bullets', active: (e) => e.isActive('bulletList'), run: (e) => { e.chain().focus().toggleBulletList().run(); } },
  { label: 'Numbers', active: (e) => e.isActive('orderedList'), run: (e) => { e.chain().focus().toggleOrderedList().run(); } },
  { label: 'Quote', active: (e) => e.isActive('blockquote'), run: (e) => { e.chain().focus().toggleBlockquote().run(); } },
  { label: 'Code block', active: (e) => e.isActive('codeBlock'), run: (e) => { e.chain().focus().toggleCodeBlock().run(); } },
];

/**
 * Edits `original`, a note that opens in the editor. `onNote` gets the original string for as long as the document is
 * unchanged, so saving a note nobody touched never rewrites it, and the editor's own Markdown once it is.
 */
export function RichNoteEditor({ id, original, onNote, onUnfit }: {
  readonly id: string | undefined;
  readonly original: string;
  readonly onNote: (value: string) => void;
  /** Called when the editor built a document that does not match the note, so the page can fall back to the text box. */
  readonly onUnfit: () => void;
}) {
  const baseline = useRef<string | null>(null);
  const [edited, setEdited] = useState(false);
  const [, redraw] = useState(0);
  const editor = useEditor({
    extensions: [...noteExtensions(), Markdown],
    content: original,
    contentType: 'markdown',
    injectCSS: false,
    editorProps: {
      attributes: { ...(id === undefined ? {} : { id }), role: 'textbox', 'aria-multiline': 'true', 'aria-label': 'Note', class: EDITOR },
    },
    onCreate: ({ editor: created }) => {
      baseline.current = created.getMarkdown().trim();
      if (noteShape(original) !== noteShape(baseline.current)) onUnfit();
    },
    onUpdate: ({ editor: changed }) => {
      const markdown = changed.getMarkdown().trim();
      const different = markdown !== baseline.current;
      setEdited(different);
      onNote(nextNote(original, baseline.current ?? '', markdown));
    },
    onSelectionUpdate: () => { redraw((count) => count + 1); },
  });
  if (editor === null) return null;
  return <div className="flex flex-col gap-2">
    <div role="toolbar" aria-label="Formatting" className="flex flex-wrap gap-1">
      {TOOLS.map((tool) => <Button key={tool.label} size="sm" variant={tool.active(editor) ? 'secondary' : 'ghost'}
        aria-pressed={tool.active(editor)} onClick={() => { tool.run(editor); }}>{tool.label}</Button>)}
    </div>
    <EditorContent editor={editor} />
    {edited ? <p className="m-0 text-small text-muted" role="note">The note is rewritten in standard Markdown when you save.</p> : null}
  </div>;
}

/** The editor as an element, so the page that loaded this chunk never names a component it fetched at run time. */
export function richEditor(props: Parameters<typeof RichNoteEditor>[0]) {
  return <RichNoteEditor {...props} />;
}
