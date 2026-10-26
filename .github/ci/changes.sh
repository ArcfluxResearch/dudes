#!/bin/sh
# Decides which CI areas a pull request needs. Sets the step outputs `rust`, `floor` and `any`, and
# writes the decision and the changed files to the job summary.
#
# Every event except a pull request runs everything, and so does any failure to learn the changed
# files: a wrong answer must cost time, never skip a check.
#
# Inputs (environment): EVENT_NAME, BASE_SHA, HEAD_SHA. A changed-files list can be given as the
# first argument for a local run: `.github/ci/changes.sh changed.txt`.
set -u

here=$(dirname "$0")
outputs=${GITHUB_OUTPUT:-/dev/stdout}
summary=${GITHUB_STEP_SUMMARY:-/dev/stderr}

emit() {
	{
		echo "rust=$1"
		echo "floor=$2"
		echo "any=$3"
	} >>"$outputs"
}

everything() {
	emit true true true
	echo "### CI areas: rust=true floor=true" >>"$summary"
	echo "$1" >>"$summary"
	exit 0
}

# True when path $1 matches a glob line of file $2. Blank lines and `#` lines are skipped.
listed() {
	while IFS= read -r pattern; do
		case $pattern in '' | '#'*) continue ;; esac
		# shellcheck disable=SC2254
		case $1 in $pattern) return 0 ;; esac
	done <"$2"
	return 1
}

if [ -n "${1:-}" ]; then
	changed=$1
else
	[ "${EVENT_NAME:-}" = pull_request ] || everything "Not a pull request: every job runs."
	changed=${RUNNER_TEMP:-/tmp}/changed-files.txt
	git diff --name-only --no-renames "${BASE_SHA:?}...${HEAD_SHA:?}" >"$changed" ||
		everything "Could not list the changed files: every job runs."
fi

[ -s "$changed" ] || everything "No changed files found: every job runs."

rust=false
floor=false
while IFS= read -r path; do
	case $path in
	.github/*)
		rust=true
		floor=true
		;;
	floor/*)
		floor=true
		listed "$path" "$here/floor-files-rust-reads.txt" && rust=true
		;;
	*)
		rust=true
		listed "$path" "$here/rust-files-floor-reads.txt" && floor=true
		;;
	esac
done <"$changed"

emit "$rust" "$floor" true
{
	echo "### CI areas: rust=$rust floor=$floor"
	echo
	echo "Changed files:"
	echo
	sed 's/^/- `/; s/$/`/' "$changed" | head -n 200
	echo
	echo "$(wc -l <"$changed" | tr -d ' ') files in all."
} >>"$summary"
