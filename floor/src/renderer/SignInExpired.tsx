// SPDX-License-Identifier: MIT OR Apache-2.0

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

/** Said when this browser has no sign-in the daemon will accept, and the way to get one. */
export function SignInExpired() {
  return (
    <p className="link-refused" role="alert">
      You are signed out of this Floor. That happens after a day without use, after 30 days, when you sign out, or
      when Dudes restarts and cannot keep sign-ins. To sign in again, run <OpenCommand /> in a terminal.
    </p>
  );
}
