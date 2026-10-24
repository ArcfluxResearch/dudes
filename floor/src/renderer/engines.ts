// SPDX-License-Identifier: MIT OR Apache-2.0

import { HIRE_ENGINES } from './onboarding/catalog.ts';

const ENGINE_NAMES: Readonly<Record<string, string>> = Object.freeze({
  ...HIRE_ENGINES,
  copilot: 'GitHub Copilot', antigravity: 'Antigravity', grok: 'Grok', kimi: 'Kimi',
  qwen: 'Qwen Code', opencode: 'OpenCode', crush: 'Crush',
});

export function engineName(engine: string): string {
  return Object.hasOwn(ENGINE_NAMES, engine) ? ENGINE_NAMES[engine]! : engine;
}
