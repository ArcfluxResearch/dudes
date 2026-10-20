// SPDX-License-Identifier: MIT OR Apache-2.0

/** Said when this browser has no sign-in the daemon will accept, and the way to get one. */
export function SignInExpired() {
  return (
    <p className="link-refused" role="alert">
      You are signed out of this Floor. That happens after a day without use, after 30 days, when you sign out, or
      when Dudes restarts and cannot keep sign-ins. Open Dudes to sign in again.
    </p>
  );
}
