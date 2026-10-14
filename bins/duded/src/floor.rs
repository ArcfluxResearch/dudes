//! Daemon-local assembly of read-only agent and board rows from journal projections.

use anyhow::{Context as _, Result};
use dudes_journal::{AgentRow, GoalRow, Projections};
use dudes_proto::QueueKey;
use dudes_proto::rpc::floor::FloorFinishedToday;
use dudes_proto::{
    ActivityView, AgentView, CardTicket, CardView, GoalView, LiveActivityView, RetiredAgentView,
};
use dudes_types::{AgentId, CardStatus, GoalId, GoalStatus, Lsn, Timestamp};
use std::collections::BTreeMap;

pub(crate) const MAX_MAIL_BYTES: usize = dudes_proto::MAX_MESSAGES_BYTES;
pub(crate) const ARCHIVE_RESERVE_BYTES: usize =
    dudes_proto::MAX_RETIRED_AGENT_BYTES + b",\"retired_agents\":".len();
pub(crate) const QUESTION_HISTORY_RESERVE_BYTES: usize =
    dudes_proto::MAX_QUESTION_HISTORY_BYTES + b",\"question_history\":".len();
pub(crate) const TRACE_RESERVE_BYTES: usize = dudes_proto::MAX_TRACE_BYTES + b",\"traces\":".len();

/// Daemon-local Floor rows at one applied journal position.
///
/// The Floor stream assembles the wire [`dudes_proto::FloorProjection`] from this value.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSnapshot {
    /// The journal position all rows reflect.
    pub lsn: Lsn,
    /// Every current agent, in the projection's canonical id order.
    pub agents: Vec<AgentView>,
    /// Every board card at the same applied journal position.
    pub cards: Vec<CardView>,
    /// The bounded permitted journal activity at the same position.
    pub activity: Vec<ActivityView>,
    /// The bounded display-only retired-agent archive at the same position.
    pub retired_agents: Vec<RetiredAgentView>,
    /// Recent answered questions for current agents within the browser disclosure limits.
    pub question_history: Vec<dudes_proto::AnsweredQuestionView>,
    /// Recent completed turns for current agents, within the Floor's trace limits.
    pub traces: Vec<dudes_proto::TurnTraceView>,
    /// Bounded agent-to-agent mail metadata at the same journal position.
    pub mail: dudes_journal::projections::MailView,
    /// The count of goals finished on the daemon's local day, read at this position.
    /// `None` where the stream does not carry it, which a page must not read as zero.
    pub finished_today: Option<FloorFinishedToday>,
}

/// Build the Floor rows at the projection's current journal position.
///
/// `now` comes from the daemon's clock and bounds the activity display lifetime.
///
/// # Errors
///
/// Returns the projection read error, including an unrepresentable unread count, or an error when
/// an agent row names a goal row the projection does not contain.
#[doc(hidden)]
pub fn snapshot(projections: &mut Projections, now: Timestamp) -> Result<AgentSnapshot> {
    let mail = projections
        .mail_view()
        .context("could not read paired mail metadata")?;
    let lsn = mail.lsn;
    let agents = agents(projections, now)?;
    let cards = cards(projections)?;
    let activity = projections
        .activity()
        .context("could not read projected activity")?;
    Ok(AgentSnapshot {
        lsn,
        agents,
        cards,
        activity,
        retired_agents: projections
            .retired_agents()
            .context("could not read retired agents")?,
        question_history: projections
            .question_history()
            .context("could not read answered question history")?,
        traces: projections.traces().context("could not read turn traces")?,
        mail,
        finished_today: None,
    })
}

/// Every board card, with the summary a ticket card carries. The summary holds codes and counts
/// only: the note, the agent's summary and the check result come from `ticket.read`.
pub(crate) fn cards(projections: &Projections) -> Result<Vec<CardView>> {
    let mut cards = projections
        .cards()
        .context("could not read projected board cards")?;
    let tickets = projections
        .tickets()
        .context("could not read projected tickets")?;
    let by_id: BTreeMap<_, _> = tickets
        .iter()
        .map(|(view, ticket)| (view.id, ticket))
        .collect();
    // Each agent's queue in serving order, so a place is a lookup and not a scan.
    let mut queues: BTreeMap<AgentId, Vec<QueueKey>> = BTreeMap::new();
    for (view, ticket) in &tickets {
        if let (CardStatus::Claimed, Some(agent), Some(key)) =
            (view.status, view.claimed_by, ticket.queue)
        {
            queues.entry(agent).or_default().push(key);
        }
    }
    for queue in queues.values_mut() {
        queue.sort_unstable();
    }
    for card in &mut cards {
        let Some(ticket) = by_id.get(&card.id) else {
            continue;
        };
        let position = match (card.status, card.claimed_by, ticket.queue) {
            (CardStatus::Claimed, Some(agent), Some(key)) => queues
                .get(&agent)
                .and_then(|queue| queue.binary_search(&key).ok())
                .and_then(|at| u32::try_from(at + 1).ok()),
            _ => None,
        };
        card.ticket = Some(CardTicket {
            position,
            round: ticket.round,
            reason: if card.status == CardStatus::Blocked {
                ticket.reason
            } else {
                None
            },
            stopping: card.status == CardStatus::Todo && finishing(projections, ticket)?,
            instruction_review: ticket
                .instruction_review
                .as_ref()
                .map(dudes_proto::InstructionPendingReview::summary),
        });
    }
    Ok(cards)
}

/// A stopped ticket's old goal is cleared while its agent is still in a turn.
fn finishing(projections: &Projections, ticket: &dudes_proto::TicketRow) -> Result<bool> {
    let Some(goal) = ticket.goal else {
        return Ok(false);
    };
    let Some(goal) = projections
        .goal(goal)
        .context("could not read a ticket's goal")?
        .filter(|row| row.status == GoalStatus::Abandoned)
    else {
        return Ok(false);
    };
    Ok(projections
        .agent(goal.agent)
        .context("could not read a ticket's agent")?
        .is_some_and(|row| row.open_turn.is_some()))
}

/// The count of goals first finished, with an observed pass, on the daemon's local day.
///
/// Unavailable, never zero, when the host zone gives no usable day, a completion could not be
/// resolved, or the projection cannot be read.
#[must_use]
pub fn finished_today(
    projections: &Projections,
    days: &crate::local_day::LocalDays,
) -> FloorFinishedToday {
    finished_in(projections, days.window().as_ref())
}

/// [`finished_today`] over a day the caller already sampled.
#[must_use]
pub fn finished_in(
    projections: &Projections,
    day: Option<&crate::local_day::DayWindow>,
) -> FloorFinishedToday {
    let Some(day) = day else {
        return FloorFinishedToday::Unavailable;
    };
    match projections.first_done_count(day.start, day.end) {
        Ok(Some((total, lsn))) => FloorFinishedToday::Ready {
            local_date: day.local_date.clone(),
            utc_offset_minutes: day.utc_offset_minutes,
            total,
            through_lsn: lsn.0,
        },
        Ok(None) => FloorFinishedToday::Unavailable,
        Err(error) => {
            tracing::warn!(%error, "the finished-today count could not be read");
            FloorFinishedToday::Unavailable
        }
    }
}

/// Every hired, unretired agent in the projection's canonical id order: the Floor's rows and
/// `agent.list`'s answer.
///
/// # Errors
///
/// As [`snapshot`], for the agent rows.
pub(crate) fn agents(projections: &Projections, now: Timestamp) -> Result<Vec<AgentView>> {
    let rows = projections
        .agents()
        .context("could not read projected agents")?;
    let leases = projections
        .worktrees()
        .context("could not read projected worktrees")?;
    let mut directories: std::collections::BTreeMap<std::path::PathBuf, usize> =
        std::collections::BTreeMap::new();
    for row in &rows {
        *directories.entry(row.cwd.clone()).or_default() += 1;
    }
    rows.into_iter()
        .map(|row| {
            let shares_checkout = directories.get(&row.cwd).is_some_and(|count| *count > 1);
            let worktree = row
                .worktree
                .and_then(|id| leases.iter().find(|lease| lease.worktree == id))
                .map(dudes_journal::WorktreeRow::view);
            let mut view = agent_view(projections, row, now)?;
            view.worktree = worktree;
            view.shares_checkout = shares_checkout;
            Ok(view)
        })
        .collect()
}

fn agent_view(projections: &Projections, row: AgentRow, now: Timestamp) -> Result<AgentView> {
    let goal_id = row.goal.unwrap_or(GoalId::NIL);
    let goal_row = row
        .goal
        .map(|id| {
            let goal = projections
                .goal(id)
                .with_context(|| format!("could not read projected goal {id}"))?;
            required_goal(row.id, id, goal)
        })
        .transpose()?;
    let views = projections
        .spend_views(row.id, goal_id, now.utc_day())
        .with_context(|| format!("could not read projected spend for agent {}", row.id))?;
    let goal = goal_row.map(|goal| GoalView {
        completion_check: goal.completion_check,
        id: goal.id,
        text: goal.text,
        status: goal.status,
        budget: goal.budget,
        turns: goal.turns_on_goal,
        hop_cap: goal.hop_cap,
        spent: views.goal,
        last_result: goal.last_result,
        baseline: goal.baseline.as_ref().map(dudes_proto::CheckBaseline::kind),
    });
    let unread = projections
        .unread_by_agent(row.id)
        .with_context(|| format!("could not read unread mail for projected agent {}", row.id))?;

    Ok(AgentView {
        character: row.character,
        accent: row.accent,
        id: row.id,
        name: row.name,
        role: row.role,
        look_lsn: row.look_lsn.map(|lsn| lsn.0.to_string()),
        department: row.department,
        provider: row.provider,
        workspace: row.workspace,
        status: row.status,
        live_activity: row.live_activity.and_then(|activity| {
            remaining_lifetime(activity.at, now, u32::MAX).map(|expires_in_ms| LiveActivityView {
                kind: activity.kind,
                at: activity.at,
                expires_in_ms,
            })
        }),
        mode: row.mode,
        paused: row.paused,
        halted: row.halted,
        breaker: row.breaker,
        goal,
        turns: row.turns,
        spent_today: views.agent_today,
        unread,
        question: row.question,
        question_lsn: row.question_lsn.map(|lsn| lsn.0.to_string()),
        automatic_hold: row.automatic_hold,
        control_lsn: row.control_lsn.map(|lsn| lsn.0.to_string()),
        session: row.session,
        last_turn_ended_at: row.last_turn_ended_at,
        worktree: None,
        shares_checkout: false,
    })
}

/// Recompute each agent's remaining display lifetime at `now`, never above the value it had.
pub(crate) fn refresh_live_activity(agents: &mut [AgentView], now: Timestamp) {
    for agent in agents {
        agent.live_activity = agent.live_activity.and_then(|activity| {
            remaining_lifetime(activity.at, now, activity.expires_in_ms).map(|expires_in_ms| {
                LiveActivityView {
                    expires_in_ms,
                    ..activity
                }
            })
        });
    }
}

/// Milliseconds left of the 60-second display lifetime of activity recorded `at`, capped at
/// `cap` so a backward clock step cannot lengthen a lifetime already computed. `None` once none
/// is left, or while `at` is later than `now`.
pub(crate) fn remaining_lifetime(at: Timestamp, now: Timestamp, cap: u32) -> Option<u32> {
    const LIFETIME_MS: i64 = 60_000;
    let age = now.millis_since(at);
    (0..LIFETIME_MS)
        .contains(&age)
        .then(|| {
            u32::try_from(LIFETIME_MS - age)
                .expect("recent activity age fits u32")
                .min(cap)
        })
        .filter(|remaining| *remaining > 0)
}

fn required_goal(
    agent: dudes_types::AgentId,
    id: GoalId,
    goal: Option<GoalRow>,
) -> Result<GoalRow> {
    goal.with_context(|| format!("projected agent {agent} names missing goal {id}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dudes_types::AgentId;

    use dudes_proto::{Entry, LiveActivityKind, Record};

    #[test]
    fn dangling_projected_goal_is_an_error() {
        let agent = AgentId::from_parts(1, 1);
        let goal = GoalId::from_parts(2, 1);
        let error = required_goal(agent, goal, None).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("projected agent {agent} names missing goal {goal}")
        );
    }

    #[test]
    fn a_fresh_agent_view_omits_old_and_future_activity() {
        let agent = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        let mut projections = Projections::in_memory().unwrap();
        projections
            .apply(
                Lsn(1),
                0,
                &Entry {
                    at: Timestamp::from_unix_millis(1_000),
                    record: serde_json::from_value(serde_json::json!({
                        "t":"agent_hired", "id":agent, "name":"Ada", "role":"builder",
                        "department":"engineering", "provider":"claude", "cwd":"/work",
                        "budget":{}
                    }))
                    .unwrap(),
                },
            )
            .unwrap();
        projections
            .apply(
                Lsn(2),
                0,
                &Entry {
                    at: Timestamp::from_unix_millis(2_000),
                    record: Record::LiveActivityObserved {
                        agent,
                        kind: LiveActivityKind::UsingTool,
                    },
                },
            )
            .unwrap();
        assert_eq!(
            agents(&projections, Timestamp::from_unix_millis(32_000)).unwrap()[0].live_activity,
            Some(LiveActivityView {
                kind: LiveActivityKind::UsingTool,
                at: Timestamp::from_unix_millis(2_000),
                expires_in_ms: 30_000,
            })
        );
        assert_eq!(
            agents(&projections, Timestamp::from_unix_millis(62_000)).unwrap()[0].live_activity,
            None
        );
        assert_eq!(
            agents(&projections, Timestamp::from_unix_millis(1_999)).unwrap()[0].live_activity,
            None
        );

        let mut cached = agents(&projections, Timestamp::from_unix_millis(52_000)).unwrap();
        refresh_live_activity(&mut cached, Timestamp::from_unix_millis(12_000));
        assert_eq!(
            cached[0]
                .live_activity
                .map(|activity| activity.expires_in_ms),
            Some(10_000),
            "a backward clock step keeps the lifetime computed at 52 s"
        );
        refresh_live_activity(&mut cached, Timestamp::from_unix_millis(55_000));
        assert_eq!(
            cached[0]
                .live_activity
                .map(|activity| activity.expires_in_ms),
            Some(7_000)
        );
    }

    fn day_of_ten_seconds() -> crate::local_day::LocalDays {
        crate::local_day::LocalDays::from_fn(|| {
            Some(crate::local_day::DayWindow {
                local_date: "2026-10-07".to_owned(),
                utc_offset_minutes: 780,
                start: Timestamp::from_unix_millis(0),
                end: Timestamp::from_unix_millis(10_000),
            })
        })
    }

    fn hired_projection() -> (Projections, AgentId, GoalId) {
        let agent = AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        let goal = GoalId::from_parts(5, 1);
        let mut projections = Projections::in_memory().unwrap();
        let at = Timestamp::from_unix_millis(1_000);
        for (lsn, record) in [
            (
                1,
                serde_json::from_value::<Record>(serde_json::json!({
                    "t":"agent_hired", "id":agent, "name":"Ada", "role":"builder",
                    "department":"engineering", "provider":"claude", "cwd":"/work",
                    "budget":{}
                }))
                .unwrap(),
            ),
            (
                2,
                Record::GoalSet {
                    agent,
                    goal: dudes_proto::Goal {
                        completion_check: None,
                        id: goal,
                        text: "ship".into(),
                        predicate: dudes_proto::Predicate::Command {
                            argv: vec!["true".into()],
                            timeout_s: 1,
                        },
                        budget: dudes_proto::GoalBudget {
                            usd: None,
                            tokens: None,
                        },
                        hop_cap: 3,
                        baseline: None,
                    },
                    browser_onboarding: None,
                    browser_goal: None,
                    goal_replacement: None,
                },
            ),
        ] {
            projections
                .apply(Lsn(lsn), 0, &Entry { at, record })
                .unwrap();
        }
        (projections, agent, goal)
    }

    fn done_for(projections: &mut Projections, lsn: u64, agent: AgentId) {
        let record = Record::LoopDecided {
            agent,
            input_digest: "d".into(),
            decision: dudes_proto::LoopDecision::Done,
            gate_trace: Vec::new(),
        };
        projections
            .apply(
                Lsn(lsn),
                0,
                &Entry {
                    at: Timestamp::from_unix_millis(5_000),
                    record,
                },
            )
            .unwrap();
    }

    #[test]
    fn finished_today_is_ready_with_the_day_and_total_and_never_zero_when_it_cannot_be_read() {
        let (mut projections, agent, goal) = hired_projection();
        let observed = Record::PredicateResult {
            goal,
            turn: None,
            provenance: dudes_types::Provenance::Observed,
            pass: true,
            evidence: dudes_proto::Evidence::Command {
                argv: vec!["true".into()],
                program: None,
                exit: Some(0),
                output_tail: String::new(),
            },
        };
        projections
            .apply(
                Lsn(3),
                0,
                &Entry {
                    at: Timestamp::from_unix_millis(4_000),
                    record: observed,
                },
            )
            .unwrap();
        done_for(&mut projections, 4, agent);
        assert_eq!(
            finished_today(&projections, &day_of_ten_seconds()),
            FloorFinishedToday::Ready {
                local_date: "2026-10-07".to_owned(),
                utc_offset_minutes: 780,
                total: 1,
                through_lsn: 4,
            }
        );
        let no_zone = crate::local_day::LocalDays::from_fn(|| None);
        assert_eq!(
            finished_today(&projections, &no_zone),
            FloorFinishedToday::Unavailable,
            "no usable zone is unavailable, never a fall back to UTC or to zero"
        );
    }

    #[test]
    fn a_done_without_an_observed_pass_makes_the_total_unavailable() {
        let (mut projections, agent, _) = hired_projection();
        done_for(&mut projections, 3, agent);
        assert_eq!(
            finished_today(&projections, &day_of_ten_seconds()),
            FloorFinishedToday::Unavailable
        );
    }
}
