import { cn } from '../../ui/index.ts';
import { FIELD_BOX } from '../../ui/Field.tsx';

/** A line of guidance under a heading or beside a control. The hook class is what the tests and the page find it by. */
export const HINT = 'ticket-hint m-0 text-small text-muted';

/** The native select, drawn as the same box the other text controls use. */
export const SELECT_BOX = cn(FIELD_BOX, 'h-8 cursor-pointer');

/** The row a form's buttons sit in. */
export const FORM_ACTIONS = 'flex flex-wrap items-center gap-2';

/** The rhythm of the review sheet: each section is a direct child of the sheet, with a hairline above it. */
export const SHEET_SECTIONS = [
  '[&>section]:flex [&>section]:flex-col [&>section]:gap-2 [&>section]:border-t [&>section]:border-line [&>section]:py-4',
  '[&>section>h4]:m-0 [&>section>h4]:text-small [&>section>h4]:font-semibold [&>section>h4]:text-ink',
  '[&>section_h5]:m-0 [&>section_h5]:mt-2 [&>section_h5]:text-small [&>section_h5]:font-semibold [&>section_h5]:text-muted',
  '[&>section_p]:m-0 [&>section_ul]:m-0 [&>section_ul]:pl-5',
].join(' ');

/** A list of paths: one per row, the kind first. */
export const FILE_LIST = 'ticket-files m-0 flex list-none flex-col p-0';
export const FILE_ROW = 'flex items-baseline gap-3 py-1';
export const PATH = 'git-path data font-mono text-small wrap-anywhere';

/** The plain card sheet, for a board card that is not a ticket. */
export const CARD_DETAIL = 'card-detail flex flex-col gap-4 p-6 font-sans text-body text-ink wrap-anywhere max-[800px]:px-4 max-[800px]:pt-4';
export const CARD_HEADING = 'm-0 text-page font-semibold focus:outline-none';
export const CARD_FACTS = 'm-0 grid gap-3 [&_dd]:m-0 [&_dd]:mt-0.5 [&_dt]:text-caption [&_dt]:font-semibold [&_dt]:tracking-label [&_dt]:text-muted [&_dt]:uppercase';
