//! How a managed turn ended, from the `result` line and the exit status — nothing else.
//!
//! The four signals are all load-bearing on Claude Code 2.1.239: an auth failure arrives as
//! `subtype: "success"` with `is_error: true` and exit 1; a budget stop arrives as
//! `error_max_budget_usd` with exit 1; a stream can end with no `result` line at all. No hook
//! event takes part: [`turn_outcome`] has no parameter for one, so a `Stop` hook can never be
//! a precondition for closing a turn (the vendor fires none on the ceiling exits).
//!
//! The two strings an outcome carries (`last_message`, `reason`) are model-controlled and
//! journal-bound. They leave here whole: the supervisor scrubs known secrets out of them and only
//! then cuts them at [`MAX_TEXT_BYTES`] with [`bound`], so a secret that straddles the limit is
//! never half kept. Stripping control bytes is the sink's job.

use dudes_proto::ExitStatus;
use dudes_proto::providers::claude::ResultLine;

use crate::stream::exit_name;
pub use crate::stream::{MAX_TEXT_BYTES, bound};

/// What the adapter concludes when the child has exited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    /// Every signal agreed: `success`, not an error, `completed` (or unstated), exit 0.
    Completed {
        /// The final text the vendor reported, if any.
        last_message: Option<String>,
    },
    /// The vendor's `--max-turns` ceiling ended the turn.
    MaxTurns,
    /// The vendor's `--max-budget-usd` ceiling ended the turn.
    BudgetExhausted,
    /// Anything else the vendor or the exit status called a failure.
    Failed {
        /// The most specific signal available — `terminal_reason`, else the first error
        /// message, else the subtype; or the exit status (`exit code N`, `signal N`,
        /// `exit status unknown`) when only that disagreed — followed by `: ` and the
        /// vendor's `result` text when there is one.
        reason: String,
    },
    /// The stream ended without a `result` line; the exit status is all there is.
    NoResult {
        /// How the child ended.
        exit: ExitStatus,
    },
}

impl From<TurnOutcome> for dudes_proto::TurnOutcome {
    /// Cross to the journalled outcome, dropping the two payloads
    /// [`Record::TurnEnded`](dudes_proto::Record::TurnEnded) already carries as fields of its
    /// own: `Completed.last_message` is that record's `last_message`, and `NoResult.exit` is
    /// its `exit`. **Take both off the provider outcome before converting** — the wire enum has
    /// nowhere to put them. `Failed.reason` has no such field, so it is the one payload that
    /// crosses, whole. The supervisor scrubs and then cuts it before it journals the end: an
    /// over-long reason is refused by the journal writer, so the cut is not optional there.
    ///
    /// The wire's other four variants are unreachable from here and no adapter may produce them:
    /// `Killed{by}` is written when the supervisor's kill ledger holds the turn, and `Vanished`
    /// is the reaper's `Observed` end for a child found gone. `NotLaunched` belongs
    /// to the host-confirmed failure path and `LaunchRefused` is reserved for the replayed launch-refusal
    /// path. None follows from a vendor `result` line, which is all this module reads.
    fn from(outcome: TurnOutcome) -> Self {
        match outcome {
            TurnOutcome::Completed { .. } => Self::Completed,
            TurnOutcome::MaxTurns => Self::MaxTurns,
            TurnOutcome::BudgetExhausted => Self::BudgetExhausted,
            TurnOutcome::Failed { reason } => Self::Failed { reason },
            TurnOutcome::NoResult { .. } => Self::NoResult,
        }
    }
}

/// Derive the outcome. First match wins:
///
/// 1. no `result` line → `NoResult`, whatever the exit code;
/// 2. `error_max_turns` → `MaxTurns`; `error_max_budget_usd` → `BudgetExhausted`;
/// 3. `is_error` → `Failed`, including under `subtype: "success"`;
/// 4. a subtype other than `success` → `Failed`;
/// 5. a `terminal_reason` other than `completed` → `Failed`;
/// 6. an exit that is not code 0 → `Failed`;
/// 7. otherwise `Completed`.
#[must_use]
pub fn turn_outcome(result: Option<&ResultLine>, exit: ExitStatus) -> TurnOutcome {
    let Some(result) = result else {
        return TurnOutcome::NoResult { exit };
    };
    let failed = |signal: String| TurnOutcome::Failed {
        reason: match result.result.as_deref() {
            Some(text) if !text.is_empty() => format!("{signal}: {text}"),
            _ => signal,
        },
    };
    let vendor_signal = || {
        result
            .terminal_reason
            .clone()
            .or_else(|| result.errors.first().cloned())
            .unwrap_or_else(|| result.subtype.clone())
    };
    match result.subtype.as_str() {
        "error_max_turns" => TurnOutcome::MaxTurns,
        "error_max_budget_usd" => TurnOutcome::BudgetExhausted,
        _ if result.is_error => failed(vendor_signal()),
        "success" => match result.terminal_reason.as_deref() {
            None | Some("completed") if exit.is_success() => TurnOutcome::Completed {
                last_message: result.result.clone(),
            },
            None | Some("completed") => failed(exit_name(exit)),
            Some(other) => failed(other.to_owned()),
        },
        _ => failed(vendor_signal()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXIT_0: ExitStatus = ExitStatus {
        code: Some(0),
        signal: None,
    };
    const EXIT_1: ExitStatus = ExitStatus {
        code: Some(1),
        signal: None,
    };
    const SIGNAL_15: ExitStatus = ExitStatus {
        code: None,
        signal: Some(15),
    };

    fn line(subtype: &str, is_error: bool, terminal_reason: Option<&str>) -> ResultLine {
        ResultLine {
            subtype: subtype.into(),
            is_error,
            terminal_reason: terminal_reason.map(str::to_owned),
            result: Some("text".into()),
            ..ResultLine::default()
        }
    }

    /// `Completed` requires all four signals to agree, and the two ceiling
    /// variants are decided by subtype alone. Exhaustive over 5 × 2 × 3 × 4 = 120 inputs.
    #[test]
    fn completed_requires_all_four_signals() {
        let subtypes = [
            "success",
            "error_max_turns",
            "error_max_budget_usd",
            "error_during_execution",
            "weird",
        ];
        let reasons = [None, Some("completed"), Some("api_error")];
        let exits = [EXIT_0, EXIT_1, SIGNAL_15, ExitStatus::UNKNOWN];
        let mut seen = 0;
        for subtype in subtypes {
            for is_error in [false, true] {
                for reason in reasons {
                    for exit in exits {
                        seen += 1;
                        let got = turn_outcome(Some(&line(subtype, is_error, reason)), exit);
                        let all_agree = subtype == "success"
                            && !is_error
                            && matches!(reason, None | Some("completed"))
                            && exit.is_success();
                        let case = format!("{subtype}/{is_error}/{reason:?}/{exit:?}");
                        match subtype {
                            "error_max_turns" => assert_eq!(got, TurnOutcome::MaxTurns, "{case}"),
                            "error_max_budget_usd" => {
                                assert_eq!(got, TurnOutcome::BudgetExhausted, "{case}");
                            }
                            _ if all_agree => assert_eq!(
                                got,
                                TurnOutcome::Completed {
                                    last_message: Some("text".into())
                                },
                                "{case}"
                            ),
                            _ => assert!(
                                matches!(got, TurnOutcome::Failed { .. }),
                                "{case}: {got:?}"
                            ),
                        }
                    }
                }
            }
        }
        assert_eq!(seen, 120);
    }

    /// The auth-failure shape: `success` with `is_error` is a failure; its reason is the
    /// `terminal_reason` followed by the vendor's `result` text, which is where the vendor put
    /// the message a human needs ("Not logged in").
    #[test]
    fn success_with_is_error_is_failure() {
        let result = ResultLine {
            result: Some("Not logged in · Please run /login".into()),
            ..line("success", true, Some("api_error"))
        };
        assert_eq!(
            turn_outcome(Some(&result), EXIT_1),
            TurnOutcome::Failed {
                reason: "api_error: Not logged in · Please run /login".into()
            }
        );
    }

    /// `Failed.reason` precedence: `terminal_reason`, else `errors[0]`, else the subtype; the
    /// `result` text follows only when present and non-empty.
    #[test]
    fn failed_reason_falls_back_to_first_error_then_subtype() {
        let result = ResultLine {
            errors: vec!["boom".into(), "later".into()],
            result: None,
            ..line("error_during_execution", true, None)
        };
        assert_eq!(
            turn_outcome(Some(&result), EXIT_1),
            TurnOutcome::Failed {
                reason: "boom".into()
            }
        );
        let result = ResultLine {
            result: Some(String::new()),
            ..line("error_during_execution", true, None)
        };
        assert_eq!(
            turn_outcome(Some(&result), EXIT_1),
            TurnOutcome::Failed {
                reason: "error_during_execution".into()
            }
        );
    }

    /// A turn that only the exit status called a failure names it in stable words, never a
    /// `Debug` rendering, so the journal string survives a type change.
    #[test]
    fn exit_failure_has_a_stable_name() {
        let reason = |exit| match turn_outcome(Some(&line("success", false, None)), exit) {
            TurnOutcome::Failed { reason } => reason,
            other => panic!("{other:?}"),
        };
        assert_eq!(reason(EXIT_1), "exit code 1: text");
        assert_eq!(reason(SIGNAL_15), "signal 15: text");
        assert_eq!(reason(ExitStatus::UNKNOWN), "exit status unknown: text");
    }

    /// The outcome carries the vendor's words whole; the supervisor cuts them after scrubbing.
    #[test]
    fn text_leaves_whole() {
        let huge = "x".repeat(50 * 1024);
        let got = turn_outcome(
            Some(&ResultLine {
                result: Some(huge.clone()),
                ..line("success", false, None)
            }),
            EXIT_0,
        );
        assert_eq!(
            got,
            TurnOutcome::Completed {
                last_message: Some(huge.clone())
            }
        );
        let got = turn_outcome(
            Some(&ResultLine {
                result: Some(huge.clone()),
                ..line("success", true, Some("api_error"))
            }),
            EXIT_1,
        );
        let TurnOutcome::Failed { reason } = got else {
            panic!("{got:?}");
        };
        assert_eq!(reason, format!("api_error: {huge}"));
    }

    /// `bound` cuts on a char boundary with a visible marker; short text is untouched.
    #[test]
    fn bound_cuts_on_a_char_boundary() {
        let huge = "x".repeat(50 * 1024);
        let prefix = "a".repeat(MAX_TEXT_BYTES - 4);
        let mixed = format!("{prefix}{}", "€".repeat(100));
        assert_eq!(bound(&mixed), format!("{prefix}…"));
        assert_eq!(bound(&mixed).len(), MAX_TEXT_BYTES - 1);
        assert_eq!(bound(&huge).len(), MAX_TEXT_BYTES);
        assert_eq!(bound("short"), "short");
        assert_eq!(bound(&huge[..MAX_TEXT_BYTES]), &huge[..MAX_TEXT_BYTES]);
    }

    /// A stream that ended without a `result` line still closes to a definite
    /// outcome from the exit status alone — for every kind of exit, including a clean one —
    /// and no hook event is needed or possible.
    #[test]
    fn stream_without_result_still_closes() {
        for exit in [EXIT_0, EXIT_1, SIGNAL_15, ExitStatus::UNKNOWN] {
            assert_eq!(turn_outcome(None, exit), TurnOutcome::NoResult { exit });
        }
    }

    /// the five outcomes an adapter can reach cross one for one into the wire enum
    /// `dudes_proto::TurnOutcome`, and the crossing drops exactly the two payloads
    /// `Record::TurnEnded` already carries as fields of its own — `Completed.last_message` and
    /// `NoResult.exit`. `Failed.reason` has no such field, so it crosses verbatim. The wire's
    /// other four variants are unreachable from here: `Killed` is the supervisor's kill ledger,
    /// `Vanished` the reaper's absence probe, `NotLaunched` the host result, and `LaunchRefused`
    /// the separately implemented adapter-refusal lifecycle. No provider result implies one.
    #[test]
    fn every_outcome_crosses_to_its_wire_variant() {
        // Exhaustive over the provider enum, so a sixth variant stops this compiling instead of
        // going unmapped, and it names the wire tag the journal gives that outcome rather than
        // deriving one, so the vocabulary is pinned here and not read back off the conversion.
        // A `cases.len()` assertion could do neither: an array literal's length is a
        // compile-time constant that agrees with itself whatever the enum does.
        fn wire_tag(outcome: &TurnOutcome) -> &'static str {
            match outcome {
                TurnOutcome::Completed { .. } => "completed",
                TurnOutcome::MaxTurns => "max_turns",
                TurnOutcome::BudgetExhausted => "budget_exhausted",
                TurnOutcome::Failed { .. } => "failed",
                TurnOutcome::NoResult { .. } => "no_result",
            }
        }

        let cases = [
            (
                TurnOutcome::Completed {
                    last_message: Some("done".into()),
                },
                dudes_proto::TurnOutcome::Completed,
            ),
            (TurnOutcome::MaxTurns, dudes_proto::TurnOutcome::MaxTurns),
            (
                TurnOutcome::BudgetExhausted,
                dudes_proto::TurnOutcome::BudgetExhausted,
            ),
            (
                TurnOutcome::Failed {
                    reason: "api_error: Not logged in".into(),
                },
                dudes_proto::TurnOutcome::Failed {
                    reason: "api_error: Not logged in".into(),
                },
            ),
            (
                TurnOutcome::NoResult { exit: SIGNAL_15 },
                dudes_proto::TurnOutcome::NoResult,
            ),
        ];

        // Collected rather than asserted one at a time: two arms swapped in the conversion must
        // show as exactly two wrong variants with the other three right, and a run that aborts
        // on the first mismatch cannot show that.
        let mut wrong: Vec<String> = Vec::new();
        let mut covered: Vec<&str> = Vec::new();
        for (outcome, want) in cases {
            let tag = wire_tag(&outcome);
            covered.push(tag);
            let got = dudes_proto::TurnOutcome::from(outcome);
            let got_tag = serde_json::to_value(&got).expect("wire outcome serialises")["kind"]
                .as_str()
                .map(str::to_owned);
            if got != want || got_tag.as_deref() != Some(tag) {
                wrong.push(format!("{tag}: got {got:?} ({got_tag:?}), want {want:?}"));
            }
            assert!(
                !matches!(got_tag.as_deref(), Some("not_launched" | "launch_refused")),
                "provider conversion produced a supervisor-written outcome"
            );
        }
        assert_eq!(wrong, Vec::<String>::new(), "turn-outcome mapping");
        assert_eq!(
            covered,
            [
                "completed",
                "max_turns",
                "budget_exhausted",
                "failed",
                "no_result"
            ],
            "one case per provider variant, in the journal's order"
        );

        // The dropped payloads are dropped, not re-encoded: two `Completed`s differing only in
        // `last_message`, and two `NoResult`s differing only in `exit`, are one wire value each.
        let completed = |last_message: Option<&str>| {
            dudes_proto::TurnOutcome::from(TurnOutcome::Completed {
                last_message: last_message.map(str::to_owned),
            })
        };
        assert_eq!(completed(Some("done")), completed(None));
        // An *empty* `last_message` is a completed turn too, and it is production-reachable: a
        // vendor `"result": ""` gives `Completed { last_message: Some("") }`, because
        // `turn_outcome` maps the text through `bound` and `bound` returns "" unchanged. An arm
        // that read emptiness as absence would journal `no_result` for a turn that completed —
        // a dispatch hole, not a payload transform, so it is asserted separately from the
        // `Some`/`None` pair above rather than assumed to follow from it.
        assert_eq!(completed(Some("")), completed(Some("done")));
        let no_result = |exit| dudes_proto::TurnOutcome::from(TurnOutcome::NoResult { exit });
        assert_eq!(no_result(EXIT_0), no_result(ExitStatus::UNKNOWN));

        // The one payload that is kept crosses whole; the supervisor cuts it after scrubbing.
        let crossed =
            |reason: String| match dudes_proto::TurnOutcome::from(TurnOutcome::Failed { reason }) {
                dudes_proto::TurnOutcome::Failed { reason } => reason,
                other => panic!("a `Failed` crossed to {other:?}"),
            };
        let oversized = "x".repeat(10 * MAX_TEXT_BYTES);
        assert_eq!(crossed(oversized.clone()), oversized);
        assert_eq!(crossed("short".to_owned()), "short");
    }
}
