// SPDX-License-Identifier: MIT OR Apache-2.0

interface NavigatorLike {
  readonly userAgent?: string;
  readonly userAgentData?: {
    readonly brands?: readonly { readonly brand: string }[];
    readonly platform?: string;
  };
}

const BRANDS: readonly (readonly [string, string])[] = [
  ['Microsoft Edge', 'Edge'], ['Opera', 'Opera'], ['Brave', 'Brave'], ['Google Chrome', 'Chrome'], ['Chromium', 'Chromium'],
];

const BROWSERS: readonly (readonly [RegExp, string])[] = [
  [/Edg(?:e|A|iOS)?\//, 'Edge'], [/OPR\/|Opera/, 'Opera'], [/Firefox\/|FxiOS\//, 'Firefox'],
  [/Chrome\/|CriOS\//, 'Chrome'], [/Safari\//, 'Safari'],
];

const SYSTEMS: readonly (readonly [RegExp, string])[] = [
  [/iPhone/, 'iPhone'], [/iPad/, 'iPad'], [/Android/, 'Android'], [/CrOS/, 'ChromeOS'],
  [/Windows/, 'Windows'], [/Mac OS X|Macintosh/, 'macOS'], [/Linux|X11/, 'Linux'],
];

function first(table: readonly (readonly [RegExp, string])[], agent: string): string | null {
  return table.find(([pattern]) => pattern.test(agent))?.[1] ?? null;
}

/**
 * What the page calls this browser when it signs in, such as "Chrome on macOS". It is a label for
 * the person's list, not proof of anything, and it carries no secret.
 */
export function browserName(
  navigator: NavigatorLike | undefined = (globalThis as { navigator?: NavigatorLike }).navigator,
): string {
  if (navigator === undefined) return 'Browser';
  const agent = navigator.userAgent ?? '';
  const data = navigator.userAgentData;
  const brands = data?.brands?.map((entry) => entry.brand) ?? [];
  const app = BRANDS.find(([brand]) => brands.includes(brand))?.[1] ?? first(BROWSERS, agent);
  const system = (data?.platform ? data.platform : null) ?? first(SYSTEMS, agent);
  if (app === null) return 'Browser';
  return system === null || system === '' ? app : `${app} on ${system}`;
}
