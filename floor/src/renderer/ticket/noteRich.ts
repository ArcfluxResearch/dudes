// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What the rich note editor may produce, and how the page decides whether a note can open in it. This module and the
// editor component are the only importers of Tiptap and `marked`, and both load as one lazy chunk.
//
// The editor is configured to write only what the closed Markdown renderer shows: headings, bold, italic, inline code,
// code blocks, quotes, bullet and numbered lists, links and line breaks. Strike, underline and horizontal rules are off,
// there are no task lists, tables or images, and Link is the one registered by the starter kit.

import { MarkdownManager } from '@tiptap/markdown';
import { Paragraph } from '@tiptap/extension-paragraph';
import StarterKit from '@tiptap/starter-kit';
import type { AnyExtension, JSONContent } from '@tiptap/core';
import { Lexer } from 'marked';
import { webAddress } from '../chat/markdown.tsx';
import type { Token, Tokens } from 'marked';

// The stock paragraph writes `&nbsp;` for an empty paragraph, which the renderer would show as literal text.
const NoteParagraph = Paragraph.extend({
  renderMarkdown: (node, helpers) => Array.isArray(node.content) && node.content.length > 0
    ? helpers.renderChildren(node.content) : '',
});

// The stock text encoding turns `&`, `<` and `>` into entities, so typing "Q&A" would save `Q&amp;A`, which the note
// renderer shows literally. A note is plain Markdown: text is backslash-escaped only where Markdown needs it, and `<` only
// where it would start an HTML tag. The Markdown extension builds its own manager, so the method is replaced on the class.
type Encoder = {
  codeTypes: Set<string>;
  escapeMarkdownSyntax(text: string): string;
  encodeTextForMarkdown(text: string, node: { marks?: readonly (string | { type: string })[] }, parent?: { type?: string }): string;
};
(MarkdownManager.prototype as unknown as Encoder).encodeTextForMarkdown = function encodeText(this: Encoder, text, node, parent) {
  const inCode = (parent?.type !== undefined && this.codeTypes.has(parent.type))
    || (node.marks ?? []).some((mark) => this.codeTypes.has(typeof mark === 'string' ? mark : mark.type));
  return inCode ? text : this.escapeMarkdownSyntax(text).replace(/<(?=[A-Za-z/!?])/g, '\\<');
};

export function noteExtensions(): AnyExtension[] {
  return [
    StarterKit.configure({
      paragraph: false,
      strike: false,
      underline: false,
      horizontalRule: false,
      dropcursor: false,
      gapcursor: false,
      trailingNode: false,
      link: { openOnClick: false, autolink: true, linkOnPaste: false, defaultProtocol: 'https', isAllowedUri: webAddress },
    }),
    NoteParagraph,
  ];
}

let manager: MarkdownManager | null = null;
function markdown(): MarkdownManager {
  manager ??= new MarkdownManager({ extensions: noteExtensions() });
  return manager;
}

/** A document written as the editor writes it. */
export function serialize(doc: JSONContent): string {
  return markdown().serialize(doc).trim();
}

/** The note as the editor would write it, without opening an editor. */
export function reserialize(text: string): string {
  const manager = markdown();
  return serialize(manager.parse(text) as JSONContent);
}

type Shape = string | { readonly [key: string]: unknown };

function inlineShape(tokens: readonly Token[] | undefined): Shape[] {
  const out: Shape[] = [];
  const push = (text: string) => {
    const last = out.at(-1);
    if (typeof last === 'string') out[out.length - 1] = last + text;
    else out.push(text);
  };
  for (const token of tokens ?? []) {
    switch (token.type) {
      case 'text': case 'escape': push((token as Tokens.Text).text); break;
      case 'br': push('\n'); break;
      case 'strong': case 'em':
        out.push({ t: token.type, c: inlineShape((token as Tokens.Strong).tokens) }); break;
      case 'codespan': out.push({ t: 'code', s: (token as Tokens.Codespan).text }); break;
      case 'link':
        out.push({ t: 'link', href: (token as Tokens.Link).href, c: inlineShape((token as Tokens.Link).tokens) }); break;
      default: out.push({ t: token.type, s: token.raw });
    }
  }
  const first = out[0];
  if (typeof first === 'string') out[0] = first.trimStart();
  const last = out.at(-1);
  if (typeof last === 'string') out[out.length - 1] = last.trimEnd();
  return out.filter((part) => part !== '');
}

function blockShape(tokens: readonly Token[]): Shape[] {
  const out: Shape[] = [];
  for (const token of tokens) {
    switch (token.type) {
      case 'space': break;
      case 'heading':
        out.push({ t: 'heading', depth: (token as Tokens.Heading).depth, c: inlineShape((token as Tokens.Heading).tokens) }); break;
      case 'paragraph': case 'text':
        out.push({ t: 'p', c: inlineShape((token as Tokens.Paragraph).tokens ?? [{ type: 'text', raw: token.raw, text: (token as Tokens.Text).text }]) });
        break;
      case 'code':
        out.push({ t: 'block', lang: (token as Tokens.Code).lang ?? '', s: (token as Tokens.Code).text }); break;
      case 'blockquote': out.push({ t: 'quote', c: blockShape((token as Tokens.Blockquote).tokens) }); break;
      case 'list': {
        const list = token as Tokens.List;
        out.push({
          t: 'list', ordered: list.ordered, start: list.ordered ? list.start : null,
          c: list.items.map((item) => ({ task: item.task, c: blockShape(item.tokens) })),
        });
        break;
      }
      default: out.push({ t: token.type, s: token.raw });
    }
  }
  return out;
}

/** The document's structure with markers, blank lines and wrapping left out, so two spellings of one note compare equal. */
export function noteShape(text: string): string {
  return JSON.stringify(blockShape(new Lexer().lex(text)));
}

/**
 * Whether a note survives the editor without a change of meaning: lexed, the note and the editor's version of it have the
 * same tokens once bullet markers, emphasis markers and blank-line counts are set aside. A table, an image, raw HTML or a
 * task list is dropped or rewritten by the editor, so such a note opens as text instead.
 */
export function opensRich(text: string): boolean {
  try {
    return noteShape(text) === noteShape(reserialize(text));
  } catch {
    return false;
  }
}
