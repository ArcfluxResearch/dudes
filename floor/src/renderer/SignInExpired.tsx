// SPDX-License-Identifier: MIT OR Apache-2.0

/** Said when this browser has no sign-in the daemon will accept, and the way to get one. */
export function SignInExpired() {
  return (
    <p className="link-refused" role="alert">
      You are signed out of this Floor. That happens after 30 minutes without use, or when Dudes restarts. Run{' '}
      <code>dudes floor open</code> for a new link. Each link works once, in one browser.
    </p>
  );
}
