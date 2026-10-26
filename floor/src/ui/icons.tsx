// SPDX-License-Identifier: MIT OR Apache-2.0

import {
  Check, ChevronDown, CircleAlert, CircleCheck, Ellipsis, Info, LoaderCircle, Pencil, Search, Trash2, X,
} from 'lucide-react';
import type { LucideProps } from 'lucide-react';

/** Lucide at the Floor's default size and stroke, hidden from assistive technology unless it is given a label. */
function defaults(props: LucideProps): LucideProps {
  return { size: 16, strokeWidth: 1.75, 'aria-hidden': props['aria-label'] === undefined ? true : undefined, focusable: 'false', ...props };
}

export const CheckIcon = (props: LucideProps) => <Check {...defaults(props)} />;
export const ChevronDownIcon = (props: LucideProps) => <ChevronDown {...defaults(props)} />;
export const AlertIcon = (props: LucideProps) => <CircleAlert {...defaults(props)} />;
export const SuccessIcon = (props: LucideProps) => <CircleCheck {...defaults(props)} />;
export const MoreIcon = (props: LucideProps) => <Ellipsis {...defaults(props)} />;
export const InfoIcon = (props: LucideProps) => <Info {...defaults(props)} />;
export const SpinnerIcon = (props: LucideProps) => <LoaderCircle {...defaults(props)} />;
export const EditIcon = (props: LucideProps) => <Pencil {...defaults(props)} />;
export const SearchIcon = (props: LucideProps) => <Search {...defaults(props)} />;
export const TrashIcon = (props: LucideProps) => <Trash2 {...defaults(props)} />;
export const CloseIcon = (props: LucideProps) => <X {...defaults(props)} />;
