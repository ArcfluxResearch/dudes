// SPDX-License-Identifier: MIT OR Apache-2.0

const RELEASES = 'https://github.com/ArcfluxResearch/dudes/releases';

/** Only a plain `x.y.z` version has a release page; a dev or pre-release build has no tag. */
const isReleaseVersion = (version: string): boolean => /^\d+\.\d+\.\d+$/.test(version);

/** The release notes page of one version, or the list of releases when that version has no tag. */
export function releaseNotesUrl(version: string): string {
  return isReleaseVersion(version) ? `${RELEASES}/tag/v${version}` : RELEASES;
}

export function releaseNotesLabel(version: string): string {
  return isReleaseVersion(version) ? `Release notes for ${version}` : 'Release notes';
}
