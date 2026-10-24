//! Usage accounting from a `result` line, and each API call's own tokens from an `assistant`
//! line. Invariant 4 (budget is admission) needs the real numbers: on Claude Code 2.1.239 the
//! result's `usage` block is all zero on the error arm while `modelUsage` and `total_cost_usd`
//! carry what was actually spent, so this module never reads the result's `usage`.

use std::collections::BTreeMap;

use dudes_proto::ContextReport;
use dudes_proto::providers::claude::{ApiUsage, ResultLine};
use dudes_types::{Tokens, Usage, Usd};

use crate::stream::UsageError;

/// Sum `modelUsage` over every model (main loop and subagents alike) and take the cost from
/// `total_cost_usd`. When that is absent the cost is the sum of the per-model `costUSD`
/// values, each validated and rounded to micro-dollars before summing: a model without a
/// `costUSD` is [`UsageError::MissingCost`], an unusable value is [`UsageError::InvalidCost`],
/// and the first offender in key order wins. No models and no total → `None`.
pub fn account(result: &ResultLine) -> Result<Usage, UsageError> {
    let mut usage = Usage::ZERO;
    let mut per_model_cost: Option<Usd> = None;
    for (model, used) in &result.model_usage {
        usage.input = usage.input.saturating_add(Tokens::new(used.input_tokens));
        usage.output = usage.output.saturating_add(Tokens::new(used.output_tokens));
        usage.cache_read = usage
            .cache_read
            .saturating_add(Tokens::new(used.cache_read_input_tokens));
        usage.cache_creation = usage
            .cache_creation
            .saturating_add(Tokens::new(used.cache_creation_input_tokens));
        if result.total_cost_usd.is_none() {
            let cost = used.cost_usd.ok_or_else(|| UsageError::MissingCost {
                model: model.clone(),
            })?;
            let micros = micro_dollars(cost)?.micros();
            per_model_cost = Some(Usd::from_micros(
                per_model_cost.map_or(0, Usd::micros).saturating_add(micros),
            ));
        }
    }
    usage.usd = match result.total_cost_usd {
        Some(total) => Some(micro_dollars(total)?),
        None => per_model_cost,
    };
    Ok(usage)
}

/// The same `modelUsage` map, **uncollapsed**: one [`Usage`] per key the vendor named, keys kept
/// byte-for-byte.
///
/// This is evidence and never a charge, which is what makes it a different function from
/// [`account`] rather than a second return value of it:
///
/// * a key's tokens are its own reported counts, not a share of anything;
/// * a valid `costUSD` becomes that key's `usd`; a **missing or unusable** one leaves it `None`
///   and gives it nothing from `total_cost_usd`, because a total apportioned across keys is
///   invented cost and invariant 4 admits only reported cost;
/// * nothing here fails. [`account`] still refuses a partial cost for the aggregate under
///   the all-or-nothing cost accounting, and the map still preserves what the vendor did report — the
///   two answers are about different questions and disagreeing is the point.
///
/// The aggregate `usage` on `TurnEnded` remains the only value budget admission reads.
pub fn by_model(result: &ResultLine) -> BTreeMap<String, Usage> {
    result
        .model_usage
        .iter()
        .map(|(model, used)| {
            (
                model.clone(),
                Usage {
                    input: Tokens::new(used.input_tokens),
                    output: Tokens::new(used.output_tokens),
                    cache_read: Tokens::new(used.cache_read_input_tokens),
                    cache_creation: Tokens::new(used.cache_creation_input_tokens),
                    usd: used.cost_usd.and_then(|cost| micro_dollars(cost).ok()),
                },
            )
        })
        .collect()
}

/// A call's reported tokens from its `assistant` line. The turn's sum is a lower bound:
/// unseen calls are absent, and some releases repeat an initial output count on later lines.
pub(crate) fn call(usage: &ApiUsage) -> Usage {
    Usage {
        input: Tokens::new(usage.input_tokens),
        output: Tokens::new(usage.output_tokens),
        cache_read: Tokens::new(usage.cache_read_input_tokens),
        cache_creation: Tokens::new(usage.cache_creation_input_tokens),
        usd: None,
    }
}

/// The prompt a call sent: fresh, cached and newly cached tokens.
pub(crate) fn prompt_tokens(usage: &ApiUsage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.cache_read_input_tokens)
        .saturating_add(usage.cache_creation_input_tokens)
}

/// The window the vendor reported for `model` in this result, paired with the prompt the turn's
/// last call sent. A model the result does not name falls back to its only entry; with several
/// entries and no match, or no reported window, nothing is claimed.
pub(crate) fn context(result: &ResultLine, model: &str, used: u64) -> Option<ContextReport> {
    let reported = result.model_usage.get(model).or_else(|| {
        let mut all = result.model_usage.values();
        all.next().filter(|_| all.next().is_none())
    });
    let window = reported?.context_window.filter(|window| *window > 0)?;
    Some(ContextReport {
        model: model.chars().take(MAX_MODEL_CHARS).collect(),
        window,
        used,
    })
}

const MAX_MODEL_CHARS: usize = 128;

/// Vendor float dollars → integer micro-dollars, rounded half away from zero. Refused before
/// rounding: NaN, ±∞ and any negative amount, however small (`-1e-9` is an error, not zero).
/// `-0.0` compares equal to zero and is zero. Refused after rounding: anything that does not
/// fit in `u64` micro-dollars.
fn micro_dollars(dollars: f64) -> Result<Usd, UsageError> {
    if !dollars.is_finite() || dollars < 0.0 {
        return Err(UsageError::InvalidCost(dollars));
    }
    let micros = (dollars * 1_000_000.0).round();
    // u64::MAX as f64 rounds up to 2^64, which is itself out of range, so `<` is the bound.
    #[allow(clippy::cast_precision_loss)] // a bound, not a value
    if micros >= u64::MAX as f64 {
        return Err(UsageError::InvalidCost(dollars));
    }
    // Non-negative and below the bound, so the cast is exact for the integer-valued `micros`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(Usd::from_micros(micros as u64))
}

#[cfg(test)]
mod tests {
    use dudes_proto::providers::claude::ModelUsage;

    use super::*;

    fn model(output: u64, cost: Option<f64>) -> ModelUsage {
        ModelUsage {
            input_tokens: 1,
            output_tokens: output,
            cache_read_input_tokens: 10,
            cache_creation_input_tokens: 100,
            cost_usd: cost,
            ..ModelUsage::default()
        }
    }

    fn per_model(models: &[(&str, Option<f64>)]) -> ResultLine {
        ResultLine {
            total_cost_usd: None,
            model_usage: models
                .iter()
                .map(|(name, cost)| ((*name).to_owned(), model(2, *cost)))
                .collect(),
            ..ResultLine::default()
        }
    }

    /// The per-model breakdown, the whole behaviour in one hand-built result: the breakdown preserves what the
    /// vendor reported per model, the aggregate stays exactly what it was, and neither borrows
    /// from the other.
    ///
    /// **Hand-built rather than a captured fixture, and that is a limitation worth naming.** Every
    /// recorded Claude stream under `tests/fixtures/claude/` carries exactly one `modelUsage` key
    /// or none, so no fixture can exercise the subagent case at all; extending `tests/golden.rs`
    /// over that corpus would assert nothing about two keys. This proves the accounting. It does
    /// not prove the vendor emits this shape — that needs a captured two-key stream.
    ///
    /// **Four arms, and the third is the one that carries the claim.** "Both keys are present" is
    /// satisfied by an implementation that apportions the total across them; only a key whose cost
    /// the vendor never reported can tell those apart, and it must stay `None`. The fourth arm is
    /// the aggregate's own control: with no top-level total and one per-model cost missing,
    /// `account` still refuses under the all-or-nothing cost accounting while the sidecar keeps every
    /// token count the vendor did report.
    #[test]
    fn a_two_key_result_keeps_both_models_and_invents_no_cost() {
        // One main-loop model and one subagent, with different counts, and a cost for the main
        // model only.
        let result = ResultLine {
            total_cost_usd: Some(0.75),
            model_usage: BTreeMap::from([
                ("claude-haiku-4-5".to_owned(), model(9, Some(0.25))),
                ("claude-opus-4-1-20250805".to_owned(), model(4, None)),
            ]),
            ..ResultLine::default()
        };
        let by = by_model(&result);

        // 1. Both keys survive, spelled exactly as the vendor spelled them.
        assert_eq!(
            by.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["claude-haiku-4-5", "claude-opus-4-1-20250805"],
            "both raw keys survive, including the subagent's"
        );
        // 2. Each key carries its own counts, not the other's and not the total's.
        assert_eq!(by["claude-haiku-4-5"].output.count(), 9);
        assert_eq!(by["claude-opus-4-1-20250805"].output.count(), 4);
        assert_eq!(by["claude-haiku-4-5"].usd, Some(Usd::from_micros(250_000)));
        // 3. **The load-bearing arm.** The key the vendor priced nothing for gets nothing — not a
        //    zero, not a share of the $0.75 total. An implementation that allocated the total
        //    across keys passes arms 1 and 2 and fails here.
        assert_eq!(
            by["claude-opus-4-1-20250805"].usd, None,
            "a missing per-model cost stays missing and is never allocated from the total"
        );

        // 4. The aggregate is untouched by any of it: the top-level total is still authoritative
        //    and the tokens are still the saturating sum.
        let aggregate = account(&result).expect("a valid total is charged as it always was");
        assert_eq!(aggregate.usd, Some(Usd::from_micros(750_000)));
        assert_eq!(aggregate.output.count(), 13, "9 + 4, summed as before");
        assert_eq!(aggregate.input.count(), 2);

        // The aggregate's own control, on the arm the all-or-nothing cost accounting governs: with no top-level total and
        // one per-model cost missing, the *aggregate* is refused outright while the sidecar still
        // records every token the vendor reported. The two answers differ, deliberately.
        let partial = ResultLine {
            total_cost_usd: None,
            ..result
        };
        assert!(
            matches!(
                account(&partial),
                Err(UsageError::MissingCost { ref model }) if model == "claude-opus-4-1-20250805"
            ),
            "invariant 4: a partial cost is refused for the aggregate, exactly as before"
        );
        let by_partial = by_model(&partial);
        assert_eq!(
            by_partial.len(),
            2,
            "the breakdown is unaffected by that refusal"
        );
        assert_eq!(
            by_partial["claude-haiku-4-5"].usd,
            Some(Usd::from_micros(250_000))
        );
        assert_eq!(by_partial["claude-opus-4-1-20250805"].usd, None);
        assert_eq!(by_partial["claude-opus-4-1-20250805"].output.count(), 4);
    }

    /// An unusable per-model cost is `None` for that key rather than an error, because the
    /// breakdown is an observation and refusing the whole map would lose the token counts beside
    /// it. The aggregate keeps refusing it, which is the pair that makes each answer legible.
    #[test]
    fn an_unusable_per_model_cost_leaves_that_key_unpriced() {
        let result = per_model(&[("a", Some(f64::NAN)), ("b", Some(2.0))]);
        let by = by_model(&result);
        assert_eq!(by["a"].usd, None, "an unusable cost prices nothing");
        assert_eq!(
            by["a"].output.count(),
            2,
            "and costs the key none of its tokens"
        );
        assert_eq!(by["b"].usd, Some(Usd::from_micros(2_000_000)));
        assert!(
            matches!(account(&result), Err(UsageError::InvalidCost(c)) if c.is_nan()),
            "the aggregate still refuses it (invariant 4)"
        );
    }

    /// No `modelUsage` at all is an empty breakdown, which `TurnEnded` writes as no field.
    #[test]
    fn no_model_usage_is_an_empty_breakdown() {
        assert!(by_model(&ResultLine::default()).is_empty());
    }

    /// `usage` is never read. A result whose `usage` block is huge but whose
    /// `modelUsage` is empty accounts to zero tokens.
    #[test]
    fn usage_block_is_never_read() {
        let result = ResultLine {
            usage: Some(ApiUsage {
                input_tokens: u64::MAX,
                output_tokens: u64::MAX,
                cache_creation_input_tokens: u64::MAX,
                cache_read_input_tokens: u64::MAX,
            }),
            model_usage: BTreeMap::new(),
            ..ResultLine::default()
        };
        assert_eq!(account(&result).unwrap(), Usage::ZERO);
    }

    /// Tokens are summed across every model and the cost falls back to the sum
    /// of per-model `costUSD` when `total_cost_usd` is absent.
    #[test]
    fn sums_across_models_and_falls_back_to_per_model_cost() {
        let result = ResultLine {
            total_cost_usd: None,
            model_usage: BTreeMap::from([
                ("a".to_owned(), model(2, Some(0.001))),
                ("b".to_owned(), model(3, Some(0.0005))),
            ]),
            ..ResultLine::default()
        };
        let usage = account(&result).unwrap();
        assert_eq!(usage.input.count(), 2);
        assert_eq!(usage.output.count(), 5);
        assert_eq!(usage.cache_read.count(), 20);
        assert_eq!(usage.cache_creation.count(), 200);
        assert_eq!(usage.usd, Some(Usd::from_micros(1_500)));
    }

    /// Invariant 4: with `total_cost_usd` absent, a model without `costUSD` makes the whole
    /// account an error — never a partial sum of the models that did report one.
    #[test]
    fn missing_per_model_cost_is_an_error_not_a_partial_sum() {
        let result = per_model(&[("a", Some(1.0)), ("b", None)]);
        assert!(matches!(
            account(&result),
            Err(UsageError::MissingCost { ref model }) if model == "b"
        ));
        // With a total present the per-model values are not consulted at all.
        let result = ResultLine {
            total_cost_usd: Some(0.25),
            ..per_model(&[("a", None)])
        };
        assert_eq!(
            account(&result).unwrap().usd,
            Some(Usd::from_micros(250_000))
        );
    }

    /// Invariant 4: each per-model `costUSD` is validated before it is summed, so a negative
    /// value cannot be netted against a positive one (`[-5.0, 6.0]` is refused, not `$1.00`).
    #[test]
    fn per_model_costs_are_validated_before_summing() {
        let result = per_model(&[("a", Some(-5.0)), ("b", Some(6.0))]);
        assert!(matches!(
            account(&result),
            Err(UsageError::InvalidCost(c)) if c.is_sign_negative()
        ));
    }

    /// Invariant 4: a cost that cannot be charged is an error, never silently zero or
    /// negative — any negative amount is refused before rounding, while `-0.0` is zero. No
    /// cost at all is `None`.
    #[test]
    fn invalid_cost_is_an_error() {
        for bad in [
            -1.0,
            -1e-9,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            1e20,
        ] {
            let result = ResultLine {
                total_cost_usd: Some(bad),
                ..ResultLine::default()
            };
            assert!(
                matches!(account(&result), Err(UsageError::InvalidCost(_))),
                "{bad}"
            );
        }
        let zero = ResultLine {
            total_cost_usd: Some(-0.0),
            ..ResultLine::default()
        };
        assert_eq!(account(&zero).unwrap().usd, Some(Usd::ZERO));
        assert_eq!(account(&ResultLine::default()).unwrap().usd, None);
    }
}
