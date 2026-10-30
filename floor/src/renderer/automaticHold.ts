// SPDX-License-Identifier: MIT OR Apache-2.0

export type AutomaticHold =
  | { readonly kind: 'repeated_fast_failure'; readonly first: string; readonly latest: string }
  | { readonly kind: 'launch_refused' | 'launch_failed'; readonly reason: string }
  | { readonly kind: 'worktree_missing'; readonly worktree: string };

export function automaticHoldOf(row: Readonly<Record<string, unknown>>): AutomaticHold | null {
  if (row.mode === 'attached' || row.paused === true || row.halted === true) return null;
  const value = row.automatic_hold;
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return null;
  const hold = value as Record<string, unknown>;
  if (hold.kind === 'repeated_fast_failure' && Object.keys(hold).length === 3
    && typeof hold.first === 'string' && /^[0-7][0-9A-HJKMNP-TV-Z]{25}$/.test(hold.first)
    && typeof hold.latest === 'string' && /^[0-7][0-9A-HJKMNP-TV-Z]{25}$/.test(hold.latest)) {
    return { kind: hold.kind, first: hold.first, latest: hold.latest };
  }
  if (hold.kind === 'worktree_missing' && Object.keys(hold).length === 2
    && typeof hold.worktree === 'string' && /^[0-9a-f]{32}$/.test(hold.worktree)) {
    return { kind: hold.kind, worktree: hold.worktree };
  }
  if ((hold.kind === 'launch_refused' || hold.kind === 'launch_failed') && Object.keys(hold).length === 2
    && typeof hold.reason === 'string' && hold.reason.length > 0 && hold.reason.length <= 2048
    && !/\p{Cc}/u.test(hold.reason)) return { kind: hold.kind, reason: hold.reason };
  return null;
}

/** The program each hireable engine runs. */
const ENGINES: Readonly<Record<string, { readonly name: string; readonly program: string }>> = Object.freeze({
  claude: { name: 'Claude Code', program: 'claude' },
  codex: { name: 'Codex', program: 'codex' },
  pi: { name: 'Pi', program: 'pi' },
});

function engineOf(provider: unknown) {
  return typeof provider === 'string' && Object.hasOwn(ENGINES, provider) ? ENGINES[provider] : undefined;
}

function capitalized(reason: string): string {
  return reason.charAt(0).toUpperCase() + reason.slice(1);
}

function sentence(reason: string): string {
  const text = capitalized(reason);
  return text.endsWith('.') ? text : `${text}.`;
}

/** The recorded reason ends in the error kind and names no engine, and other not-found errors share it, so the program is only the usual cause. */
function missingEngine(hold: AutomaticHold, provider: unknown) {
  return hold.kind === 'launch_failed' && hold.reason.endsWith('(NotFound)') ? engineOf(provider) : undefined;
}

function missingEngineHint(hold: AutomaticHold, provider: unknown): string {
  const engine = missingEngine(hold, provider);
  if (engine === undefined) return '';
  return `. Dudes could not start ${engine.name}, usually because ${engine.program} is not installed. `
    + 'Install it and sign in, then resume this agent, or hire an agent with an engine you have.';
}

/** The full account, with the recorded reason and the hint, for Overview and the crew row. */
export function automaticHoldText(hold: AutomaticHold, provider?: unknown): string {
  switch (hold.kind) {
    case 'repeated_fast_failure': return 'Repeated fast failures. Check the CLI before resuming.';
    case 'launch_refused': return `Launch refused. ${capitalized(hold.reason)}`;
    case 'launch_failed': {
      const hint = missingEngineHint(hold, provider);
      return `Launch failed. ${hint === '' ? capitalized(hold.reason) : capitalized(hold.reason.replace(/\.$/, ''))}${hint}`;
    }
    case 'worktree_missing': return "This agent's worktree is missing. Restore it or retire the agent, then resume.";
  }
}

/** The short account for the header, which sits right above the Resume button. */
export function automaticHoldSummary(hold: AutomaticHold, provider?: unknown): string {
  switch (hold.kind) {
    case 'launch_refused': return `Launch refused. ${sentence(hold.reason)}`;
    case 'launch_failed': {
      const engine = missingEngine(hold, provider);
      return engine === undefined ? `Launch failed. ${sentence(hold.reason)} Press Resume to try again.`
        : `${engine.name} did not start, usually because ${engine.program} is not installed. Install it and sign in, then press Resume.`;
    }
    default: return automaticHoldText(hold, provider);
  }
}
