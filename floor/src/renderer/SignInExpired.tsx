// SPDX-License-Identifier: MIT OR Apache-2.0

import { signedOutReason } from '../session.ts';
import type { SignedOutReason } from '../session.ts';
import { copyToClipboard } from './setup/SetupView.tsx';

/**
 * The one command that gets a person back in when the page holds no session. It is the one place
 * the page sends someone to a terminal on purpose: a page with no session has nothing left to ask
 * the daemon with. Every message that needs it draws this component, so the command is written once.
 */
const OPEN = 'dudes open';

export function OpenCommand() {
  return <span className="open-command"><code>{OPEN}</code>
    <button type="button" className="open-command-copy" onClick={() => copyToClipboard(OPEN)}>Copy</button></span>;
}

const WHY: Readonly<Record<SignedOutReason, string>> = {
  signed_out: 'You signed out of this Floor in this browser.',
  removed: 'This browser was removed from another browser or from the terminal.',
  expired: 'This browser was signed out after 30 days without use, or 90 days in all.',
  replaced: 'A newer sign-in took this browser\'s place, because a Floor keeps at most eight signed-in browsers.',
};

/** Said when this browser has no sign-in the daemon will accept, why, and the way to get one. */
export function SignInExpired({ why }: { readonly why?: string }) {
  const reason = signedOutReason();
  return (
    <p className="link-refused" role="alert">
      {why !== undefined ? why : reason === null
        ? 'You are signed out of this Floor. That happens when you sign out, when the browser is removed, or after 30 days without use or 90 days in all.'
        : WHY[reason]}
      {' '}To sign in again, run <OpenCommand /> in a terminal.
    </p>
  );
}
