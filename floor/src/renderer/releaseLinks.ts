// SPDX-License-Identifier: MIT OR Apache-2.0

/** The release notes page of one version; the version is the daemon's own, not session material. */
export function releaseNotesUrl(version: string): string {
  return `https://github.com/ArcfluxResearch/dudes/releases/tag/v${encodeURIComponent(version)}`;
}
