#!/bin/sh
# Prior external effects remain unknown even when the replacement uses fresh context.
set -u

. "$(dirname "$0")/lib.sh"

if [ "$#" -ne 0 ]; then
	echo "usage: $0" >&2
	exit 64
fi

MODE=after-spawn
TITLE="the post-launch arm: explicit consent after the old child is gone"

require_binaries
require_recovery_tools
open_sandbox

# --- the daemon this arm drives -------------------------------------------------------------------
#
# The same feature-built binary `crash.sh` uses, armed at the other window. This script uses its
# after-launch wrapper to park the loop and touch a file to say it has. **It kills nothing.**
# The SIGKILL below is a real external signal, sent by
# this script exactly as a person would send it.
#
# It builds into a target directory of its own, so `target/release/duded` — what arm 1 runs — is
# never replaced by a binary with a fault in it.
FAULT_TARGET="$TARGET_DIR/two-roles-evidence/fault"
DUDED="$FAULT_TARGET/release/duded"

printf '%s\n' "$TITLE"
printf '  cli       %s\n' "$DUDES_CLI"
printf '  sandbox   %s\n' "$SANDBOX"
printf '  building  duded --features fault-injection into %s\n' "$FAULT_TARGET"
(cd "$REPO_ROOT" && CARGO_TARGET_DIR="$FAULT_TARGET" cargo build --quiet --release -p duded \
	--features fault-injection) || fatal "could not build the fault-injection daemon"
printf '  daemon    %s\n' "$DUDED"
[ -x "$DUDED" ] || fatal "no $DUDED after the build"

# Which of the stand-in's runs does the goal's work. `2` is the arm, and both runs are load-bearing:
#
#   run 1  the crashed turn's child. Must **not** do the work: a goal closed by the child of a turn
#          that was then re-issued would leave the re-issue nothing to be observed doing.
#   run 2  the re-issued turn's child, on a session of its own. Does the work, so the goal closes.
#
# Two runs and no more, because *"exactly two transcripts"* is a statement about this agent's whole
# run: one more turn would be one more file and the count would stop meaning what it says. What
# that costs is one shared check — see where `assert_a_predicate_came_out_false` is **not** called.
MARK_ON_RUN=${MARK_ON_RUN:-2}
STALL_MARKER="$SANDBOX/stalled"
# Which launch the fault stops after. `1` is the arm — the agent's first turn, the one with no
# vendor-confirmed session behind it. Reachable from outside so a reader can check the assertions
# guarding it are load-bearing: `STALL_NTH=9` is a launch this run never reaches, and the arm fails
# with "the daemon never reached the window".
STALL_NTH=${STALL_NTH:-1}

# The names this arm puts into a search of the journal, written once.
#
# Two of its checks below are absences — no `turn_spawned` naming the crashed child, no
# `session_learned` at all — and an absence check has one failure no care in the surrounding logic
# prevents: a name spelled the way nothing writes it matches nothing, whatever the journal holds,
# and reports the absence it went looking for. A control written beside such a search with the name
# spelled a second time controls nothing, because the two spellings can differ. So each name lives
# in one variable, the searches and the controls at the end of the script both read it, and a
# misspelling moves them together — the control then goes red and this arm exits non-zero.
KIND_TURN_SPAWNED=turn_spawned
KIND_SESSION_LEARNED=session_learned
FIELD_PID=pid

start_daemon "$MODE" \
	DUDES_FAULT_STALL_AFTER_SPAWN="$STALL_NTH" \
	DUDES_FAULT_STALL_MARKER="$STALL_MARKER"

header "the daemon is up and answering the operator"
if dudes_ping; then
	pass "system.ping answered"
else
	fail "the daemon did not answer system.ping"
fi

hire_two
set_one_goal_detached

# --- the window: a child is running and no record names it ----------------------------------------

header "the launch happened: a child is alive"
# Both waits, and in this order, because they are two different facts: the marker says the host
# answered and the loop stopped, and the stand-in's own file says the process it answered about
# reached its first line. Either alone would leave the other racing.
if await "the stand-in's first run to start" 60 file_exists "$WORK_DIR/started.1" &&
	await "the daemon to reach the post-launch window" 60 file_exists "$STALL_MARKER"; then
	assert_one_child_is_the_agents 1
else
	fail "the daemon never reached the window with a child in it; nothing to kill"
	finish "$TITLE"
fi
orphan=$(standin_pid 1)

header "SIGKILL"
# Nothing is asked of the daemon between the window and the signal, and nothing can be: `dudes tail`
# is answered by the same parked loop. Everything below the kill is read from the journal offline.
kill_daemon_hard
pass "the daemon is gone; it ran no exit path and reaped nothing"
await_detached_goal_ends

at_kill="$GATE_OUT/$MODE.at-kill.journal"
dump_journal >"$at_kill" || fatal "could not read the journal the crash left behind"
note "the journal as the crash left it: $at_kill"
# The state directory as the crash left it, taken while nothing holds it. It is a second real
# projection, and it is what makes the export comparison at the end of this script non-vacuous.
cp -R "$STATE_DIR" "$SANDBOX/at-kill-state" || fatal "could not copy the state the crash left"

header "the goal is durable, though nothing ever answered the call that set it"
goals=$(records_of "$at_kill" goal_set | grep -c . || true)
if [ "$goals" -eq 1 ]; then
	pass "one goal_set in the journal: the record was fsynced before the effect that parked the loop"
else
	fail "expected one goal_set in the journal the crash left, found $goals"
fi

header "the crash left a durable TurnIssued with no Spawn (invariant 2)"
unspawned=$(issues_without_spawn "$at_kill")
count=$(printf '%s' "$unspawned" | grep -c . || true)
if [ "$count" -eq 1 ]; then
	pass "exactly one issued turn has no spawn: $unspawned"
else
	fail "expected exactly one unspawned turn, found $count"
	note "${unspawned:-none}"
fi

header "and this arm's difference from arm 2: that turn has a child, which outlived the daemon"
if kill -0 "$orphan" 2>/dev/null; then
	pass "the process table still reports the old child pid $orphan"
else
	fail "the child did not survive the kill: this arm is then arm 2's window under another name"
fi

header "no record names that child"
# The pid and the session it ran on are written by `TurnSpawned`, which is
# the record the crash prevented. So the session it is writing is *unnameable* — which is why the
# transcript checks below identify it by pid from the file system and never from the journal.
#
# Two names go into this search, the kind and the field, and either can be misspelled on its own.
# Both are read from the variables above; both are checked against the contract that defines them,
# at the end of the script.
named=$(records_of "$at_kill" "$KIND_TURN_SPAWNED" | grep -o "\"$FIELD_PID\":[0-9]*" | cut -d: -f2 |
	grep -cxF "$orphan" || true)
if [ "$named" -eq 0 ]; then
	pass "no $KIND_TURN_SPAWNED carries $FIELD_PID $orphan"
else
	fail "$named record(s) name $FIELD_PID $orphan, so the kill did not land in this window"
fi

header "which branch this arm is in: nothing has confirmed the session that turn announced"
# This records the branch, it does not choose between two the daemon could take. The turn about to
# be re-issued has no vendor-confirmed session behind it — the branch
# that mints — and everything below is a claim about that branch and no other.
#
# What it is **not** is a filter that could send this run down the other one. The watcher can post
# `Input::SessionLearned`, but the loop that would persist it is parked inside the host's `spawn`
# call until after this script kills the daemon. The count is therefore zero in this fault window
# even though ordinary managed turns now confirm their opening line. Read the line it prints as
# "this is where the arm stands", and read the assertion as a guard on the crash ordering.
#
# The control on the spelling — that `session_learned` is what the contract calls this record — is
# at the end of the script with the other controls.
learned=$(records_of "$at_kill" "$KIND_SESSION_LEARNED" | grep -c . || true)
if [ "$learned" -eq 0 ]; then
	pass "no $KIND_SESSION_LEARNED before the kill: the announced id is unconfirmed"
else
	fail "$learned session(s) were already confirmed; that is the other branch, and not this arm's"
fi

# --- the restart, and the second child ------------------------------------------------------------

header "the daemon restarts on the state the crash left"
# The fault is not armed on the restart: what is under test now is replay, and a second stall would
# only stop the thing being measured.
start_daemon "$MODE-restart"
pass "restarted on $STATE_DIR"

header "restart keeps the missing-Spawn barrier while the old child remains alive"
inspect_missing_spawn "$at_kill" 2
if kill -0 "$orphan" 2>/dev/null; then
	pass "the old child survived the first restart; no replacement was launched"
else
	fail "the old child ended before the no-repeat observation"
fi

header "release the old child and observe its absence before consenting"
release_standin 1
if await "the old child to disappear from the process table" 60 process_gone "$orphan"; then
	pass "the OS no longer reports pid $orphan; this shell did not waitpid that orphan"
else
	fatal "the old child remains present; no repeat consent will be sent"
fi
submit_fresh_repeat

header "the consented same-turn repeat has one held child"
if await_standin 2; then
	assert_one_child_is_the_agents 2
else
	fail "no replacement child followed the recorded consent"
	finish "$TITLE"
fi
reissued_child=$(standin_pid 2)
release_standin 2

header "the goal closes"
if await "the goal to reach Done" 60 journal_shows '"kind":"done"'; then
	pass "the goal reached Done"
else
	fail "the goal never reached Done"
fi

save_final_recovery_receipt
stop_daemon

# --- what the journal, the projections and the transcripts say, read offline ----------------------

dump="$GATE_OUT/$MODE.journal"
store_export="$GATE_OUT/$MODE.export.store"
replay_export="$GATE_OUT/$MODE.export.replay"
dump_journal >"$dump" || fatal "could not read the journal back"
dump_export store >"$store_export" || fatal "could not export the projection store"
dump_export journal >"$replay_export" || fatal "could not replay the journal into a projection"
note "journal kept at $dump"
note "projection exports kept at $store_export and $replay_export"

# The transcripts are this arm's own evidence and they live in a sandbox that goes away with the
# script, so they are copied out beside the journals. Only files this script's own runs write, and
# only by extension, so a reader is never shown a leftover from an earlier attempt.
kept="$GATE_OUT/$MODE.transcripts"
mkdir -p "$kept"
rm -f "$kept"/*.jsonl
cp "$WORK_DIR"/transcripts/*.jsonl "$kept"/ || fatal "no transcripts to keep"
note "session transcripts kept at $kept"

assert_done_is_observed "$dump" "$store_export"
# `assert_a_predicate_came_out_false` is **not** called here, and the omission is deliberate rather
# than an oversight. It needs a run in which a turn ends without meeting the goal, and this run has
# exactly one turn-end in it by construction: its two children are two attempts at *one* turn, and
# the second does the work. Arm 1 and arm 2 make that check, over the same daemon, unweakened. What
# it would take to make it here is a second turn, and a second turn is a third session transcript —
# which would cost this arm the only thing it exists to count.
assert_one_spawn_per_issued_turn "$dump"
assert_not_exhausted_by_hops "$dump"
assert_recovery_repeat "$at_kill" "$dump" "$store_export"

# --- what the re-issued turn announced ----------------------------------------

header "exactly one durable Spawn follows the explicit repeat"
reissued=$(records_of "$dump" "$KIND_TURN_SPAWNED" | grep -cF '"reissued":true' || true)
if [ "$reissued" -eq 1 ]; then
	pass "one consented Spawn is marked reissued"
else
	fail "expected one consented reissued Spawn, found $reissued"
	finish "$TITLE"
fi

# The turn that spawn belongs to, and then the two sessions the journal holds for it: the one it was
# *issued* with, and the one it was *launched* on. Read from the two records rather than from the
# turn row, so what is compared is what each record says at the moment it was written.
crashed_turn=$(records_of "$dump" "$KIND_TURN_SPAWNED" | grep -F '"reissued":true' |
	grep -o '"turn":"[^"]*"' | head -1 | cut -d'"' -f4)
announced=$(records_of "$dump" turn_issued | grep -F "\"turn\":\"$crashed_turn\"" |
	grep -o '"session":"[^"]*"' | head -1 | cut -d'"' -f4)
launched=$(records_of "$dump" "$KIND_TURN_SPAWNED" | grep -F "\"turn\":\"$crashed_turn\"" |
	grep -o '"session":"[^"]*"' | head -1 | cut -d'"' -f4)
spawned_pid=$(records_of "$dump" "$KIND_TURN_SPAWNED" | grep -F "\"turn\":\"$crashed_turn\"" |
	grep -o "\"$FIELD_PID\":[0-9]*" | head -1 | cut -d: -f2)

header "the re-issued turn ran on a different session from the one it was issued with"
if [ -n "$announced" ] && [ -n "$launched" ] && [ "$announced" != "$launched" ]; then
	pass "turn $crashed_turn: issued on $announced, launched on $launched"
else
	fail "issued on ${announced:-none}, launched on ${launched:-none} — a re-issue that resumed"
fi

header "and the child the journal names for it is the one this script watched start"
# This is also the firing half of the pid absence above: the same reader and the same
# `$FIELD_PID` pattern that found nothing in the journal the crash left do return a pid from the
# finished one, and it has to be the pid this script watched start. A reader that looked for the
# wrong field would find nothing here either, and this line would be red.
if [ -n "$spawned_pid" ] && [ "$spawned_pid" = "$reissued_child" ]; then
	pass "$KIND_TURN_SPAWNED names $FIELD_PID $spawned_pid"
else
	fail "$KIND_TURN_SPAWNED names $FIELD_PID ${spawned_pid:-none}; run 2 was $reissued_child"
fi


sessions=$(transcript_sessions)
present=$(printf '%s\n' "$sessions" | grep -c . || true)

header "exactly two session transcripts exist for the agent that took the turns"
if [ "$present" -eq 2 ]; then
	pass "two transcripts under $kept"
else
	fail "expected two session transcripts, found $present"
	note "$(printf '%s' "$sessions" | tr '\n' ' ')"
fi

header "the two files match the original and consented fresh session identities"
want=$(printf '%s\n%s\n' "$announced" "$launched" | sort)
if [ "$sessions" = "$want" ]; then
	pass "$(printf '%s' "$want" | tr '\n' ' ')"
else
	fail "the transcripts on disk are not the sessions the journal names"
	note "on disk: $(printf '%s' "$sessions" | tr '\n' ' ')"
	note "journal: $(printf '%s' "$want" | tr '\n' ' ')"
fi

header "each child wrote into its own session, and into no other"
# Two propositions per child, and the first is what keeps the second honest. That a pid **is** found
# where it should be is the positive half: a reader looking in the wrong directory, or for the wrong
# field, finds nothing anywhere and would make every absence below come out true. That it is found
# nowhere else is the claim.
#
# What this pair cannot see, said plainly so nobody mistakes it for the whole check: when the two
# ids are the *same* id — a re-issue that resumed — "its own session" is one file for both children
# and both lines pass. The assertion that fires on that is the next one, and the two are one
# mechanism.
for pair in "$orphan:$announced" "$reissued_child:$launched"; do
	pid=${pair%%:*}
	want=${pair#*:}
	found=""
	for session in $sessions; do
		if pids_in_transcript "$session" | grep -qxF "$pid"; then
			found="$found $session"
		fi
	done
	if [ "$found" = " $want" ]; then
		pass "pid $pid wrote into $want and nowhere else"
	else
		fail "pid $pid wrote into:${found:- nothing}; expected $want alone"
	fi
done

header "each physical attempt left its own session transcript"
# The claim this arm exists for, stated plainly: over *these two*
# children, which are the two attempts at one turn. It follows from the pair above and is asserted
# anyway, because it is the sentence a reader came here to see and because it is what a future
# change would break first.
mixed=""
for session in $sessions; do
	pids=$(pids_in_transcript "$session")
	if printf '%s\n' "$pids" | grep -qxF "$orphan" &&
		printf '%s\n' "$pids" | grep -qxF "$reissued_child"; then
		mixed="$mixed $session"
	fi
done
if [ -z "$mixed" ]; then
	pass "pid $orphan and pid $reissued_child never appear in one session file"
else
	fail "both children wrote into:$mixed"
fi
for session in $sessions; do
	note "$session: $(pids_in_transcript "$session" | tr '\n' ' ')"
done

header "the control for both absences: those searches used the contract's own spellings"
# The two absences above are worth exactly what the spelling of the names they searched for is
# worth, and there are three such names: two record kinds and one field. Each is checked here
# against `schemas/journal-record.schema.json`, the generated record contract — and each is read
# from the same variable the search read, which is the half that makes this a control on the search
# rather than a fact about a schema. Spell one of them wrong and both the search and the check
# below move to the new spelling; the search then matches nothing and reports its absence, and this
# is the line that goes red and takes the run's exit status with it.
#
# `schema_record_carries` matches the kind on its own `"t"` const and the field inside that same
# record, so "turn_spawned carries pid" is a statement about that record and not about the file.
if schema_record_carries "$KIND_TURN_SPAWNED" "$FIELD_PID"; then
	pass "the contract defines $KIND_TURN_SPAWNED, carrying a $FIELD_PID field"
else
	fail "the contract defines no $KIND_TURN_SPAWNED carrying $FIELD_PID: that search cannot match"
	note "contract: $RECORD_CONTRACT"
fi
if schema_record_carries "$KIND_SESSION_LEARNED"; then
	pass "the contract defines $KIND_SESSION_LEARNED"
else
	fail "the contract defines no $KIND_SESSION_LEARNED record: that search cannot match"
	note "contract: $RECORD_CONTRACT"
fi

header "and the reader that returned it does return records from this journal"
kinds=$(awk '{ print $2 }' "$at_kill" | sort -u | tr '\n' ' ')
if [ -n "$kinds" ]; then
	pass "the journal the crash left holds: $kinds"
else
	fail "the reader returned nothing at all for the journal the crash left"
fi

# --- the projection export after replay equals a from-scratch replay's -------------------

header "the projection is rebuildable from the journal alone (invariant 1)"
if diff -u "$replay_export" "$store_export" >"$SANDBOX/export.diff"; then
	pass "the export after replay equals the export of a from-scratch replay ($(wc -l <"$store_export" | tr -d ' ') lines)"
else
	fail "the two exports differ"
	sed 's/^/      | /' "$SANDBOX/export.diff"
fi

header "the control for that comparison: the export moves when the projection does"
# Without this the equality above could be passing on two empty documents, or on a comparison that
# never looks at a row. The store the SIGKILL left is a second real projection, produced by the same
# code and several records behind the final one.
behind="$GATE_OUT/$MODE.export.at-kill"
dump_export_of "$SANDBOX/at-kill-state" >"$behind" ||
	fatal "could not export the store the crash left"
if ! diff -q "$behind" "$store_export" >/dev/null; then
	pass "the store the crash left exports differently from the one the run ended with"
	note "kept at $behind"
else
	fail "a projection several records behind the log exports the same document"
fi

finish "$TITLE"
