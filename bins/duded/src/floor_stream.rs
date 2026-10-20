//! Run-local publication state for the authenticated browser Floor stream.
//!
//! Browser-wire-v3 carries the projection stream and one read-only selected Managed output view.
//! The projection payloads remain the shared [`FloorProjection`] and [`FloorDelta`] value shapes.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use dudes_proto::projection::{FloorDeltaView, FloorProjectionView};
use dudes_proto::rpc::floor::FloorFinishedToday;
use dudes_proto::{AgentView, CardView, FloorDelta, FloorProjection};
use dudes_types::{AgentId, CardId, Mode, Timestamp, TurnId};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::floor::AgentSnapshot;
use crate::floor_actions::OnboardingView;
use crate::floor_answer::AnswerView;
use crate::floor_control::ControlView;
use crate::floor_goal::GoalView;
use crate::floor_retire::RetireView;
use crate::floor_terminal::TerminalView;
use crate::floor_websocket::{MAX_MESSAGE_BYTES, text_frame, text_payload};
use crate::output::{Loss, OutputRegistry, OutputSnapshot, OutputState, ReplyLoss, Unavailable};
use crate::provider_check::ProviderCheck;
use crate::sign_in::SignInCheck;

/// Accepted maximum queued messages on one Floor stream.
pub const MAX_QUEUE_MESSAGES: usize = 256;
/// Accepted maximum complete serialized WebSocket frame bytes queued on one Floor stream.
pub const MAX_QUEUE_BYTES: usize = 8 * 1024 * 1024;

const LIVE: u8 = 0;
const OVERFLOWED: u8 = 1;
const ENDED: u8 = 2;

const CLIENT_TYPES: [&str; 4] = [
    "floor.subscribe",
    "floor.unsubscribe",
    "floor.output.select",
    "floor.output.clear",
];
const SERVER_TYPES: [&str; 11] = [
    "floor.snapshot",
    "floor.delta",
    "floor.resync",
    "floor.heartbeat",
    "disconnected",
    "floor.output.snapshot",
    "floor.output.chunk",
    "floor.output.gap",
    "floor.output.unavailable",
    "floor.output.ended",
    "floor.output.reply",
];

#[derive(Serialize)]
#[serde(tag = "type")]
enum OutputFrame<'a> {
    #[serde(rename = "floor.output.reply")]
    Reply {
        selection_epoch: u64,
        agent_id: AgentId,
        turn_id: TurnId,
        revision: String,
        text: &'a str,
        truncated: bool,
        gap: Option<&'static str>,
        ended: bool,
    },
    #[serde(rename = "floor.output.unavailable")]
    Unavailable {
        selection_epoch: u64,
        agent_id: AgentId,
        reason: &'a str,
    },
    #[serde(rename = "floor.output.snapshot")]
    Snapshot {
        selection_epoch: u64,
        agent_id: AgentId,
        turn_id: TurnId,
        first_seq: u64,
        next_seq: u64,
        chunks: Vec<OutputChunkWire<'a>>,
        truncated: bool,
        ended: bool,
    },
    #[serde(rename = "floor.output.chunk")]
    Chunk {
        selection_epoch: u64,
        agent_id: AgentId,
        turn_id: TurnId,
        seq: u64,
        text: &'a str,
    },
    #[serde(rename = "floor.output.gap")]
    Gap {
        selection_epoch: u64,
        agent_id: AgentId,
        turn_id: TurnId,
        next_seq: u64,
        reason: &'a str,
    },
    #[serde(rename = "floor.output.ended")]
    Ended {
        selection_epoch: u64,
        agent_id: AgentId,
        turn_id: TurnId,
    },
}

#[derive(Serialize)]
struct OutputChunkWire<'a> {
    seq: u64,
    text: &'a str,
}

/// How often a registry with a live subscriber emits [`HEARTBEAT_TYPE`].
///
/// Two seconds against the client's unchanged 5 000 ms staleness threshold, which is what makes
/// **one** lost or late beat tolerable and two consecutive misses not. Shipped as this constant and
/// passed as a parameter, so a test can run the same code at a period it can afford to wait for;
/// the constant fixes what ships, not what a test may use.
pub(crate) const HEARTBEAT_PERIOD: Duration = Duration::from_millis(2_000);

/// The one discriminator a heartbeat carries, and the whole frame beside it.
const HEARTBEAT_TYPE: &str = "floor.heartbeat";

/// The discriminator of the contentless message that tells a page to subscribe again.
const RESYNC_TYPE: &str = "floor.resync";

/// How often the daemon looks at its local day, at most the two seconds allowed between a
/// change and the resync it causes.
pub(crate) const DAY_WATCH_PERIOD: Duration = Duration::from_millis(1_000);

/// The finite accepted client-originated messages.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", deny_unknown_fields)]
pub(crate) enum ClientMessage {
    /// Begin one stream and receive its snapshot.
    #[serde(rename = "floor.subscribe")]
    Subscribe {},
    /// End the stream.
    #[serde(rename = "floor.unsubscribe")]
    Unsubscribe {},
    /// Select one Managed agent's run-local output on this WebSocket.
    #[serde(rename = "floor.output.select")]
    OutputSelect {
        agent_id: AgentId,
        #[serde(default)]
        view: OutputView,
    },
    /// Release the socket's current output selection.
    #[serde(rename = "floor.output.clear")]
    OutputClear {},
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutputView {
    #[default]
    Output,
    Reply,
}

/// Decode one bounded, finite browser message.
pub(crate) fn decode_client(text: &str) -> Result<ClientMessage> {
    decode_object(text)
}

/// Decode one bounded browser message that must be a single JSON object of type `T`.
pub(crate) fn decode_object<T: DeserializeOwned>(text: &str) -> Result<T> {
    if text.len() > MAX_MESSAGE_BYTES {
        bail!("browser message exceeds 1 MiB");
    }
    if json_nesting(text.as_bytes())? > 16 {
        bail!("browser message exceeds JSON nesting 16");
    }
    // The derived decoder also reads a tagged enum from an array led by its tag; only an object is
    // a message the browser wire lists.
    let bytes = text.as_bytes();
    if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        bail!("browser message is not one JSON object");
    }
    serde_json::from_str::<T>(text).context("decoding a finite Floor client message")
}

/// Whether a server discriminator belongs to browser-wire-v3's accepted set.
#[must_use]
pub fn server_type_is_listed(value: &str) -> bool {
    SERVER_TYPES.contains(&value)
}

/// Whether a client discriminator belongs to browser-wire-v3's accepted set.
#[must_use]
pub fn client_type_is_listed(value: &str) -> bool {
    CLIENT_TYPES.contains(&value)
}

#[derive(Debug, Serialize)]
struct SnapshotMessage<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    projection: FloorProjectionView<'a>,
    question_history: &'a [dudes_proto::AnsweredQuestionView],
    traces: &'a [dudes_proto::TurnTraceView],
    messages: &'a [dudes_proto::MessageView],
    messages_lsn: String,
    threads: &'a [dudes_proto::ThreadView],
    threads_lsn: String,
    #[serde(flatten)]
    siblings: Siblings<'a>,
}

/// The optional session-specific safe read siblings one snapshot carries beside `projection`:
/// the onboarding catalog, and each enabled action's view. An absent
/// one is omitted from the frame.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub(crate) struct Siblings<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) onboarding: Option<&'a OnboardingView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) checks: Option<&'a dudes_proto::rpc::floor::ChecksView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) answer: Option<&'a AnswerView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) chat: Option<&'a dudes_proto::ChatView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) control: Option<&'a ControlView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) recovery: Option<dudes_proto::rpc::floor::RecoveryView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) goal: Option<&'a GoalView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) retire: Option<&'a RetireView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) terminal: Option<&'a TerminalView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) git: Option<&'a crate::floor_git::GitSwitch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) diff: Option<&'a crate::floor_diff::DiffSwitch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ticket: Option<&'a crate::floor_ticket::TicketSwitch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) journal_stopped: Option<dudes_proto::rpc::floor::FloorJournalStopped>,
    /// The daemon's count of goals finished on its local day. A caller's value wins over
    /// the one the registry's current snapshot holds, so a new subscription reads the day afresh.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) finished_today: Option<&'a FloorFinishedToday>,
}

impl Siblings<'_> {
    fn is_empty(&self) -> bool {
        self.onboarding.is_none()
            && self.answer.is_none()
            && self.chat.is_none()
            && self.control.is_none()
            && self.recovery.is_none()
            && self.goal.is_none()
            && self.retire.is_none()
            && self.terminal.is_none()
            && self.git.is_none()
            && self.diff.is_none()
            && self.ticket.is_none()
            && self.journal_stopped.is_none()
            && self.finished_today.is_none()
    }
}

/// A server message that is its `type` and nothing else: `{"type":"floor.heartbeat"}`
/// or `{"type":"floor.resync"}`.
///
/// No head `lsn`. The client is forbidden to detect loss by arithmetic on `lsn`
/// (the client may not act on it), so a heartbeat carrying one would be a field with no permitted
/// consumer sitting next to a rule someone has to remember. Omitting it makes the prohibition
/// structural. Adding it later is non-breaking in both directions; removing it would not be.
#[derive(Debug, Serialize)]
struct TypeOnlyMessage {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Debug, Serialize)]
struct DeltaMessage<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    chat: Option<&'a dudes_proto::ChatView>,
    #[serde(rename = "type")]
    kind: &'static str,
    delta: FloorDeltaView<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    question_history: Option<&'a [dudes_proto::AnsweredQuestionView]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    traces: Option<&'a [dudes_proto::TurnTraceView]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    journal_stopped: Option<dudes_proto::rpc::floor::FloorJournalStopped>,
    #[serde(skip_serializing_if = "Option::is_none")]
    finished_today: Option<&'a FloorFinishedToday>,
    #[serde(skip_serializing_if = "Option::is_none")]
    messages: Option<&'a [dudes_proto::MessageView]>,
    messages_lsn: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    threads: Option<&'a [dudes_proto::ThreadView]>,
    threads_lsn: String,
}

#[derive(Debug)]
struct Inner {
    state: Mutex<State>,
    output: Arc<OutputRegistry>,
    fault: FloorStreamFault,
    /// The `daemon` sibling's source, joined on the connection thread.
    daemon: Option<ProviderCheck>,
    /// Whether each engine in the `daemon` sibling is signed in.
    sign_in: Option<SignInCheck>,
    /// Woken by [`end_locked`], so a heartbeat thread parked between beats leaves at once rather
    /// than at its next tick. The `Weak` it also holds covers the other way a registry stops —
    /// being dropped with nobody calling `end` — but that one cannot be signalled, only noticed.
    ended: Condvar,
}

#[derive(Debug)]
struct State {
    current: AgentSnapshot,
    chat: Option<dudes_proto::ChatView>,
    closing: Option<crate::closing::FloorClosing>,
    snapshot: Vec<u8>,
    journal_stopped: Option<dudes_proto::rpc::floor::FloorJournalStopped>,
    subscriptions: Vec<Weak<Queue>>,
    ended: bool,
    /// Asked of the heartbeat thread alone, and never of anything else.
    ///
    /// **The direction of ownership is the point.** A registry that ends takes its timer with it;
    /// a timer that stops must leave the registry exactly as it found it. `HeartbeatTask::shut_down` must
    /// therefore not call `end_locked`: that would invert the ownership, so dropping the timer
    /// would end every subscriber's stream. The two facts are separate flags sharing one lock and
    /// one `Condvar`.
    heartbeat_stopped: bool,
}

/// The daemon-loop publisher and gateway subscriber registry for one run.
#[derive(Clone, Debug)]
pub(crate) struct FloorStreams(Arc<Inner>);

enum ChatReplacement {
    Keep,
    Clear,
    Replace(dudes_proto::ChatView),
}

fn same_closing(
    left: Option<crate::closing::FloorClosing>,
    right: Option<crate::closing::FloorClosing>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left.phase == right.phase && left.same_counts(right),
        _ => false,
    }
}

#[derive(Debug)]
pub(crate) struct FloorPatch {
    changes: FloorDelta,
    questions: Option<Vec<dudes_proto::AnsweredQuestionView>>,
    traces: Option<Vec<dudes_proto::TurnTraceView>>,
    mail: Option<dudes_journal::projections::MailView>,
    finished_today: Option<FloorFinishedToday>,
}

impl FloorPatch {
    pub(crate) fn between(previous: &AgentSnapshot, next: &AgentSnapshot) -> Self {
        Self {
            changes: delta(previous, next),
            questions: (previous.question_history != next.question_history)
                .then(|| next.question_history.clone()),
            traces: (previous.traces != next.traces).then(|| next.traces.clone()),
            mail: (previous.mail.messages != next.mail.messages
                || previous.mail.threads != next.mail.threads)
                .then(|| next.mail.clone()),
            finished_today: next.finished_today.clone(),
        }
    }

    fn apply(self, mut previous: AgentSnapshot) -> AgentSnapshot {
        previous.lsn = self.changes.lsn;
        let mut agents: BTreeMap<_, _> = previous
            .agents
            .into_iter()
            .map(|row| (row.id, row))
            .collect();
        for id in self.changes.removed_agents {
            agents.remove(&id);
        }
        for row in self.changes.agents {
            agents.insert(row.id, row);
        }
        previous.agents = agents.into_values().collect();
        let mut cards: BTreeMap<_, _> = previous
            .cards
            .into_iter()
            .map(|row| (row.id, row))
            .collect();
        for id in self.changes.removed_cards {
            cards.remove(&id);
        }
        for row in self.changes.cards {
            cards.insert(row.id, row);
        }
        previous.cards = cards.into_values().collect();
        if let Some(activity) = self.changes.activity {
            previous.activity = activity;
        }
        if let Some(retired) = self.changes.retired_agents {
            previous.retired_agents = retired;
        }
        if let Some(questions) = self.questions {
            previous.question_history = questions;
        }
        if let Some(traces) = self.traces {
            previous.traces = traces;
        }
        if let Some(mail) = self.mail {
            previous.mail = mail;
        }
        previous.mail.lsn = previous.lsn;
        previous.finished_today = self.finished_today;
        previous
    }
}

impl FloorStreams {
    /// Create the stream at the post-`started()` projection cut.
    pub(crate) fn new(initial: AgentSnapshot, fault: FloorStreamFault) -> Result<Self> {
        Self::with_output(initial, fault, Arc::new(OutputRegistry::new()))
    }

    pub(crate) fn with_output(
        initial: AgentSnapshot,
        fault: FloorStreamFault,
        output: Arc<OutputRegistry>,
    ) -> Result<Self> {
        Self::build(initial, fault, output, None)
    }

    /// The production registry: every fresh snapshot also carries the `daemon` sibling when the
    /// provider check answers in time.
    pub(crate) fn with_daemon_check(
        initial: AgentSnapshot,
        fault: FloorStreamFault,
        output: Arc<OutputRegistry>,
        daemon: ProviderCheck,
        sign_in: SignInCheck,
    ) -> Result<Self> {
        Self::build(initial, fault, output, Some((daemon, sign_in)))
    }

    fn build(
        initial: AgentSnapshot,
        fault: FloorStreamFault,
        output: Arc<OutputRegistry>,
        daemon: Option<(ProviderCheck, SignInCheck)>,
    ) -> Result<Self> {
        let (daemon, sign_in) = daemon.unzip();
        let snapshot = serialize_snapshot(&initial)?;
        Ok(Self(Arc::new(Inner {
            state: Mutex::new(State {
                current: initial,
                chat: None,
                closing: None,
                snapshot,
                journal_stopped: None,
                subscriptions: Vec::new(),
                ended: false,
                heartbeat_stopped: false,
            }),
            output,
            fault,
            daemon,
            sign_in,
            ended: Condvar::new(),
        })))
    }

    pub(crate) fn select_output(
        &self,
        subscription: &Subscription,
        agent: AgentId,
        view: OutputView,
    ) -> Result<()> {
        let floor = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if floor.ended {
            bail!("the Floor stream registry has ended");
        }
        let mut queue = subscription
            .queue
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let epoch = queue
            .selection_epoch
            .checked_add(1)
            .ok_or_else(|| anyhow!("Floor output selection epoch overflow"))?;
        queue.selection_epoch = epoch;
        queue.purge_output();
        queue.selection = None;
        let row = floor.current.agents.iter().find(|row| row.id == agent);
        let unavailable = match row {
            None => Some(
                if matches!(
                    self.0.output.snapshot(agent).state,
                    OutputState::Unavailable(Unavailable::Retired)
                ) {
                    "retired"
                } else {
                    "unknown"
                },
            ),
            Some(row) if row.mode != Mode::Managed => Some("attached"),
            Some(_) => None,
        };
        let frame = if let Some(reason) = unavailable {
            OutputFrame::Unavailable {
                selection_epoch: epoch,
                agent_id: agent,
                reason,
            }
        } else {
            let snapshot = self.0.output.snapshot(agent);
            if view == OutputView::Reply {
                let selection = OutputSelection {
                    agent,
                    epoch,
                    view,
                    revision: snapshot.revision,
                    reply_revision: None,
                    turn: None,
                    first_seq: 0,
                    next_seq: 0,
                    ended: false,
                    unavailable: None,
                    loss: None,
                };
                let sent = refresh_reply(&mut queue, selection, &snapshot);
                subscription.queue.changed.notify_all();
                if !sent {
                    bail!("Floor reply selection queue has ended");
                }
                return Ok(());
            }
            match snapshot.state {
                OutputState::Unavailable(reason) => {
                    if matches!(reason, Unavailable::NoRunOutput | Unavailable::Unbound) {
                        queue.selection = Some(OutputSelection {
                            agent,
                            epoch,
                            view,
                            revision: snapshot.revision,
                            reply_revision: None,
                            turn: None,
                            first_seq: 0,
                            next_seq: 0,
                            ended: false,
                            unavailable: Some(reason),
                            loss: None,
                        });
                    }
                    OutputFrame::Unavailable {
                        selection_epoch: epoch,
                        agent_id: agent,
                        reason: unavailable_reason(reason),
                    }
                }
                OutputState::Ready { .. } | OutputState::Gap { .. } => {
                    let result = Self::select_ready_locked(&mut queue, agent, epoch, snapshot);
                    subscription.queue.changed.notify_all();
                    return result;
                }
            }
        };
        queue.enqueue_frame(Arc::from(serialize_frame(&frame)?), QueueKind::Output)?;
        subscription.queue.changed.notify_all();
        Ok(())
    }

    fn select_ready_locked(
        queue: &mut QueueState,
        agent: AgentId,
        epoch: u64,
        snapshot: crate::output::OutputSnapshot,
    ) -> Result<()> {
        match snapshot.state {
            OutputState::Ready {
                turn,
                first_seq,
                next_seq,
                chunks,
                truncated,
                ended,
                loss,
                ..
            } => {
                let frame = OutputFrame::Snapshot {
                    selection_epoch: epoch,
                    agent_id: agent,
                    turn_id: turn,
                    first_seq,
                    next_seq,
                    chunks: chunks
                        .iter()
                        .map(|chunk| OutputChunkWire {
                            seq: chunk.seq,
                            text: &chunk.text,
                        })
                        .collect(),
                    truncated,
                    ended,
                };
                queue.enqueue_frame(Arc::from(serialize_frame(&frame)?), QueueKind::Output)?;
                queue.selection = Some(OutputSelection {
                    agent,
                    epoch,
                    view: OutputView::Output,
                    revision: snapshot.revision,
                    reply_revision: None,
                    turn: Some(turn),
                    first_seq,
                    next_seq,
                    ended,
                    unavailable: None,
                    loss,
                });
                Ok(())
            }
            OutputState::Gap {
                turn,
                first_seq,
                next_seq,
                chunks,
                truncated,
                ended,
                reason,
                ..
            } => {
                let snapshot = OutputFrame::Snapshot {
                    selection_epoch: epoch,
                    agent_id: agent,
                    turn_id: turn,
                    first_seq,
                    next_seq,
                    chunks: chunks
                        .iter()
                        .map(|chunk| OutputChunkWire {
                            seq: chunk.seq,
                            text: &chunk.text,
                        })
                        .collect(),
                    truncated,
                    ended,
                };
                queue.enqueue_frame(Arc::from(serialize_frame(&snapshot)?), QueueKind::Output)?;
                queue.enqueue_frame(
                    Arc::from(serialize_frame(&OutputFrame::Gap {
                        selection_epoch: epoch,
                        agent_id: agent,
                        turn_id: turn,
                        next_seq,
                        reason: loss_reason(reason),
                    })?),
                    QueueKind::Output,
                )?;
                Ok(())
            }
            OutputState::Unavailable(_) => unreachable!(),
        }
    }

    pub(crate) fn clear_output(&self, subscription: &Subscription) -> Result<()> {
        let _floor = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut queue = subscription
            .queue
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(selection) = queue.selection.take() else {
            bail!("no Floor output selection to clear");
        };
        let epoch = queue
            .selection_epoch
            .checked_add(1)
            .ok_or_else(|| anyhow!("Floor output selection epoch overflow"))?;
        queue.selection_epoch = epoch;
        queue.purge_output();
        queue.enqueue_frame(
            Arc::from(serialize_frame(&OutputFrame::Unavailable {
                selection_epoch: epoch,
                agent_id: selection.agent,
                reason: "cleared",
            })?),
            QueueKind::Output,
        )?;
        subscription.queue.changed.notify_all();
        Ok(())
    }

    pub(crate) fn before_output_drain(&self, subscription: &Subscription) {
        self.0.fault.before_output_drain(&subscription.queue);
    }

    /// Atomically capture the current snapshot and register for every later delta.
    #[cfg(test)]
    pub(crate) fn subscribe(&self) -> Result<Opened> {
        self.finish_subscription(self.subscribe_with_siblings(
            Siblings::default(),
            None,
            dudes_types::Timestamp::UNIX_EPOCH,
        )?)
    }

    /// Capture one projection cut with the optional session-specific safe read siblings.
    ///
    /// `settings` is carried to [`FloorStreams::finish_subscription`] and joined there.
    pub(crate) fn subscribe_with_siblings(
        &self,
        siblings: Siblings<'_>,
        settings: Option<dudes_proto::rpc::floor::SettingsView>,
        now: dudes_types::Timestamp,
    ) -> Result<Opened> {
        let (snapshot, queue) = {
            let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.ended {
                bail!("the Floor stream registry has ended");
            }
            state
                .subscriptions
                .retain(|subscription| subscription.strong_count() > 0);
            let queue = Arc::new(Queue::default());
            state.subscriptions.push(Arc::downgrade(&queue));
            let siblings = Siblings {
                onboarding: siblings.onboarding,
                checks: siblings.checks,
                answer: siblings.answer,
                control: siblings.control,
                recovery: siblings.recovery,
                goal: siblings.goal,
                retire: siblings.retire,
                terminal: siblings.terminal,
                git: siblings.git,
                diff: siblings.diff,
                ticket: siblings.ticket,
                chat: state.chat.as_ref(),
                journal_stopped: state.journal_stopped,
                finished_today: siblings.finished_today,
            };
            let has_activity = state
                .current
                .agents
                .iter()
                .any(|agent| agent.live_activity.is_some());
            let snapshot = if siblings.is_empty() && !has_activity && state.closing.is_none() {
                state.snapshot.clone()
            } else {
                self.0
                    .fault
                    .0
                    .serializations
                    .fetch_add(1, Ordering::Relaxed);
                let mut current = state.current.clone();
                crate::floor::refresh_live_activity(&mut current.agents, now);
                serialize_snapshot_with_closing(
                    &current,
                    siblings,
                    state.closing.map(|mut view| {
                        view.progress = view.progress(now);
                        view
                    }),
                )?
            };
            (snapshot, queue)
        };
        Ok(Opened {
            snapshot,
            settings,
            subscription: Subscription {
                queue,
                watcher: None,
            },
        })
    }

    /// [`FloorStreams::subscribe_with_siblings`] over a day sampled from `days`, and read again
    /// once the subscription is registered.
    ///
    /// The day watch broadcasts while a subscription may not yet be registered, so a stream that
    /// sampled one day and registered after the next began would carry the old one with nothing
    /// left to correct it. Sampling again after registration closes that gap: a day that moved
    /// meanwhile sends this stream the same contentless resync the timer would have sent.
    pub(crate) fn subscribe_on_day(
        &self,
        days: &crate::local_day::LocalDays,
        count: impl FnOnce(Option<&crate::local_day::DayWindow>) -> FloorFinishedToday,
        siblings: Siblings<'_>,
        settings: Option<dudes_proto::rpc::floor::SettingsView>,
        now: dudes_types::Timestamp,
    ) -> Result<Opened> {
        let sampled = days.window();
        let finished = count(sampled.as_ref());
        let opened = self.subscribe_with_siblings(
            Siblings {
                finished_today: Some(&finished),
                ..siblings
            },
            settings,
            now,
        )?;
        if days.window() != sampled {
            let frame = serialize_frame(&TypeOnlyMessage { kind: RESYNC_TYPE })?;
            opened.subscription.queue.enqueue(Arc::from(frame));
        }
        Ok(opened)
    }

    /// Complete a captured subscription off the writer, so a held reader cannot stop appends.
    ///
    /// The `settings` and `daemon` siblings join here, on the connection thread, so the loop
    /// thread still hands over pre-serialized bytes and a check blocked in the filesystem holds
    /// only this connection, and only until its deadline.
    pub(crate) fn finish_subscription(&self, mut opened: Opened) -> Result<Opened> {
        self.0.fault.after_registered(&opened.subscription.queue);
        opened.subscription.watcher =
            Some(start_output_watcher(&self.0, &opened.subscription.queue)?);
        if let Some(joined) = opened
            .settings
            .take()
            .and_then(|settings| self.join_settings(&opened.snapshot, &settings))
        {
            opened.snapshot = joined;
        }
        let busy = self.busy_providers();
        if let Some(joined) = self
            .0
            .daemon
            .as_ref()
            .and_then(|daemon| daemon.sibling(self.0.sign_in.as_ref(), &busy))
            .and_then(|daemon| with_sibling(&opened.snapshot, "daemon", &daemon))
        {
            opened.snapshot = joined;
        }
        Ok(opened)
    }

    /// The engines with an agent whose turn is running or whose keyboard a person holds, so a
    /// status command stays away from their credentials.
    fn busy_providers(&self) -> std::collections::BTreeSet<dudes_types::Provider> {
        let state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .current
            .agents
            .iter()
            .filter(|agent| {
                matches!(
                    agent.status,
                    dudes_types::AgentStatus::Running | dudes_types::AgentStatus::Attached
                )
            })
            .map(|agent| agent.provider)
            .collect()
    }

    fn join_settings(
        &self,
        snapshot: &[u8],
        settings: &dudes_proto::rpc::floor::SettingsView,
    ) -> Option<Vec<u8>> {
        self.0.fault.settings_joined();
        with_sibling(
            snapshot,
            "settings",
            &crate::floor_settings::serialize(settings)?,
        )
    }

    /// Publish one complete post-fold agent delta without waiting on a browser.
    #[cfg(test)]
    pub(crate) fn publish(&self, next: AgentSnapshot) -> Result<()> {
        self.publish_with_output(next, || {})
    }

    /// Commit an infallible output notice with an already validated Floor publication.
    #[cfg(test)]
    pub(crate) fn publish_with_output(
        &self,
        next: AgentSnapshot,
        output_notice: impl FnOnce(),
    ) -> Result<()> {
        self.publish_closing_with_output(next, None, output_notice)
    }

    pub(crate) fn publish_closing_with_output(
        &self,
        next: AgentSnapshot,
        closing: Option<crate::closing::FloorClosing>,
        output_notice: impl FnOnce(),
    ) -> Result<()> {
        self.publish_chat(next, closing, ChatReplacement::Keep, output_notice)
    }

    pub(crate) fn patch(&self, next: &AgentSnapshot) -> FloorPatch {
        let state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        FloorPatch::between(&state.current, next)
    }

    pub(crate) fn publish_patch(
        &self,
        patch: FloorPatch,
        closing: Option<crate::closing::FloorClosing>,
        output_notice: impl FnOnce(),
    ) -> Result<()> {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        let next = patch.apply(state.current.clone());
        self.publish_locked(
            &mut state,
            next,
            closing,
            ChatReplacement::Keep,
            output_notice,
        )
    }

    pub(crate) fn publish_closing_with_chat_output(
        &self,
        next: AgentSnapshot,
        closing: Option<crate::closing::FloorClosing>,
        chat: Option<dudes_proto::ChatView>,
        output_notice: impl FnOnce(),
    ) -> Result<()> {
        let replacement = match chat {
            Some(view) => ChatReplacement::Replace(view),
            None => ChatReplacement::Clear,
        };
        self.publish_chat(next, closing, replacement, output_notice)
    }

    fn publish_chat(
        &self,
        next: AgentSnapshot,
        closing: Option<crate::closing::FloorClosing>,
        replacement: ChatReplacement,
        output_notice: impl FnOnce(),
    ) -> Result<()> {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.publish_locked(&mut state, next, closing, replacement, output_notice)
    }

    fn publish_locked(
        &self,
        state: &mut State,
        next: AgentSnapshot,
        closing: Option<crate::closing::FloorClosing>,
        replacement: ChatReplacement,
        output_notice: impl FnOnce(),
    ) -> Result<()> {
        if state.ended {
            bail!("the Floor stream registry has ended");
        }
        let (mut chat, replace_chat) = match replacement {
            ChatReplacement::Keep => (state.chat.clone(), false),
            ChatReplacement::Clear => (None, true),
            ChatReplacement::Replace(view) => (Some(view), true),
        };
        if !replace_chat && let Some(chat) = &mut chat {
            crate::floor_chat::retain_current_agents(chat, &next.agents);
        }
        if state.current == next && same_closing(state.closing, closing) && state.chat == chat {
            output_notice();
            return Ok(());
        }
        if let Some(chat) = &chat {
            crate::floor_chat::validate_view(chat)?;
            crate::floor_chat::validate_current_agents(chat, &next.agents)?;
        }
        let mut delta = delta(&state.current, &next);
        if state.closing.map(|view| view.phase) != closing.map(|view| view.phase) {
            delta.closing_time = closing.map(|view| view.phase);
        }
        let progress = (!same_closing(state.closing, closing))
            .then(|| closing.map(|view| view.progress))
            .flatten();
        let frame = match serialize_frame(&DeltaMessage {
            chat: (state.chat != chat).then_some(chat.as_ref()).flatten(),
            kind: "floor.delta",
            delta: FloorDeltaView {
                delta: &delta,
                closing_progress: progress,
            },
            question_history: (state.current.question_history != next.question_history)
                .then_some(next.question_history.as_slice()),
            traces: (state.current.traces != next.traces).then_some(next.traces.as_slice()),
            journal_stopped: state.journal_stopped,
            finished_today: finished_today_changed(
                state.current.finished_today.as_ref(),
                next.finished_today.as_ref(),
            )
            .then_some(next.finished_today.as_ref())
            .flatten(),
            messages: (state.current.mail.messages != next.mail.messages
                || state.current.mail.threads != next.mail.threads)
                .then_some(next.mail.messages.as_slice()),
            threads: (state.current.mail.messages != next.mail.messages
                || state.current.mail.threads != next.mail.threads)
                .then_some(next.mail.threads.as_slice()),
            threads_lsn: next.lsn.0.to_string(),
            messages_lsn: next.lsn.0.to_string(),
        }) {
            Ok(frame) => Arc::<[u8]>::from(frame),
            Err(error) => {
                end_locked(state, &self.0.ended);
                return Err(error);
            }
        };
        let snapshot = match serialize_snapshot_with_closing(
            &next,
            Siblings {
                journal_stopped: state.journal_stopped,
                chat: chat.as_ref(),
                ..Siblings::default()
            },
            closing,
        ) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                end_locked(state, &self.0.ended);
                return Err(error);
            }
        };
        // Both serializations have succeeded. Keep the Floor cut locked across the output
        // notice, current view, and delta queueing so a selector cannot see a new row with an
        // old output ring. The caller has precomputed every fallible projection predicate.
        output_notice();
        state.current = next;
        state.chat = chat;
        state.closing = closing;
        state.snapshot = snapshot;
        state.subscriptions.retain(|subscriber| {
            subscriber.upgrade().is_some_and(|queue| {
                let mut queued = queue.state.lock().unwrap_or_else(PoisonError::into_inner);
                let sent = queued
                    .enqueue_frame(Arc::clone(&frame), QueueKind::Projection)
                    .is_ok();
                queue.changed.notify_all();
                sent
            })
        });
        Ok(())
    }

    pub(crate) fn publish_journal_stopped(&self, lsn: dudes_types::Lsn) -> Result<()> {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.ended || state.journal_stopped.is_some() {
            return Ok(());
        }
        let stopped = dudes_proto::rpc::floor::FloorJournalStopped { lsn };
        let frame = Arc::<[u8]>::from(serialize_frame(&DeltaMessage {
            chat: None,
            kind: "floor.delta",
            delta: FloorDeltaView {
                delta: &delta(&state.current, &state.current),
                closing_progress: None,
            },
            question_history: None,
            traces: None,
            journal_stopped: Some(stopped),
            finished_today: None,
            messages: None,
            threads: None,
            threads_lsn: state.current.lsn.0.to_string(),
            messages_lsn: state.current.lsn.0.to_string(),
        })?);
        let snapshot = serialize_snapshot_with_closing(
            &state.current,
            Siblings {
                chat: state.chat.as_ref(),
                journal_stopped: Some(stopped),
                ..Siblings::default()
            },
            state.closing,
        )?;
        state.journal_stopped = Some(stopped);
        state.snapshot = snapshot;
        state.subscriptions.retain(|subscriber| {
            subscriber
                .upgrade()
                .is_some_and(|queue| queue.enqueue(Arc::clone(&frame)))
        });
        Ok(())
    }

    /// Enqueue one `floor.heartbeat` to every live subscriber, and answer whether any got it.
    ///
    /// **Three properties, each of them falsifiable, and each of them part of the heartbeat contract** (see the
    /// tests at the foot of this file):
    ///
    /// 1. **No live subscriber, no frame.** A registry nobody is reading enqueues nothing. Dead
    ///    `Weak` entries are *pruned* on the way past, exactly as `subscribe` and `publish` prune
    ///    them — so the assertion that catches a broken emitter is on the live count, never on the
    ///    vector being unchanged, which a correctly pruning emitter would fail.
    /// 2. **It goes through [`Queue::enqueue`] like any other frame.** That is what makes the
    ///    pruning above happen and what makes the queue bound real for an idle connection: a
    ///    subscriber that stays registered and never drains fills `MAX_QUEUE_MESSAGES` on
    ///    heartbeats alone and is then closed. That is the right direction: a wedged reader is released
    ///    rather than held for ever.
    /// 3. **It is not on the journal-append path.** Nothing in `runtime.rs`'s post-fsync closure
    ///    calls this; a keepalive coupled to the very thing whose absence it exists to cover would
    ///    answer only when it was not needed.
    ///
    /// Returns the number of live subscribers the frame reached, which is what the no-subscriber
    /// test asserts on.
    ///
    /// Test-only, and that is the honest shape: production beats through the timer thread and
    /// never by hand, so a non-test build has no caller for this and should not carry one.
    #[cfg(test)]
    pub(crate) fn heartbeat(&self) -> usize {
        beat(&self.0)
    }

    /// Start the timer that beats this registry while anything is subscribed to it.
    ///
    /// `period` is a parameter and [`HEARTBEAT_PERIOD`] is what ships. The shipped number is fixed
    /// against the client's staleness threshold; it says nothing about what a test
    /// may pass, and two of this file's tests cannot run at two seconds — 256 beats is eight and a
    /// half minutes.
    ///
    /// The thread holds a [`Weak`], never an `Arc`: a timer that kept its own registry alive would
    /// be a registry that never drops, which is the opposite of what a heartbeat's lifetime requires.
    pub(crate) fn start_heartbeat(&self, period: Duration) -> HeartbeatTask {
        self.start_timer(
            "dudes-floor-heartbeat",
            period,
            self.0.fault.beats(),
            |inner| {
                beat(inner);
            },
        )
    }

    /// Start the timer that tells every open stream to subscribe again when the daemon's local day
    /// changes.
    ///
    /// Each tick samples the day, and a change in its date, its window bounds or its offset sends
    /// the existing contentless `floor.resync` once. The sample taken here is the one the first tick
    /// compares with, so a repeated observation of the same day sends nothing. This covers midnight,
    /// a daylight-saving shift, a clock jump, wake from sleep and a host zone change alike, and it
    /// journals nothing and fabricates no position.
    pub(crate) fn start_day_watch(
        &self,
        days: crate::local_day::LocalDays,
        period: Duration,
    ) -> HeartbeatTask {
        let mut seen = days.window();
        self.start_timer("dudes-floor-day-watch", period, true, move |inner| {
            let now = days.window();
            if now != seen {
                seen = now;
                broadcast(inner, RESYNC_TYPE);
            }
        })
    }

    fn start_timer(
        &self,
        name: &str,
        period: Duration,
        enabled: bool,
        mut tick: impl FnMut(&Inner) + Send + 'static,
    ) -> HeartbeatTask {
        let inner = Arc::downgrade(&self.0);
        if !enabled {
            // No thread at all rather than one that ticks and does nothing: `stop` and `Drop` both
            // handle the absent handle, and there is nothing to be wrong about.
            return HeartbeatTask {
                handle: None,
                alive: Arc::new(()),
                inner,
            };
        }
        let alive = Arc::new(());
        let held = Arc::clone(&alive);
        let watched = Weak::clone(&inner);
        let handle = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                // Moved in so the thread's own frame owns it, and dropped when this closure
                // returns — which is what makes a strong count taken elsewhere answer "is that
                // frame still on the stack" without asking the thread.
                let alive_while_running = held;
                loop {
                    let Some(inner) = watched.upgrade() else {
                        // The registry dropped without anyone ending it.
                        break;
                    };
                    let state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
                    let (state, timed_out) = inner
                        .ended
                        .wait_timeout_while(state, period, |state| {
                            !state.ended && !state.heartbeat_stopped
                        })
                        .unwrap_or_else(PoisonError::into_inner);
                    if state.ended || state.heartbeat_stopped {
                        break;
                    }
                    drop(state);
                    if timed_out.timed_out() {
                        // The lock is released first: `heartbeat` takes it again, and holding it
                        // across the call would deadlock rather than beat.
                        tick(&inner);
                    }
                    // Dropped before the next park so the upgrade above is a real question.
                    drop(inner);
                }
                drop(alive_while_running);
            })
            .expect("a thread for the Floor heartbeat");
        HeartbeatTask {
            handle: Some(handle),
            alive,
            inner,
        }
    }

    /// Tell every open stream to drop its view and subscribe again. The stream stays
    /// open; the page closes it and takes a fresh snapshot within the same session.
    pub(crate) fn resync(&self) -> usize {
        broadcast(&self.0, RESYNC_TYPE)
    }

    pub(crate) fn is_ended(&self) -> bool {
        self.0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .ended
    }

    /// End every current epoch before the listener is released.
    pub(crate) fn end(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        // Notified inside `end_locked`, before the lock is dropped: the heartbeat thread re-checks
        // `ended` under this same lock, so a notification that raced ahead of the flag would be one
        // the thread slept through.
        end_locked(&mut state, &self.0.ended);
    }
}

const fn loss_reason(reason: Loss) -> &'static str {
    match reason {
        Loss::PendingOverflow => "pending_overflow",
        Loss::RingTruncation => "ring_truncation",
        Loss::GlobalEviction => "global_eviction",
        Loss::SequenceOverflow => "sequence_overflow",
    }
}

fn start_output_watcher(
    inner: &Arc<Inner>,
    queue: &Arc<Queue>,
) -> Result<std::thread::JoinHandle<()>> {
    let floor = Arc::downgrade(inner);
    let selected = Arc::downgrade(queue);
    let output = Arc::clone(&inner.output);
    std::thread::Builder::new()
        .name("dudes-floor-output".to_owned())
        .spawn(move || {
            while let Some(queue) = selected.upgrade() {
                let state = queue.state.lock().unwrap_or_else(PoisonError::into_inner);
                let (state, _) = queue
                    .changed
                    .wait_timeout_while(state, Duration::from_millis(100), |state| {
                        state.terminal == LIVE && state.selection.is_none()
                    })
                    .unwrap_or_else(PoisonError::into_inner);
                if state.terminal != LIVE {
                    break;
                }
                let Some(selection) = state.selection else {
                    continue;
                };
                drop(state);
                drop(queue);

                // This result only wakes the reader. It must not be queued: a publication may
                // replace the turn while we wait to acquire the Floor state lock below.
                let _hint = output.wait_changed(
                    selection.agent,
                    selection.revision,
                    Duration::from_millis(100),
                );
                // A Floor projection alone may change a selected agent's mode or lifetime, so a
                // timeout still revalidates the target even without an output-registry revision.
                let (Some(inner), Some(queue)) = (floor.upgrade(), selected.upgrade()) else {
                    break;
                };
                inner.fault.after_output_wake(selection.agent);
                if !refresh_output(&inner, &queue, selection.agent, selection.epoch) {
                    break;
                }
                inner.fault.after_output_refresh();
            }
        })
        .context("spawning a bounded Floor output watcher")
}

fn enqueue_output(queue: &mut QueueState, frame: &OutputFrame<'_>) -> bool {
    let Ok(frame) = serialize_frame(frame) else {
        terminate(queue, ENDED);
        return false;
    };
    queue
        .enqueue_frame(Arc::from(frame), QueueKind::Output)
        .is_ok()
}

fn gap_output(
    queue: &mut QueueState,
    selection: OutputSelection,
    turn: TurnId,
    next_seq: u64,
    reason: &str,
) -> bool {
    let sent = enqueue_output(
        queue,
        &OutputFrame::Gap {
            selection_epoch: selection.epoch,
            agent_id: selection.agent,
            turn_id: turn,
            next_seq,
            reason,
        },
    );
    queue.selection = None;
    sent
}

fn refresh_reply(
    queue: &mut QueueState,
    mut selection: OutputSelection,
    snapshot: &OutputSnapshot,
) -> bool {
    selection.revision = snapshot.revision;
    if let Some(reply) = &snapshot.reply {
        if selection.turn != Some(reply.turn) {
            queue.purge_output();
        } else if selection.reply_revision == Some(reply.revision) {
            queue.selection = Some(selection);
            return true;
        }
        let gap = reply.loss.map(|loss| match loss {
            ReplyLoss::ProviderLimit => "provider_limit",
            ReplyLoss::PendingOverflow => "pending_overflow",
            ReplyLoss::ReplyTruncation => "reply_truncation",
            ReplyLoss::GlobalEviction => "global_eviction",
            ReplyLoss::SequenceOverflow => "sequence_overflow",
        });
        if !enqueue_output(
            queue,
            &OutputFrame::Reply {
                selection_epoch: selection.epoch,
                agent_id: reply.agent,
                turn_id: reply.turn,
                revision: reply.revision.to_string(),
                text: &reply.text,
                truncated: reply.loss.is_some(),
                gap,
                ended: reply.ended,
            },
        ) {
            return false;
        }
        selection.turn = Some(reply.turn);
        selection.reply_revision = Some(reply.revision);
        selection.ended = reply.ended;
        selection.unavailable = None;
    } else {
        let reason = match snapshot.state {
            OutputState::Unavailable(reason) => reason,
            _ => Unavailable::NoRunOutput,
        };
        if selection.turn.is_some() || selection.unavailable != Some(reason) {
            queue.purge_output();
            if !enqueue_output(
                queue,
                &OutputFrame::Unavailable {
                    selection_epoch: selection.epoch,
                    agent_id: selection.agent,
                    reason: unavailable_reason(reason),
                },
            ) {
                return false;
            }
        }
        if matches!(reason, Unavailable::Retired | Unavailable::Evicted) {
            queue.selection = None;
            return true;
        }
        selection.turn = None;
        selection.reply_revision = None;
        selection.unavailable = Some(reason);
    }
    queue.selection = Some(selection);
    true
}

/// Return false when the socket queue has ended and its watcher should leave.
#[expect(
    clippy::too_many_lines,
    reason = "one ordered selected-output transition owns the queue cursor and terminal paths"
)]
fn refresh_output(inner: &Inner, queue: &Queue, agent: AgentId, epoch: u64) -> bool {
    let floor = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
    let mut state = queue.state.lock().unwrap_or_else(PoisonError::into_inner);
    if floor.ended || state.terminal != LIVE {
        return false;
    }
    let Some(mut selection) = state
        .selection
        .filter(|s| s.agent == agent && s.epoch == epoch)
    else {
        return true;
    };
    let row = floor.current.agents.iter().find(|row| row.id == agent);
    if row.is_none_or(|row| row.mode != Mode::Managed) {
        state.purge_output();
        state.selection = None;
        let reason = if row.is_some() { "attached" } else { "retired" };
        let sent = enqueue_output(
            &mut state,
            &OutputFrame::Unavailable {
                selection_epoch: epoch,
                agent_id: agent,
                reason,
            },
        );
        queue.changed.notify_all();
        return sent;
    }
    // This read is the authoritative one. A prior wait_changed snapshot is only a hint.
    let snapshot = inner.output.snapshot(agent);
    if selection.view == OutputView::Reply {
        let sent = refresh_reply(&mut state, selection, &snapshot);
        queue.changed.notify_all();
        return sent;
    }
    selection.revision = snapshot.revision;
    let sent = match snapshot.state {
        OutputState::Unavailable(reason) => {
            if reason == Unavailable::Evicted
                && let Some(turn) = selection.turn
            {
                gap_output(
                    &mut state,
                    selection,
                    turn,
                    selection.next_seq,
                    "global_eviction",
                )
            } else {
                let changed = selection.turn.is_some() || selection.unavailable != Some(reason);
                if changed {
                    state.purge_output();
                    if !enqueue_output(
                        &mut state,
                        &OutputFrame::Unavailable {
                            selection_epoch: epoch,
                            agent_id: agent,
                            reason: unavailable_reason(reason),
                        },
                    ) {
                        return false;
                    }
                }
                if matches!(reason, Unavailable::Evicted | Unavailable::Retired) {
                    state.selection = None;
                } else {
                    selection.turn = None;
                    selection.first_seq = 0;
                    selection.next_seq = 0;
                    selection.ended = false;
                    selection.unavailable = Some(reason);
                    selection.loss = None;
                    state.selection = Some(selection);
                }
                true
            }
        }
        OutputState::Ready {
            turn,
            first_seq,
            next_seq,
            chunks,
            truncated,
            ended,
            loss,
            ..
        } => {
            if selection.turn != Some(turn) {
                state.purge_output();
                let frame = OutputFrame::Snapshot {
                    selection_epoch: epoch,
                    agent_id: agent,
                    turn_id: turn,
                    first_seq,
                    next_seq,
                    chunks: chunks
                        .iter()
                        .map(|chunk| OutputChunkWire {
                            seq: chunk.seq,
                            text: &chunk.text,
                        })
                        .collect(),
                    truncated,
                    ended,
                };
                if !enqueue_output(&mut state, &frame) {
                    return false;
                }
                if truncated && let Some(reason) = loss {
                    gap_output(&mut state, selection, turn, next_seq, loss_reason(reason))
                } else {
                    selection.turn = Some(turn);
                    selection.first_seq = first_seq;
                    selection.next_seq = next_seq;
                    selection.ended = ended;
                    selection.unavailable = None;
                    selection.loss = loss;
                    state.selection = Some(selection);
                    true
                }
            } else if first_seq > selection.next_seq
                || next_seq < selection.next_seq
                || (loss == Some(Loss::GlobalEviction) && first_seq > selection.first_seq)
            {
                gap_output(
                    &mut state,
                    selection,
                    turn,
                    next_seq,
                    loss.map_or("unexpected_sequence", loss_reason),
                )
            } else {
                let mut contiguous = true;
                for chunk in &chunks {
                    if chunk.seq < selection.next_seq {
                        continue;
                    }
                    if chunk.seq != selection.next_seq || selection.ended {
                        contiguous = false;
                        break;
                    }
                    if !enqueue_output(
                        &mut state,
                        &OutputFrame::Chunk {
                            selection_epoch: epoch,
                            agent_id: agent,
                            turn_id: turn,
                            seq: chunk.seq,
                            text: &chunk.text,
                        },
                    ) {
                        return false;
                    }
                    selection.next_seq = if let Some(next) = selection.next_seq.checked_add(1) {
                        next
                    } else {
                        contiguous = false;
                        break;
                    };
                }
                if !contiguous || selection.next_seq != next_seq {
                    gap_output(&mut state, selection, turn, next_seq, "unexpected_sequence")
                } else {
                    if ended
                        && !selection.ended
                        && !enqueue_output(
                            &mut state,
                            &OutputFrame::Ended {
                                selection_epoch: epoch,
                                agent_id: agent,
                                turn_id: turn,
                            },
                        )
                    {
                        return false;
                    }
                    selection.first_seq = first_seq;
                    selection.ended = ended;
                    selection.loss = loss;
                    state.selection = Some(selection);
                    true
                }
            }
        }
        OutputState::Gap {
            turn,
            first_seq,
            next_seq,
            chunks,
            truncated,
            ended,
            reason,
            ..
        } => {
            if selection.turn != Some(turn) {
                state.purge_output();
                let frame = OutputFrame::Snapshot {
                    selection_epoch: epoch,
                    agent_id: agent,
                    turn_id: turn,
                    first_seq,
                    next_seq,
                    chunks: chunks
                        .iter()
                        .map(|chunk| OutputChunkWire {
                            seq: chunk.seq,
                            text: &chunk.text,
                        })
                        .collect(),
                    truncated,
                    ended,
                };
                if !enqueue_output(&mut state, &frame) {
                    return false;
                }
            }
            gap_output(&mut state, selection, turn, next_seq, loss_reason(reason))
        }
    };
    queue.changed.notify_all();
    sent
}

const fn unavailable_reason(reason: Unavailable) -> &'static str {
    match reason {
        Unavailable::NoRunOutput => "no_run_output",
        Unavailable::Unbound => "unbound",
        Unavailable::Evicted => "evicted",
        Unavailable::Retired => "retired",
    }
}

/// One beat against a registry, shared by [`FloorStreams::heartbeat`] and the timer thread.
///
/// A free function because the thread holds an `Arc<Inner>` rather than a `FloorStreams` — it must
/// not hold the latter, which would keep the registry alive and defeat the `Weak` the whole
/// stopping story rests on.
fn beat(inner: &Inner) -> usize {
    broadcast(inner, HEARTBEAT_TYPE)
}

/// Enqueue one frame that is only its `type` to every live subscriber, and count who got it.
fn broadcast(inner: &Inner, kind: &'static str) -> usize {
    let mut state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
    if state.ended {
        return 0;
    }
    // Serialized per beat rather than once and cached: the frame is about thirty bytes, and a
    // cached `Arc<[u8]>` would be one more thing to keep in step with `HEARTBEAT_TYPE`.
    let Ok(frame) = serialize_frame(&TypeOnlyMessage { kind }) else {
        // Unreachable for a struct with one `&'static str`, and handled rather than unwrapped
        // because `publish` ends the registry on a serialization failure, and a heartbeat that
        // panicked instead would take the daemon down for the one frame that carries nothing.
        end_locked(&mut state, &inner.ended);
        return 0;
    };
    let frame = Arc::<[u8]>::from(frame);
    let mut reached = 0;
    state.subscriptions.retain(|subscriber| {
        subscriber.upgrade().is_some_and(|queue| {
            let sent = queue.enqueue(Arc::clone(&frame));
            reached += usize::from(sent);
            sent
        })
    });
    reached
}

/// End every queue and wake anything parked on the registry's end.
///
/// Takes the `Condvar` rather than leaving the notification to each caller, because there are four
/// call sites — two of them error paths inside `publish` — and a heartbeat thread that slept
/// through one of them is exactly the failure of a timer that looks cancelled but is still
/// scheduled: a heartbeat outliving its stream. Passing it makes forgetting a compile error rather than a review item.
fn end_locked(state: &mut State, ended: &Condvar) {
    state.ended = true;
    for queue in state
        .subscriptions
        .drain(..)
        .filter_map(|queue| queue.upgrade())
    {
        queue.end();
    }
    ended.notify_all();
}

/// The running heartbeat timer, owned by whoever owns the [`FloorStreams`] it beats for.
///
/// # Why this is a value and not a detached thread
///
/// The implementation has to keep a distinction: a tick arriving after the
/// registry ended enqueues to nobody, but *that is the per-tick no-op, not the task stopping*, and
/// a timer that looks cancelled but is still scheduled is how a heartbeat outlives the stream it
/// claims to speak for. So stopping is observable here rather than assumed: [`Self::stop`] joins,
/// and a join that returns is a thread that has exited.
///
/// [`FloorStreams`] is `Clone` and lives in two places at once — the daemon context and the floor
/// gateway — so the `JoinHandle` cannot live inside it. It lives here, in one owner, beside the
/// gateway in `boot`.
///
/// # Two ways it stops, because there are two ways a registry does
///
/// `end()` is reached from one path today (`runtime.rs`'s post-fsync error arm), and on an ordinary
/// shutdown nobody calls it at all — the registry is simply dropped. A timer keyed only to `end()`
/// would outlive every clean shutdown, so the thread watches both:
///
/// * the registry **ended**: `end_locked` notifies the condvar, and the thread leaves at once
///   rather than at its next tick;
/// * the registry **dropped**: the thread holds a [`Weak`], and an upgrade that fails is the last
///   owner having gone. Nothing can notify this one, so it is noticed on the next tick — bounded by
///   one period, and reached only when no one called `end` first.
pub(crate) struct HeartbeatTask {
    handle: Option<std::thread::JoinHandle<()>>,
    /// Held by the thread and by nothing else, so a `Weak` to it answers "is that thread's frame
    /// still alive" without asking the thread.
    alive: Arc<()>,
    /// Signalled so `stop` can wake a thread parked between beats even when the registry has not
    /// ended — the shutdown order this daemon actually uses.
    inner: Weak<Inner>,
}

impl HeartbeatTask {
    /// Stop the timer and wait for the thread to be gone.
    ///
    /// Returns once the thread has exited.
    ///
    /// Test-only, because production stops this the way it stops everything else on that path — by
    /// dropping the daemon — and [`Drop`] below does exactly what this does. What a test gets from
    /// calling it is a *moment*: somewhere to stand between "running" and "gone" so it can assert
    /// the second.
    #[cfg(test)]
    pub(crate) fn stop(mut self) {
        self.shut_down();
    }

    /// Whether the thread's own frame is still alive, for a test that wants an observation the
    /// join API is not part of.
    #[cfg(test)]
    pub(crate) fn alive_probe(&self) -> Weak<()> {
        Arc::downgrade(&self.alive)
    }

    /// Ask the thread to leave, then wait until it has.
    ///
    /// **It sets the timer's flag and not the registry's.** Ending the stream is the registry's
    /// decision and ending the timer is this value's; a stop that took the whole floor down with
    /// it would invert the ownership split above, and every subscriber would lose its
    /// stream because a daemon tore down its keepalive.
    ///
    /// Returns well inside one period even when the period is long: the thread parks on the
    /// `Condvar` this notifies rather than sleeping, so the wait is the join and not the tick.
    /// `stop_returns_without_waiting_out_a_period` measures that against an hour-long period.
    fn shut_down(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            let mut state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.heartbeat_stopped = true;
            // Under the lock, so a thread between its check and its park cannot miss it.
            inner.ended.notify_all();
        }
        if let Some(handle) = self.handle.take() {
            // A panicked heartbeat thread is still a stopped one, which is what this waits for.
            let _ = handle.join();
        }
    }
}

impl Drop for HeartbeatTask {
    fn drop(&mut self) {
        self.shut_down();
    }
}

impl std::fmt::Debug for HeartbeatTask {
    /// Hand-written because a `JoinHandle` prints nothing a reader of a trace wants, and whether
    /// the thread is still running is the only thing about this value worth printing.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeartbeatTask")
            .field("joined", &self.handle.is_none())
            // The thread holds the second strong reference for exactly as long as its closure is
            // on the stack, so this answers "has that frame gone" without asking the thread.
            .field("thread_alive", &(Arc::strong_count(&self.alive) > 1))
            // Not the pointer, which says nothing a reader wants: whether the registry this beats
            // for is still there at all.
            .field("registry", &self.inner.upgrade().is_some())
            .finish()
    }
}

/// The snapshot and queue acquired in one registry cut.
pub struct Opened {
    pub(crate) snapshot: Vec<u8>,
    settings: Option<dudes_proto::rpc::floor::SettingsView>,
    pub(crate) subscription: Subscription,
}

#[derive(Debug, Default)]
struct Queue {
    state: Mutex<QueueState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct QueueState {
    frames: VecDeque<QueuedFrame>,
    bytes: usize,
    terminal: u8,
    selection_epoch: u64,
    selection: Option<OutputSelection>,
}

#[derive(Debug)]
struct QueuedFrame {
    bytes: Arc<[u8]>,
    kind: QueueKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueueKind {
    Projection,
    Output,
}

#[derive(Clone, Copy, Debug)]
struct OutputSelection {
    agent: AgentId,
    epoch: u64,
    view: OutputView,
    revision: u128,
    reply_revision: Option<u128>,
    turn: Option<TurnId>,
    first_seq: u64,
    next_seq: u64,
    ended: bool,
    unavailable: Option<Unavailable>,
    loss: Option<Loss>,
}

impl QueueState {
    fn enqueue_frame(&mut self, frame: Arc<[u8]>, kind: QueueKind) -> Result<()> {
        if self.terminal != LIVE {
            bail!("Floor queue has ended");
        }
        let Some(next_bytes) = self.bytes.checked_add(frame.len()) else {
            terminate(self, OVERFLOWED);
            bail!("Floor queue byte count overflow");
        };
        if self.frames.len() >= MAX_QUEUE_MESSAGES || next_bytes > MAX_QUEUE_BYTES {
            terminate(self, OVERFLOWED);
            bail!("Floor queue overflow");
        }
        self.bytes = next_bytes;
        self.frames.push_back(QueuedFrame { bytes: frame, kind });
        Ok(())
    }

    fn purge_output(&mut self) {
        self.frames.retain(|frame| {
            if frame.kind == QueueKind::Output {
                self.bytes -= frame.bytes.len();
                false
            } else {
                true
            }
        });
    }
}

impl Queue {
    fn enqueue(&self, frame: Arc<[u8]>) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let result = state.enqueue_frame(frame, QueueKind::Projection).is_ok();
        self.changed.notify_all();
        result
    }

    fn end(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        terminate(&mut state, ENDED);
        self.changed.notify_all();
    }

    fn next(&self, timeout: Duration) -> QueueRead {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| {
                state.terminal == LIVE && state.frames.is_empty()
            })
            .unwrap_or_else(PoisonError::into_inner);
        match state.terminal {
            OVERFLOWED => QueueRead::Overflowed,
            ENDED => QueueRead::Ended,
            _ => state.frames.pop_front().map_or(QueueRead::Empty, |frame| {
                state.bytes = state
                    .bytes
                    .checked_sub(frame.bytes.len())
                    .expect("queued byte accounting matches its frames");
                QueueRead::Frame(frame.bytes)
            }),
        }
    }

    fn usage(&self) -> (usize, usize, bool) {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        (
            state.frames.len(),
            state.bytes,
            state.terminal == OVERFLOWED,
        )
    }
}

fn terminate(state: &mut QueueState, terminal: u8) {
    state.terminal = terminal;
    state.frames.clear();
    state.bytes = 0;
}

/// One connection-local subscription epoch.
pub(crate) struct Subscription {
    queue: Arc<Queue>,
    watcher: Option<std::thread::JoinHandle<()>>,
}

impl Subscription {
    /// Wait for one frame or the stream's terminal state.
    pub(crate) fn next(&self, timeout: Duration) -> QueueRead {
        self.queue.next(timeout)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.queue.end();
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

/// What one bounded queue wait observed.
pub(crate) enum QueueRead {
    Frame(Arc<[u8]>),
    Empty,
    Overflowed,
    Ended,
}

/// Default-inert subscription ordering barrier for production-path integration tests.
#[doc(hidden)]
#[derive(Clone, Debug, Default)]
pub struct FloorStreamFault(Arc<FaultInner>);

#[derive(Debug, Default)]
struct FaultInner {
    state: Mutex<FaultState>,
    changed: Condvar,
    /// Projections the loop thread serialized for a subscription.
    serializations: AtomicUsize,
    /// The name of the thread that last joined a `settings` sibling.
    settings_joiner: Mutex<Option<String>>,
    /// A zone and clock a test substitutes for the operating system's.
    local_days: Mutex<Option<crate::local_day::LocalDays>>,
}

#[derive(Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent, test-only one-shot holds use this existing ordering controller"
)]
struct FaultState {
    armed: bool,
    registered: bool,
    queue: Option<Weak<Queue>>,
    /// Whether this registry beats at all. `true` is the shipped answer.
    heartbeat: bool,
    hold_output_wake: bool,
    output_wake_agent: Option<AgentId>,
    output_wake_held: bool,
    hold_output_refresh: bool,
    output_refresh_held: bool,
    hold_output_drain: bool,
    output_drain_held: bool,
    pause_output_pass: bool,
    output_pass_agent: Option<AgentId>,
    output_pass_paused: bool,
}

impl Default for FaultState {
    /// Every field's `Default` except `heartbeat`, which is on unless a test turns it off — the
    /// shipped behaviour has to be what you get by saying nothing.
    fn default() -> Self {
        Self {
            armed: false,
            registered: false,
            queue: None,
            heartbeat: true,
            hold_output_wake: false,
            output_wake_agent: None,
            output_wake_held: false,
            hold_output_refresh: false,
            output_refresh_held: false,
            hold_output_drain: false,
            output_drain_held: false,
            pause_output_pass: false,
            output_pass_agent: None,
            output_pass_paused: false,
        }
    }
}

impl FloorStreamFault {
    #[cfg(test)]
    fn hold_next_output_refresh(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.hold_output_refresh = true;
        state.output_refresh_held = false;
    }

    #[cfg(test)]
    fn wait_output_refresh(&self) {
        let state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (state, _) = self
            .0
            .changed
            .wait_timeout_while(state, Duration::from_secs(2), |state| {
                !state.output_refresh_held
            })
            .unwrap_or_else(PoisonError::into_inner);
        assert!(
            state.output_refresh_held,
            "output watcher did not finish first refresh"
        );
    }

    #[cfg(test)]
    fn release_output_refresh(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.hold_output_refresh = false;
        self.0.changed.notify_all();
    }

    fn after_output_refresh(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.hold_output_refresh {
            return;
        }
        state.output_refresh_held = true;
        self.0.changed.notify_all();
        while state.hold_output_refresh {
            let (next, timeout) = self
                .0
                .changed
                .wait_timeout(state, Duration::from_secs(2))
                .unwrap_or_else(PoisonError::into_inner);
            state = next;
            if timeout.timed_out() {
                break;
            }
        }
    }

    /// Substitute the zone and clock the finished-goals day is read from.
    pub fn use_local_days(&self, days: crate::local_day::LocalDays) {
        *self
            .0
            .local_days
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(days);
    }

    /// The substituted local-day source, when a test set one.
    pub(crate) fn local_days(&self) -> Option<crate::local_day::LocalDays> {
        self.0
            .local_days
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Hold one writer only after its first output frame is queued, for a queue-overflow test.
    pub fn hold_next_output_drain(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.hold_output_drain = true;
        state.output_drain_held = false;
    }

    /// Wait until the writer is held after output admission.
    #[must_use]
    pub fn wait_output_drain(&self, timeout: Duration) -> bool {
        let state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (state, _) = self
            .0
            .changed
            .wait_timeout_while(state, timeout, |state| !state.output_drain_held)
            .unwrap_or_else(PoisonError::into_inner);
        state.output_drain_held
    }

    /// Release the held writer so the production overflow close can complete.
    pub fn release_output_drain(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.hold_output_drain = false;
        self.0.changed.notify_all();
    }

    fn before_output_drain(&self, queue: &Queue) {
        let has_output = queue
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .frames
            .iter()
            .any(|frame| frame.kind == QueueKind::Output);
        if !has_output {
            return;
        }
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.hold_output_drain {
            return;
        }
        state.output_drain_held = true;
        self.0.changed.notify_all();
        while state.hold_output_drain {
            state = self
                .0
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Hold a registry wake before the watcher acquires Floor state, for an ordering test.
    #[doc(hidden)]
    pub fn hold_next_output_wake(&self, agent: AgentId) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.hold_output_wake = true;
        state.output_wake_agent = Some(agent);
        state.output_wake_held = false;
    }

    /// Observe the held watcher before changing the selected agent.
    #[doc(hidden)]
    #[must_use]
    pub fn wait_output_wake(&self, timeout: Duration) -> bool {
        let state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (state, _) = self
            .0
            .changed
            .wait_timeout_while(state, timeout, |state| !state.output_wake_held)
            .unwrap_or_else(PoisonError::into_inner);
        state.output_wake_held
    }

    /// Release the held output watcher.
    #[doc(hidden)]
    pub fn release_output_wake(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.hold_output_wake = false;
        state.output_wake_agent = None;
        self.0.changed.notify_all();
    }

    /// Park the next watcher pass for `agent` that the wake check declines to hold, before its
    /// refresh reads the registry: the window a loaded scheduler can stretch between those steps.
    #[doc(hidden)]
    pub fn pause_next_output_pass(&self, agent: AgentId) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.pause_output_pass = true;
        state.output_pass_agent = Some(agent);
        state.output_pass_paused = false;
    }

    /// Observe the parked watcher pass before arming a hold behind it.
    #[doc(hidden)]
    #[must_use]
    pub fn wait_output_pass(&self, timeout: Duration) -> bool {
        let state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (state, _) = self
            .0
            .changed
            .wait_timeout_while(state, timeout, |state| !state.output_pass_paused)
            .unwrap_or_else(PoisonError::into_inner);
        state.output_pass_paused
    }

    /// Release the parked watcher pass to its refresh.
    #[doc(hidden)]
    pub fn release_output_pass(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.pause_output_pass = false;
        state.output_pass_agent = None;
        self.0.changed.notify_all();
    }

    fn after_declined_output_wake(
        &self,
        mut state: std::sync::MutexGuard<'_, FaultState>,
        agent: AgentId,
    ) {
        if !state.pause_output_pass || state.output_pass_agent != Some(agent) {
            return;
        }
        state.output_pass_paused = true;
        self.0.changed.notify_all();
        while state.pause_output_pass {
            let (next, timeout) = self
                .0
                .changed
                .wait_timeout(state, Duration::from_secs(5))
                .unwrap_or_else(PoisonError::into_inner);
            state = next;
            if timeout.timed_out() {
                state.pause_output_pass = false;
                state.output_pass_agent = None;
                break;
            }
        }
    }

    fn after_output_wake(&self, agent: AgentId) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.hold_output_wake || state.output_wake_agent != Some(agent) {
            self.after_declined_output_wake(state, agent);
            return;
        }
        state.output_wake_held = true;
        self.0.changed.notify_all();
        while state.hold_output_wake {
            let (next, timeout) = self
                .0
                .changed
                .wait_timeout(state, Duration::from_secs(5))
                .unwrap_or_else(PoisonError::into_inner);
            state = next;
            if timeout.timed_out() {
                state.hold_output_wake = false;
                state.output_wake_agent = None;
                break;
            }
        }
    }

    /// Stop this registry beating, for a test whose subject is the queue's own arithmetic.
    ///
    /// Puts heartbeats through `Queue::enqueue` like any other frame, so they
    /// consume the same 256-message bound — deliberately, and it is what closes a wedged reader.
    /// The cost is that a test pinning "exactly 256 deltas, still live" is no longer counting only
    /// its own deltas. Such a test turns the beat off and keeps asserting what it was written to
    /// assert; it does not widen its expectation to absorb an arrival it cannot time.
    #[must_use]
    pub fn without_heartbeat(self) -> Self {
        {
            let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.heartbeat = false;
        }
        self
    }

    /// Whether a registry carrying this fault should beat.
    fn beats(&self) -> bool {
        self.0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .heartbeat
    }

    /// How many subscriptions made the loop thread serialize the projection rather than copy the
    /// pre-serialized snapshot.
    #[must_use]
    pub fn subscription_serializations(&self) -> usize {
        self.0.serializations.load(Ordering::Relaxed)
    }

    /// The name of the thread that last joined a snapshot's `settings` sibling.
    #[must_use]
    pub fn settings_joined_on(&self) -> Option<String> {
        self.0
            .settings_joiner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn settings_joined(&self) {
        *self
            .0
            .settings_joiner
            .lock()
            .unwrap_or_else(PoisonError::into_inner) =
            std::thread::current().name().map(str::to_owned);
    }

    /// Hold the next new subscription after registration and before its snapshot write.
    pub fn hold_next_subscription(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.armed = true;
        state.registered = false;
        state.queue = None;
    }

    /// Wait until the held subscription has registered.
    #[must_use]
    pub fn wait_registered(&self, timeout: Duration) -> bool {
        let state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (state, _) = self
            .0
            .changed
            .wait_timeout_while(state, timeout, |state| !state.registered)
            .unwrap_or_else(PoisonError::into_inner);
        state.registered
    }

    /// Release the held production writer.
    pub fn release(&self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.armed = false;
        self.0.changed.notify_all();
    }

    /// Queue counts for the held connection, as a test observation only.
    #[must_use]
    pub fn queue_usage(&self) -> Option<(usize, usize, bool)> {
        self.0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .queue
            .as_ref()
            .and_then(Weak::upgrade)
            .map(|queue| queue.usage())
    }

    fn after_registered(&self, queue: &Arc<Queue>) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.armed {
            return;
        }
        state.registered = true;
        state.queue = Some(Arc::downgrade(queue));
        self.0.changed.notify_all();
        while state.armed {
            state = self
                .0
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

fn projection(snapshot: &AgentSnapshot) -> FloorProjection {
    FloorProjection {
        lsn: snapshot.lsn,
        agents: snapshot.agents.clone(),
        cards: snapshot.cards.clone(),
        activity: snapshot.activity.clone(),
        retired_agents: snapshot.retired_agents.clone(),
        mail: Vec::new(),
        closing_time: None,
    }
}

fn delta(previous: &AgentSnapshot, next: &AgentSnapshot) -> FloorDelta {
    let old: BTreeMap<AgentId, &AgentView> = previous
        .agents
        .iter()
        .map(|agent| (agent.id, agent))
        .collect();
    let current: BTreeSet<AgentId> = next.agents.iter().map(|agent| agent.id).collect();
    let old_cards: BTreeMap<CardId, &CardView> =
        previous.cards.iter().map(|card| (card.id, card)).collect();
    FloorDelta {
        lsn: next.lsn,
        activity: (previous.activity != next.activity).then(|| next.activity.clone()),
        retired_agents: (previous.retired_agents != next.retired_agents)
            .then(|| next.retired_agents.clone()),
        agents: next
            .agents
            .iter()
            .filter(|agent| {
                old.get(&agent.id)
                    .is_none_or(|before| agent_changed(before, agent))
            })
            .cloned()
            .collect(),
        removed_agents: old
            .keys()
            .filter(|agent| !current.contains(agent))
            .copied()
            .collect(),
        cards: next
            .cards
            .iter()
            .filter(|card| {
                old_cards
                    .get(&card.id)
                    .is_none_or(|before| *before != *card)
            })
            .cloned()
            .collect(),
        removed_cards: old_cards
            .keys()
            .filter(|card| !next.cards.iter().any(|kept| kept.id == **card))
            .copied()
            .collect(),
        mail: Vec::new(),
        closing_time: None,
    }
}

/// Whether a delta must carry the finished-today sibling: any change except a later projection
/// cut over the same day and total, which every journal record would otherwise make.
fn finished_today_changed(
    before: Option<&FloorFinishedToday>,
    after: Option<&FloorFinishedToday>,
) -> bool {
    match (before, after) {
        (
            Some(FloorFinishedToday::Ready {
                local_date: before_date,
                utc_offset_minutes: before_offset,
                total: before_total,
                ..
            }),
            Some(FloorFinishedToday::Ready {
                local_date,
                utc_offset_minutes,
                total,
                ..
            }),
        ) => (before_date, before_offset, before_total) != (local_date, utc_offset_minutes, total),
        (before, after) => before != after,
    }
}

fn agent_changed(before: &AgentView, after: &AgentView) -> bool {
    if before == after {
        return false;
    }
    let (Some(prior_activity), Some(next_activity)) = (before.live_activity, after.live_activity)
    else {
        return true;
    };
    if prior_activity.kind != next_activity.kind || prior_activity.at != next_activity.at {
        return true;
    }
    let mut prior = before.clone();
    prior.live_activity = Some(next_activity);
    prior != *after
}

fn serialize_frame(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut output = CappedJson::new();
    serde_json::to_writer(&mut output, value).context("serializing a Floor browser event")?;
    if output.overflowed || output.bytes.len() > MAX_MESSAGE_BYTES {
        bail!("Floor browser event exceeds 1 MiB");
    }
    text_frame(&output.bytes).ok_or_else(|| anyhow!("Floor browser event exceeds 1 MiB"))
}

struct CappedJson {
    bytes: Vec<u8>,
    overflowed: bool,
}

impl CappedJson {
    fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(4096),
            overflowed: false,
        }
    }
}

impl io::Write for CappedJson {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = (MAX_MESSAGE_BYTES + 1).saturating_sub(self.bytes.len());
        let retained = remaining.min(buffer.len());
        self.bytes.extend_from_slice(&buffer[..retained]);
        self.overflowed |= retained < buffer.len();
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `snapshot` with `"<key>":<value>` appended as its last key, or `None` over the message cap.
fn with_sibling(snapshot: &[u8], key: &str, value: &[u8]) -> Option<Vec<u8>> {
    let open = text_payload(snapshot)?.strip_suffix(b"}")?;
    let mut joined = Vec::with_capacity(open.len() + key.len() + value.len() + 5);
    joined.extend_from_slice(open);
    joined.extend_from_slice(b",\"");
    joined.extend_from_slice(key.as_bytes());
    joined.extend_from_slice(b"\":");
    joined.extend_from_slice(value);
    joined.push(b'}');
    text_frame(&joined)
}

fn serialize_snapshot(snapshot: &AgentSnapshot) -> Result<Vec<u8>> {
    serialize_snapshot_with(snapshot, Siblings::default())
}

pub(crate) fn look_candidate_fits(
    snapshot: &AgentSnapshot,
    agent: AgentId,
    role: &str,
    character: Option<dudes_types::CastCharacter>,
    accent: Option<dudes_types::AccentColour>,
) -> bool {
    let mut candidate = snapshot.clone();
    let Some(row) = candidate.agents.iter_mut().find(|row| row.id == agent) else {
        return false;
    };
    role.clone_into(&mut row.role);
    row.character = character;
    row.accent = accent;
    row.look_lsn = Some(u64::MAX.to_string());
    crate::board::admit_cards(candidate.agents, candidate.cards).is_ok()
}

fn serialize_snapshot_with(snapshot: &AgentSnapshot, siblings: Siblings<'_>) -> Result<Vec<u8>> {
    serialize_snapshot_with_closing(snapshot, siblings, None)
}

fn serialize_snapshot_with_closing(
    snapshot: &AgentSnapshot,
    siblings: Siblings<'_>,
    closing: Option<crate::closing::FloorClosing>,
) -> Result<Vec<u8>> {
    let mut projection = projection(snapshot);
    projection.closing_time = closing.map(|view| view.phase);
    let siblings = Siblings {
        finished_today: siblings.finished_today.or(snapshot.finished_today.as_ref()),
        ..siblings
    };
    serialize_frame(&SnapshotMessage {
        kind: "floor.snapshot",
        projection: FloorProjectionView {
            projection: &projection,
            closing_progress: closing.map(|view| view.progress),
        },
        question_history: &snapshot.question_history,
        traces: &snapshot.traces,
        messages: &snapshot.mail.messages,
        threads: &snapshot.mail.threads,
        threads_lsn: snapshot.lsn.0.to_string(),
        messages_lsn: snapshot.lsn.0.to_string(),
        siblings,
    })
}

pub(crate) fn refresh_activity_frame(frame: &[u8], now: Timestamp) -> Result<Cow<'_, [u8]>> {
    if !frame_has_live_activity(frame) {
        return Ok(Cow::Borrowed(frame));
    }
    let payload = text_payload(frame).expect("a frame with a live activity key is text");
    let mut message: serde_json::Value = serde_json::from_slice(payload)?;
    let agents_path = match message.get("type").and_then(serde_json::Value::as_str) {
        Some("floor.snapshot") => "/projection/agents",
        Some("floor.delta") => "/delta/agents",
        _ => return Ok(Cow::Borrowed(frame)),
    };
    let Some(agents) = message
        .pointer_mut(agents_path)
        .and_then(serde_json::Value::as_array_mut)
    else {
        return Ok(Cow::Borrowed(frame));
    };
    let mut changed = false;
    for agent in agents {
        let Some(at) = agent
            .get("live_activity")
            .and_then(|activity| activity.get("at"))
            .and_then(serde_json::Value::as_i64)
        else {
            continue;
        };
        let prior = agent["live_activity"]["expires_in_ms"]
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(0);
        let Some(remaining) =
            crate::floor::remaining_lifetime(Timestamp::from_unix_millis(at), now, prior)
        else {
            agent
                .as_object_mut()
                .expect("serialized agent is an object")
                .remove("live_activity");
            changed = true;
            continue;
        };
        if agent["live_activity"]["expires_in_ms"].as_u64() != Some(u64::from(remaining)) {
            agent["live_activity"]["expires_in_ms"] = serde_json::json!(remaining);
            changed = true;
        }
    }
    if !changed {
        return Ok(Cow::Borrowed(frame));
    }
    let payload = serde_json::to_vec(&message)?;
    let frame =
        text_frame(&payload).ok_or_else(|| anyhow!("refreshed Floor frame exceeds 1 MiB"))?;
    Ok(Cow::Owned(frame))
}

pub(crate) fn frame_has_live_activity(frame: &[u8]) -> bool {
    const KEY: &[u8] = b"\"live_activity\"";
    text_payload(frame).is_some_and(|payload| payload.windows(KEY.len()).any(|part| part == KEY))
}

pub(crate) fn json_nesting(bytes: &[u8]) -> Result<usize> {
    let mut depth = 0usize;
    let mut maximum = 0usize;
    let mut string = false;
    let mut escaped = false;
    for byte in bytes {
        if string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                string = false;
            }
            continue;
        }
        match *byte {
            b'"' => string = true,
            b'{' | b'[' => {
                depth = depth
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("JSON depth overflow"))?;
                maximum = maximum.max(depth);
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(maximum)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reserved_chat_fixture() -> (FloorStreams, AgentSnapshot, dudes_proto::ChatView) {
        let mut store = dudes_journal::Projections::in_memory().unwrap();
        let mut snapshot = crate::floor::snapshot(&mut store, Timestamp::UNIX_EPOCH).unwrap();
        snapshot.agents.push(agent("chat".into()));
        let agent_id = snapshot.agents[0].id;
        let turn_id = dudes_types::TurnId::NIL;
        let mut chat = dudes_proto::ChatView::empty();
        chat.pending.push(dudes_proto::ChatPendingView {
            agent_id,
            note_lsn: "1".into(),
            goal_id: Some(dudes_types::GoalId::NIL),
            text: "note".into(),
            state: dudes_proto::ChatPendingState::Reserved,
            turn_id: Some(turn_id),
            cancellable: true,
            reason: None,
        });
        chat.turns.push(dudes_proto::ChatTurnView {
            reissued: false,
            agent_id,
            turn_id,
            note_lsn: "1".into(),
            text: "note".into(),
            state: dudes_proto::ChatTurnState::Reserved,
            reply: None,
            truncated: None,
        });
        let streams = FloorStreams::new(
            snapshot.clone(),
            FloorStreamFault::default().without_heartbeat(),
        )
        .unwrap();
        (streams, snapshot, chat)
    }

    #[test]
    fn look_growth_preserves_the_chat_reservation() {
        let mut store = dudes_journal::Projections::in_memory().unwrap();
        let mut snapshot = crate::floor::snapshot(&mut store, Timestamp::UNIX_EPOCH).unwrap();
        let mut row = agent("a".into());
        row.look_lsn = Some(u64::MAX.to_string());
        let agent_id = row.id;
        snapshot.agents.push(row);
        snapshot.cards.push(CardView {
            id: CardId::NIL,
            title: String::new(),
            department: "test".into(),
            status: dudes_types::CardStatus::Todo,
            status_by: dudes_proto::Actor::System,
            seq: dudes_types::Seq::ZERO,
            claimed_by: None,
            deps: Vec::new(),
            ticket: None,
        });
        let (mut low, mut high) = (0, MAX_MESSAGE_BYTES);
        while low < high {
            let middle = low + (high - low).div_ceil(2);
            snapshot.cards[0].title = "x".repeat(middle);
            if crate::board::admit_cards(snapshot.agents.clone(), snapshot.cards.clone()).is_ok() {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        snapshot.cards[0].title = "x".repeat(low);
        crate::board::admit_cards(snapshot.agents.clone(), snapshot.cards.clone()).unwrap();
        assert!(!look_candidate_fits(
            &snapshot,
            agent_id,
            &"x".repeat(128),
            None,
            None
        ));
    }

    #[test]
    fn chat_publication_refuses_reserved_turns_without_their_pending_reservation() {
        for state in [
            dudes_proto::ChatTurnState::Reserved,
            dudes_proto::ChatTurnState::DeliveryUncertain,
        ] {
            let (streams, snapshot, mut chat) = reserved_chat_fixture();
            let opened = streams.subscribe().unwrap();
            chat.turns[0].state = state;
            if state == dudes_proto::ChatTurnState::DeliveryUncertain {
                chat.pending[0].state = dudes_proto::ChatPendingState::DeliveryUncertain;
            }
            streams
                .publish_closing_with_chat_output(snapshot.clone(), None, Some(chat.clone()), || {})
                .unwrap();
            assert_eq!(
                queued_json(opened.subscription.next(Duration::ZERO))["chat"]["pending"][0]["text"],
                "note"
            );
            chat.pending.clear();
            assert!(
                streams
                    .publish_closing_with_chat_output(
                        snapshot.clone(),
                        None,
                        Some(chat.clone()),
                        || {}
                    )
                    .is_err()
            );
            assert!(matches!(
                opened.subscription.next(Duration::ZERO),
                QueueRead::Empty
            ));
            chat.turns[0].state = dudes_proto::ChatTurnState::ReissueCancelled;
            streams
                .publish_closing_with_chat_output(snapshot, None, Some(chat), || {})
                .unwrap();
            assert_eq!(
                queued_json(opened.subscription.next(Duration::ZERO))["chat"]["turns"][0]["state"],
                "reissue_cancelled"
            );
        }
    }

    #[test]
    fn chat_publication_accepts_completeness_and_refuses_replyless_claims() {
        for (reply, truncated) in [(None, Some(false)), (None, Some(true))] {
            let (streams, snapshot, mut chat) = reserved_chat_fixture();
            let opened = streams.subscribe().unwrap();
            chat.pending.clear();
            chat.turns[0].state = dudes_proto::ChatTurnState::Ended;
            chat.turns[0].reply = Some("reply".into());
            chat.turns[0].truncated = Some(false);
            streams
                .publish_closing_with_chat_output(snapshot.clone(), None, Some(chat.clone()), || {})
                .unwrap();
            assert_eq!(
                queued_json(opened.subscription.next(Duration::ZERO))["chat"]["turns"][0]["reply"],
                "reply"
            );
            chat.turns[0].reply = reply;
            chat.turns[0].truncated = truncated;
            assert!(
                streams
                    .publish_closing_with_chat_output(
                        snapshot.clone(),
                        None,
                        Some(chat.clone()),
                        || {}
                    )
                    .is_err()
            );
            assert!(matches!(
                opened.subscription.next(Duration::ZERO),
                QueueRead::Empty
            ));
            chat.turns[0].reply = Some("cut reply".into());
            chat.turns[0].truncated = Some(true);
            streams
                .publish_closing_with_chat_output(snapshot, None, Some(chat), || {})
                .unwrap();
            assert_eq!(
                queued_json(opened.subscription.next(Duration::ZERO))["chat"]["turns"][0]["truncated"],
                true
            );
        }
    }
    use dudes_types::{AgentStatus, BreakerRung, Lsn, Mode, Provider};
    use serde_json::Value;

    fn closing_view(phase: dudes_proto::ClosingPhase) -> crate::closing::FloorClosing {
        let mut world = dudes_supervisor::World::default();
        let mut waiting = crate::closing::Waiting::new(&world);
        world.apply(
            Lsn(1),
            &dudes_proto::Entry {
                at: Timestamp::UNIX_EPOCH,
                record: dudes_proto::Record::ClosingTime {
                    phase: dudes_proto::ClosingPhase::Announced,
                },
            },
        );
        waiting.observe(world.closing.as_ref().unwrap());
        world.apply(
            Lsn(2),
            &dudes_proto::Entry {
                at: Timestamp::UNIX_EPOCH,
                record: dudes_proto::Record::ClosingTime { phase },
            },
        );
        waiting.floor_view(&world, Timestamp::UNIX_EPOCH).unwrap()
    }

    #[test]
    fn pending_cuts_retain_changed_rows_and_reconstruct_each_large_roster_cut() {
        let mut projections = dudes_journal::Projections::in_memory().unwrap();
        let mut previous = crate::floor::snapshot(&mut projections, Timestamp::UNIX_EPOCH).unwrap();
        for index in 0..4096 {
            let mut row = agent(format!("Agent {index}"));
            row.id = AgentId::from_parts(0, index);
            row.name = format!("Agent {index}");
            previous.agents.push(row);
        }
        let base = previous.clone();
        let mut pending = Vec::new();
        let mut bytes = 0;
        for index in 0..100 {
            let mut next = previous.clone();
            next.lsn = Lsn(index + 1);
            next.mail.lsn = next.lsn;
            next.agents[usize::try_from(index).unwrap()].name.push('!');
            let patch = super::FloorPatch::between(&previous, &next);
            assert_eq!(patch.changes.agents.len(), 1);
            assert!(patch.changes.cards.is_empty());
            assert!(patch.changes.activity.is_none());
            assert!(patch.changes.retired_agents.is_none());
            assert!(patch.questions.is_none() && patch.traces.is_none() && patch.mail.is_none());
            bytes += serde_json::to_vec(&patch.changes).unwrap().len();
            pending.push(patch);
            previous = next;
        }
        assert!(
            bytes < 100_000,
            "100 queued changes retain bounded changed rows, not 100 full rosters: {bytes}"
        );
        let mut cut = base;
        for (index, patch) in pending.into_iter().enumerate() {
            cut = patch.apply(cut);
            assert_eq!(cut.lsn.0, index as u64 + 1);
            assert_eq!(cut.agents[index].name, format!("Agent {index}!"));
            assert_eq!(cut.agents.len(), 4096);
        }
        assert_eq!(cut, previous);
    }

    #[test]
    fn closing_snapshot_and_delta_exclude_ack_identity_and_counts() {
        let mut projections = dudes_journal::Projections::in_memory().unwrap();
        let initial =
            crate::floor::snapshot(&mut projections, dudes_types::Timestamp::UNIX_EPOCH).unwrap();
        let streams = FloorStreams::new(
            initial.clone(),
            FloorStreamFault::default().without_heartbeat(),
        )
        .unwrap();
        let opened = streams.subscribe().unwrap();
        let mut next = initial;
        next.lsn = Lsn(12);
        let phase = dudes_supervisor::closing::floor_phase(dudes_proto::ClosingPhase::Ack {
            agent: AgentId::from_parts(1, 2),
        });
        streams
            .publish_closing_with_output(next, Some(closing_view(phase)), || {})
            .unwrap();
        let state = streams.0.state.lock().unwrap();
        let snapshot: Value =
            serde_json::from_slice(text_payload(&state.snapshot).unwrap()).unwrap();
        assert_eq!(
            snapshot["projection"]["closing_time"],
            serde_json::json!({"kind":"announced"})
        );
        assert!(snapshot.get("participants").is_none());
        assert!(snapshot.get("acknowledged").is_none());
        assert!(snapshot.get("total").is_none());
        drop(state);
        assert_eq!(opened.subscription.queue.usage().0, 1);
    }

    #[test]
    fn late_closing_subscriptions_keep_phase_when_siblings_or_activity_refresh() {
        let ack_agent = AgentId::from_parts(500, 600);
        let control = ControlView {};
        for (with_sibling, with_activity) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let mut projections = dudes_journal::Projections::in_memory().unwrap();
            let mut snapshot =
                crate::floor::snapshot(&mut projections, Timestamp::UNIX_EPOCH).unwrap();
            if with_activity {
                let mut row = agent("active".into());
                row.live_activity = Some(dudes_proto::LiveActivityView {
                    kind: dudes_proto::LiveActivityKind::UsingTool,
                    at: Timestamp::UNIX_EPOCH,
                    expires_in_ms: 60_000,
                });
                snapshot.agents.push(row);
            }
            let streams = FloorStreams::new(
                snapshot.clone(),
                FloorStreamFault::default().without_heartbeat(),
            )
            .unwrap();
            for phase in [
                dudes_proto::ClosingPhase::Announced,
                dudes_supervisor::closing::floor_phase(dudes_proto::ClosingPhase::Ack {
                    agent: ack_agent,
                }),
            ] {
                snapshot.lsn = Lsn(snapshot.lsn.0 + 1);
                streams
                    .publish_closing_with_output(snapshot.clone(), Some(closing_view(phase)), || {})
                    .unwrap();
                let opened = streams
                    .subscribe_with_siblings(
                        Siblings {
                            control: with_sibling.then_some(&control),
                            ..Siblings::default()
                        },
                        None,
                        Timestamp::from_unix_millis(1000),
                    )
                    .unwrap();
                let payload = text_payload(&opened.snapshot).unwrap();
                let frame: Value = serde_json::from_slice(payload).unwrap();
                assert_eq!(
                    frame["projection"]["closing_time"],
                    serde_json::json!({"kind":"announced"}),
                    "sibling={with_sibling} activity={with_activity}"
                );
                assert_eq!(
                    frame["projection"]["closing_progress"]["remaining_ms"],
                    359_000
                );
                assert_eq!(frame.get("control").is_some(), with_sibling);
                assert!(
                    !std::str::from_utf8(payload)
                        .unwrap()
                        .contains(&ack_agent.to_string())
                );
                for excluded in ["participants", "acknowledged", "total"] {
                    assert!(frame.get(excluded).is_none());
                    assert!(frame["projection"].get(excluded).is_none());
                }
            }
        }
    }

    #[test]
    fn time_changes_do_not_publish_progress_but_every_subscription_refreshes_it() {
        let mut projections = dudes_journal::Projections::in_memory().unwrap();
        let snapshot = crate::floor::snapshot(&mut projections, Timestamp::UNIX_EPOCH).unwrap();
        let streams = FloorStreams::new(
            snapshot.clone(),
            FloorStreamFault::default().without_heartbeat(),
        )
        .unwrap();
        let opened = streams.subscribe().unwrap();
        let mut closing = closing_view(dudes_proto::ClosingPhase::Announced);
        streams
            .publish_closing_with_output(snapshot.clone(), Some(closing), || {})
            .unwrap();
        assert_eq!(opened.subscription.queue.usage().0, 1);
        closing.progress.remaining_ms = 10_000;
        streams
            .publish_closing_with_output(snapshot, Some(closing), || {})
            .unwrap();
        assert_eq!(
            opened.subscription.queue.usage().0,
            1,
            "time alone cannot create another delta"
        );
        for time in [10_000, 20_000] {
            let subscription = streams
                .subscribe_with_siblings(
                    Siblings::default(),
                    None,
                    Timestamp::from_unix_millis(time),
                )
                .unwrap();
            let frame: Value =
                serde_json::from_slice(text_payload(&subscription.snapshot).unwrap()).unwrap();
            assert_eq!(
                frame["projection"]["closing_progress"]["remaining_ms"],
                360_000 - time
            );
            assert_eq!(
                frame["projection"]["closing_time"],
                serde_json::json!({"kind":"announced"})
            );
        }
    }

    #[test]
    fn snapshots_and_deltas_outside_closing_have_no_progress() {
        let mut projections = dudes_journal::Projections::in_memory().unwrap();
        let mut snapshot = crate::floor::snapshot(&mut projections, Timestamp::UNIX_EPOCH).unwrap();
        let streams = FloorStreams::new(
            snapshot.clone(),
            FloorStreamFault::default().without_heartbeat(),
        )
        .unwrap();
        let opened = streams.subscribe().unwrap();
        let frame: Value = serde_json::from_slice(text_payload(&opened.snapshot).unwrap()).unwrap();
        assert!(frame["projection"].get("closing_time").is_none());
        assert!(frame["projection"].get("closing_progress").is_none());
        snapshot.lsn = Lsn(1);
        streams.publish(snapshot).unwrap();
        let frame = queued_json(opened.subscription.next(Duration::ZERO));
        assert!(frame["delta"].get("closing_time").is_none());
        assert!(frame["delta"].get("closing_progress").is_none());
    }

    #[test]
    fn stopped_publication_is_exact_once_and_uses_the_existing_queue_bound() {
        let mut projections = dudes_journal::Projections::in_memory().unwrap();
        let snapshot =
            crate::floor::snapshot(&mut projections, dudes_types::Timestamp::UNIX_EPOCH).unwrap();
        let streams =
            FloorStreams::new(snapshot, FloorStreamFault::default().without_heartbeat()).unwrap();
        let opened = streams.subscribe().unwrap();
        streams.publish_journal_stopped(Lsn(u64::MAX)).unwrap();
        streams.publish_journal_stopped(Lsn(1)).unwrap();
        assert_eq!(opened.subscription.queue.usage().0, 1);
        let status = queued_json(opened.subscription.next(Duration::ZERO));
        assert_eq!(
            status["journal_stopped"],
            serde_json::json!({"lsn":"18446744073709551615"})
        );
        let after = streams.subscribe().unwrap();
        let snapshot: Value =
            serde_json::from_slice(text_payload(&after.snapshot).unwrap()).unwrap();
        assert_eq!(snapshot["journal_stopped"], status["journal_stopped"]);
        for _ in 0..MAX_QUEUE_MESSAGES {
            assert_eq!(streams.heartbeat(), 2);
        }
        assert_eq!(opened.subscription.queue.usage().0, MAX_QUEUE_MESSAGES);
        assert_eq!(streams.heartbeat(), 0);
        assert!(matches!(
            opened.subscription.next(Duration::ZERO),
            QueueRead::Overflowed
        ));
    }

    fn archived(name: String) -> dudes_proto::RetiredAgentView {
        dudes_proto::RetiredAgentView {
            id: AgentId::from_parts(1, 1),
            name,
            role: "builder".into(),
            character: None,
            accent: None,
            provider: Provider::Claude,
            workspace_label: Some("Parser".into()),
            hired_at: dudes_types::Timestamp::UNIX_EPOCH,
            retired_at: dudes_types::Timestamp::UNIX_EPOCH,
            turns: 0,
            spend: dudes_proto::Spend::default(),
            worktree: None,
        }
    }

    #[test]
    fn threaded_frames_pair_replacements_and_the_last_thread_eviction() {
        let sender = AgentId::from_parts(1, 1);
        let recipient = AgentId::from_parts(1, 2);
        let third = AgentId::from_parts(1, 3);
        let thread = dudes_types::ThreadId::from_parts(1, 1);
        let id = dudes_types::MailId::from_parts(1, 1);
        let mut store = dudes_journal::Projections::in_memory().unwrap();
        let before = crate::floor::snapshot(&mut store, Timestamp::UNIX_EPOCH).unwrap();
        let streams =
            FloorStreams::new(before, FloorStreamFault::default().without_heartbeat()).unwrap();
        let opened = streams.subscribe().unwrap();
        let initial: Value =
            serde_json::from_slice(text_payload(&opened.snapshot).unwrap()).unwrap();
        assert_eq!(initial["threads"], serde_json::json!([]));
        assert_eq!(initial["threads_lsn"], initial["messages_lsn"]);
        for n in 1..=129 {
            let modern = n == 1;
            let entry = dudes_proto::Entry { at:Timestamp::UNIX_EPOCH,
                record: serde_json::from_value(serde_json::json!({"t":"mail", "id":if modern { id } else { dudes_types::MailId::from_parts(n, 1) },
                    "from":{"kind":"agent","agent":sender}, "to":{"kind":"agent","agent":recipient},
                    "act":"message", "subject":"private-subject", "body":"private-body", "body_len":12,
                    "body_path":"private-path", "body_blake3":"private-hash", "state":{"state":"durable"},
                    "thread":if modern { Some(thread) } else { None }, "thread_seq":u8::from(modern),
                    "members":if modern { vec![sender,recipient,third] } else { vec![] }})).unwrap() };
            store.apply(dudes_types::Lsn(n), 0, &entry).unwrap();
            if n == 1 || n == 128 || n == 129 {
                streams
                    .publish(crate::floor::snapshot(&mut store, Timestamp::UNIX_EPOCH).unwrap())
                    .unwrap();
                let update = queued_json(opened.subscription.next(Duration::ZERO));
                assert_eq!(update["threads_lsn"], update["messages_lsn"]);
                assert_eq!(update["messages_lsn"], n.to_string());
                assert_eq!(
                    update["threads"],
                    if n == 129 {
                        serde_json::json!([])
                    } else {
                        serde_json::json!([{"id":thread,"members":[sender,recipient,third]}])
                    }
                );
                assert!(!update.to_string().contains("private-"));
            }
        }
        let snapshot = streams.subscribe().unwrap().snapshot;
        let value: Value = serde_json::from_slice(text_payload(&snapshot).unwrap()).unwrap();
        assert_eq!(value["threads"], serde_json::json!([]));
        assert_eq!(value["threads_lsn"], "129");
    }

    #[test]
    fn typed_threaded_frames_fit_and_refuse_one_byte_over_with_chat() {
        let a = AgentId::from_parts(u64::MAX, u128::MAX);
        let b = AgentId::from_parts(u64::MAX, u128::MAX - 1);
        let message = dudes_proto::MessageView {
            id: dudes_types::MailId::from_parts(u64::MAX, u128::MAX),
            from: a,
            to: b,
            at: Timestamp::from_unix_millis(i64::MIN),
            lsn: u64::MAX.to_string(),
            state: dudes_proto::MessageState::Notified,
            thread: Some(dudes_types::ThreadId::NIL),
        };
        let thread = dudes_proto::ThreadView {
            id: dudes_types::ThreadId::NIL,
            members: (0..8)
                .rev()
                .map(|n| AgentId::from_parts(u64::MAX, u128::MAX - n))
                .collect(),
        };
        let messages: Vec<_> = (0..128)
            .map(|n| dudes_proto::MessageView {
                id: dudes_types::MailId::from_parts(u64::MAX, n),
                lsn: (u64::MAX - 127 + u64::try_from(n).unwrap()).to_string(),
                thread: Some(dudes_types::ThreadId::from_parts(u64::MAX, n)),
                ..message.clone()
            })
            .collect();
        let threads: Vec<_> = (0..128)
            .map(|n| dudes_proto::ThreadView {
                id: dudes_types::ThreadId::from_parts(u64::MAX, n),
                ..thread.clone()
            })
            .collect();
        for kind in ["floor.snapshot", "floor.delta"] {
            let mut frame = serde_json::json!({"type":kind,"messages":messages,
                "messages_lsn":u64::MAX.to_string(), "threads":threads,
                "threads_lsn":u64::MAX.to_string(),"padding":""});
            frame[if kind == "floor.snapshot" {
                "projection"
            } else {
                "delta"
            }] = if kind == "floor.snapshot" {
                serde_json::to_value(FloorProjection {
                    lsn: dudes_types::Lsn(u64::MAX),
                    ..FloorProjection::default()
                })
                .unwrap()
            } else {
                serde_json::to_value(FloorDelta {
                    lsn: dudes_types::Lsn(u64::MAX),
                    ..FloorDelta::default()
                })
                .unwrap()
            };
            frame["chat"] = serde_json::to_value(dudes_proto::ChatView::empty()).unwrap();
            let bytes = serde_json::to_vec(&frame).unwrap().len();
            frame["padding"] = serde_json::json!("x".repeat(MAX_MESSAGE_BYTES - bytes));
            assert_eq!(
                text_payload(&serialize_frame(&frame).unwrap())
                    .unwrap()
                    .len(),
                MAX_MESSAGE_BYTES
            );
            frame["padding"] =
                serde_json::json!(format!("{}x", frame["padding"].as_str().unwrap()));
            assert!(serialize_frame(&frame).is_err());
        }
    }

    #[test]
    fn a_near_cap_prior_board_replays_with_the_new_typed_mail_pair() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal");
        let card = dudes_types::CardId::from_parts(1, 1);
        let card_view = CardView {
            id: card,
            title: String::new(),
            department: "engineering".into(),
            status: dudes_types::CardStatus::Claimed,
            status_by: dudes_proto::Actor::Agent {
                agent: AgentId::NIL,
            },
            seq: dudes_types::Seq(u64::MAX),
            claimed_by: Some(AgentId::NIL),
            deps: vec![],
            ticket: None,
        };
        let old_empty = serde_json::json!({"type":"floor.snapshot", "projection":FloorProjection {
            lsn:dudes_types::Lsn(u64::MAX), cards:vec![card_view], ..FloorProjection::default()
        }, "messages":[], "messages_lsn":u64::MAX.to_string()});
        let overhead = serde_json::to_vec(&old_empty).unwrap().len();
        let title = "x".repeat(
            MAX_MESSAGE_BYTES
                - overhead
                - crate::floor::MAX_MAIL_BYTES
                - crate::floor::ARCHIVE_RESERVE_BYTES
                - crate::floor::QUESTION_HISTORY_RESERVE_BYTES
                - crate::floor::TRACE_RESERVE_BYTES
                - dudes_proto::MAX_ACTIVITY_BYTES
                - 65_536
                - crate::floor_chat::CHAT_RESERVE_BYTES,
        );
        let (mut writer, _) = dudes_journal::open(
            &journal,
            dudes_journal::JournalOptions {
                fsync: dudes_journal::Fsync::None,
                checkpoint_every: 0,
                ..dudes_journal::JournalOptions::default()
            },
        )
        .unwrap();
        writer
            .append(&dudes_proto::Entry {
                at: Timestamp::UNIX_EPOCH,
                record: dudes_proto::Record::CardCreated {
                    card,
                    title: title.clone(),
                    department: "engineering".into(),
                    by: dudes_proto::Actor::System,
                    deps: vec![],
                },
            })
            .unwrap();
        let members: Vec<_> = (1..=8).map(|n| AgentId::from_parts(1, n)).collect();
        for n in 1..=128 {
            writer.append(&dudes_proto::Entry { at:Timestamp::from_unix_millis(i64::MIN),
                record:serde_json::from_value(serde_json::json!({"t":"mail",
                    "id":dudes_types::MailId::from_parts(1,n),
                    "from":{"kind":"agent","agent":members[0]}, "to":{"kind":"agent","agent":members[1]},
                    "act":"message", "subject":"", "body_len":0,"body_path":"","body_blake3":"",
                    "state":{"state":"durable"}, "thread":dudes_types::ThreadId::from_parts(1,n),
                    "thread_seq":1,"members":members })).unwrap() }).unwrap();
        }
        drop(writer);
        let mut rebuilt = dudes_journal::Projections::in_memory().unwrap();
        let replay =
            dudes_journal::replay(&dudes_journal::Reader::new(&journal), &mut rebuilt).unwrap();
        assert_eq!(replay.applied, 129);
        let snapshot = crate::floor::snapshot(&mut rebuilt, Timestamp::UNIX_EPOCH).unwrap();
        assert_eq!(snapshot.cards[0].title, title);
        assert_eq!(snapshot.mail.messages.len(), 128);
        assert_eq!(snapshot.mail.threads.len(), 128);
        let chat = dudes_proto::ChatView::empty();
        let frame = serialize_snapshot_with(
            &snapshot,
            Siblings {
                chat: Some(&chat),
                ..Siblings::default()
            },
        )
        .unwrap();
        assert!(text_payload(&frame).unwrap().len() <= MAX_MESSAGE_BYTES);
    }

    #[test]
    fn messages_frames_expose_only_agent_metadata_and_keep_the_exact_cut() {
        const SECRET: &str = "mail-private-sentinel";
        let sender = AgentId::from_parts(1, 1);
        let recipient = AgentId::from_parts(1, 2);
        let id = dudes_types::MailId::from_parts(99_999, 1);
        let mut store = dudes_journal::Projections::in_memory().unwrap();
        let before = crate::floor::snapshot(&mut store, Timestamp::UNIX_EPOCH).unwrap();
        let streams = FloorStreams::with_output(
            before,
            FloorStreamFault::default().without_heartbeat(),
            Arc::new(crate::output::OutputRegistry::new()),
        )
        .unwrap();
        let opened = streams.subscribe().unwrap();
        let initial: Value =
            serde_json::from_slice(text_payload(&opened.snapshot).unwrap()).unwrap();
        assert_eq!(initial["messages"], serde_json::json!([]));
        assert_eq!(initial["messages_lsn"], "0");
        for (lsn, from, to) in [
            (
                1,
                serde_json::json!({"kind":"agent","agent":sender}),
                serde_json::json!({"kind":"agent","agent":recipient}),
            ),
            (
                2,
                serde_json::json!({"kind":"human"}),
                serde_json::json!({"kind":"agent","agent":recipient}),
            ),
            (
                3,
                serde_json::json!({"kind":"agent","agent":sender}),
                serde_json::json!({"kind":"human"}),
            ),
        ] {
            store.apply(Lsn(lsn), 0, &dudes_proto::Entry {
                at: Timestamp::from_unix_millis(400 - i64::try_from(lsn).unwrap()),
                record: serde_json::from_value(serde_json::json!({
                    "t":"mail", "id": if lsn == 1 { id } else { dudes_types::MailId::from_parts(1, u128::from(lsn)) },
                    "from":from, "to":to, "act":"message", "subject":SECRET, "body":SECRET,
                    "body_len":SECRET.len(), "body_path":SECRET, "body_blake3":SECRET, "state":{"state":"durable"}
                })).unwrap(),
            }).unwrap();
        }
        store
            .apply(
                Lsn(4),
                0,
                &dudes_proto::Entry {
                    at: Timestamp::from_unix_millis(-100),
                    record: dudes_proto::Record::MailState {
                        id,
                        state: dudes_types::MailState::Bounced { why: SECRET.into() },
                    },
                },
            )
            .unwrap();
        let after = crate::floor::snapshot(&mut store, Timestamp::UNIX_EPOCH).unwrap();
        let expected = serde_json::json!([{"id":id,"from":sender,"to":recipient,"at":399,"lsn":"1","state":"bounced"}]);
        let full = serialize_snapshot(&after).unwrap();
        let snapshot: Value = serde_json::from_slice(text_payload(&full).unwrap()).unwrap();
        streams.publish(after.clone()).unwrap();
        let update = queued_json(opened.subscription.next(Duration::ZERO));
        assert_eq!(
            [&snapshot["messages"], &update["messages"]],
            [&expected, &expected]
        );
        assert_eq!(
            [&snapshot["messages_lsn"], &update["messages_lsn"]],
            ["4", "4"]
        );
        assert_eq!(
            [&snapshot["projection"]["mail"], &update["delta"]["mail"]],
            [&serde_json::json!([]), &serde_json::json!([])]
        );
        assert!(!snapshot.to_string().contains(SECRET));
        assert!(!update.to_string().contains(SECRET));
        store
            .apply(
                Lsn(5),
                0,
                &dudes_proto::Entry {
                    at: Timestamp::UNIX_EPOCH,
                    record: dudes_proto::Record::AgentRenamed {
                        id: sender,
                        name: "Ada".into(),
                        browser_rename: None,
                    },
                },
            )
            .unwrap();
        streams
            .publish(crate::floor::snapshot(&mut store, Timestamp::UNIX_EPOCH).unwrap())
            .unwrap();
        let unchanged = queued_json(opened.subscription.next(Duration::ZERO));
        assert!(unchanged.get("messages").is_none());
        assert_eq!(unchanged["messages_lsn"], "5");
    }

    #[test]
    fn messages_delta_replaces_departing_and_historically_entering_rows_and_clears() {
        let row = |lsn: u64| dudes_proto::MessageView {
            id: dudes_types::MailId::from_parts(lsn, 1),
            from: AgentId::from_parts(1, 1),
            to: AgentId::from_parts(1, 2),
            at: Timestamp::from_unix_millis(-1),
            lsn: lsn.to_string(),
            state: dudes_proto::MessageState::Durable,
            thread: None,
        };
        let mut store = dudes_journal::Projections::in_memory().unwrap();
        let mut before = crate::floor::snapshot(&mut store, Timestamp::UNIX_EPOCH).unwrap();
        before.lsn = Lsn(10);
        before.mail.messages = vec![row(9), row(10)];
        let streams = FloorStreams::with_output(
            before.clone(),
            FloorStreamFault::default().without_heartbeat(),
            Arc::new(crate::output::OutputRegistry::new()),
        )
        .unwrap();
        let opened = streams.subscribe().unwrap();
        let mut next = before;
        next.lsn = Lsn(11);
        next.mail.messages = vec![row(2), row(11)];
        streams.publish(next.clone()).unwrap();
        let update = queued_json(opened.subscription.next(Duration::ZERO));
        assert_eq!(
            update["messages"],
            serde_json::to_value(&next.mail.messages).unwrap()
        );
        assert_eq!(update["messages"][0]["lsn"], "2");
        assert_eq!(update["messages_lsn"], "11");
        next.lsn = Lsn(12);
        streams.publish(next.clone()).unwrap();
        let unchanged = queued_json(opened.subscription.next(Duration::ZERO));
        assert!(unchanged.get("messages").is_none());
        assert_eq!(unchanged["messages_lsn"], "12");
        next.lsn = Lsn(13);
        next.mail.messages.clear();
        streams.publish(next).unwrap();
        let cleared = queued_json(opened.subscription.next(Duration::ZERO));
        assert_eq!(cleared["messages"], serde_json::json!([]));
        assert_eq!(cleared["messages_lsn"], "13");
        let reconnect = streams.subscribe().unwrap();
        let snapshot: Value =
            serde_json::from_slice(text_payload(&reconnect.snapshot).unwrap()).unwrap();
        assert_eq!(snapshot["messages"], serde_json::json!([]));
        assert_eq!(snapshot["messages_lsn"], "13");
    }

    #[test]
    fn question_history_frames_have_exact_fields_and_no_private_answer_payload() {
        const SECRET: &str = "history-private-answer-sentinel";
        let id = AgentId::from_parts(1, 1);
        let mut store = dudes_journal::Projections::in_memory().unwrap();
        let before =
            crate::floor::snapshot(&mut store, dudes_types::Timestamp::UNIX_EPOCH).unwrap();
        let streams = FloorStreams::with_output(
            before,
            FloorStreamFault::default().without_heartbeat(),
            Arc::new(crate::output::OutputRegistry::new()),
        )
        .unwrap();
        let opened = streams.subscribe().unwrap();
        let records = [
            serde_json::json!({"t":"agent_hired","id":id,"name":"Ada","role":"builder","department":"engineering","provider":"claude","cwd":SECRET,"budget":{},"trusted":false}),
            serde_json::json!({"t":"human_question","agent":id,"text":"Which branch?"}),
            serde_json::json!({"t":"human_answer","agent":id,"text":SECRET}),
        ];
        for (number, record) in records.into_iter().enumerate() {
            store
                .apply(
                    Lsn(u64::try_from(number).unwrap() + 1),
                    0,
                    &dudes_proto::Entry {
                        at: dudes_types::Timestamp::UNIX_EPOCH,
                        record: serde_json::from_value(record).unwrap(),
                    },
                )
                .unwrap();
        }
        let after = crate::floor::snapshot(&mut store, dudes_types::Timestamp::UNIX_EPOCH).unwrap();
        let expected = serde_json::json!([{"agent_id":id,"question_lsn":"2","answer_lsn":"3","question":"Which branch?"}]);
        let full = serialize_snapshot(&after).unwrap();
        let wire: Value = serde_json::from_slice(text_payload(&full).unwrap()).unwrap();
        streams.publish(after.clone()).unwrap();
        let delta = queued_json(opened.subscription.next(Duration::ZERO));
        assert_eq!(
            [&wire["question_history"], &delta["question_history"]],
            [&expected, &expected]
        );
        assert!(!String::from_utf8_lossy(&full).contains(SECRET));
        assert!(!delta.to_string().contains(SECRET));
        let unchanged = AgentSnapshot {
            finished_today: None,
            lsn: Lsn(4),
            ..after.clone()
        };
        streams.publish(unchanged).unwrap();
        assert!(
            queued_json(opened.subscription.next(Duration::ZERO))
                .get("question_history")
                .is_none()
        );
        streams
            .publish(AgentSnapshot {
                finished_today: None,
                lsn: Lsn(5),
                question_history: Vec::new(),
                traces: Vec::new(),
                mail: dudes_journal::projections::MailView::default(),
                ..after
            })
            .unwrap();
        assert_eq!(
            queued_json(opened.subscription.next(Duration::ZERO))["question_history"],
            serde_json::json!([])
        );
    }

    fn private_trace_end(
        agent: AgentId,
        turn: dudes_types::TurnId,
        secret: &str,
    ) -> dudes_proto::Record {
        dudes_proto::Record::TurnEnded {
            final_reply: None,
            agent,
            turn,
            mode: dudes_types::Mode::Managed,
            provenance: dudes_types::Provenance::Observed,
            elapsed_ms: Some(250),
            exit: dudes_proto::ExitStatus {
                code: Some(0),
                signal: None,
            },
            usage: Some(dudes_types::Usage {
                input: dudes_types::Tokens::new(10),
                output: dudes_types::Tokens::new(20),
                cache_read: dudes_types::Tokens::new(30),
                cache_creation: dudes_types::Tokens::new(40),
                usd: Some(dudes_types::Usd::from_micros(31_575)),
            }),
            session_usage: None,
            model_usage: BTreeMap::new(),
            last_message: Some(secret.into()),
            session: None,
            outcome: Some(dudes_proto::TurnOutcome::Failed {
                reason: secret.into(),
            }),
        }
    }

    #[test]
    fn trace_frames_have_exact_fields_and_no_private_turn_payload() {
        const SECRET: &str = "trace-private-payload-sentinel";
        let mut store = dudes_journal::Projections::in_memory().unwrap();
        let before =
            crate::floor::snapshot(&mut store, dudes_types::Timestamp::UNIX_EPOCH).unwrap();
        let streams = FloorStreams::with_output(
            before,
            FloorStreamFault::default().without_heartbeat(),
            Arc::new(crate::output::OutputRegistry::new()),
        )
        .unwrap();
        let opened = streams.subscribe().unwrap();
        let agent = AgentId::from_parts(1, 1);
        let turn = dudes_types::TurnId::from_parts(1, 2);
        let hire: dudes_proto::Record = serde_json::from_value(serde_json::json!({
            "t":"agent_hired","id":agent,"name":"Ada","role":"builder",
            "department":"engineering","provider":"claude","cwd":"/work",
            "budget":{},"trusted":false
        }))
        .unwrap();
        let issue = dudes_proto::Record::TurnIssued {
            agent,
            turn,
            goal: dudes_types::GoalId::from_parts(1, 3),
            task: SECRET.into(),
            session: None,
            mode: dudes_types::Mode::Managed,
            model_selection: None,
            lsn_of_decision: Lsn(1),
            answers: Vec::new(),
        };
        let spawn = dudes_proto::Record::TurnSpawned {
            agent,
            turn,
            pid: 42,
            start_time: 7,
            boot: None,
            session: None,
            reissued: false,

            mcp_grant: None,
        };
        let end = private_trace_end(agent, turn, SECRET);
        for (number, record) in [hire, issue, spawn, end].into_iter().enumerate() {
            store
                .apply(
                    Lsn(u64::try_from(number).unwrap() + 1),
                    0,
                    &dudes_proto::Entry {
                        at: dudes_types::Timestamp::from_unix_millis(
                            i64::try_from(number).unwrap() + 1_000,
                        ),
                        record,
                    },
                )
                .unwrap();
        }
        let after = crate::floor::snapshot(&mut store, dudes_types::Timestamp::UNIX_EPOCH).unwrap();
        let expected = serde_json::json!([{
            "agent_id": agent,
            "started_at": 1002,
            "duration_ms": 250,
            "outcome": "failed",
            "engine": "claude",
            "tokens": "100",
            "cost_usd_micros": "31575"
        }]);
        let full = serialize_snapshot(&after).unwrap();
        let wire: Value = serde_json::from_slice(text_payload(&full).unwrap()).unwrap();
        streams.publish(after.clone()).unwrap();
        let delta = queued_json(opened.subscription.next(Duration::ZERO));
        assert_eq!([&wire["traces"], &delta["traces"]], [&expected, &expected]);
        assert!(!String::from_utf8_lossy(&full).contains(SECRET));
        assert!(!delta.to_string().contains(SECRET));
        streams
            .publish(AgentSnapshot {
                finished_today: None,
                lsn: Lsn(5),
                ..after.clone()
            })
            .unwrap();
        assert!(
            queued_json(opened.subscription.next(Duration::ZERO))
                .get("traces")
                .is_none()
        );
        streams
            .publish(AgentSnapshot {
                finished_today: None,
                lsn: Lsn(6),
                traces: Vec::new(),
                mail: dudes_journal::projections::MailView::default(),
                ..after
            })
            .unwrap();
        assert_eq!(
            queued_json(opened.subscription.next(Duration::ZERO))["traces"],
            serde_json::json!([])
        );
    }

    #[test]
    fn archive_changes_replace_the_entire_window_including_empty() {
        let before = AgentSnapshot {
            finished_today: None,
            lsn: Lsn(1),
            agents: vec![],
            cards: vec![],
            activity: vec![],
            question_history: Vec::new(),
            traces: Vec::new(),
            mail: dudes_journal::projections::MailView::default(),
            retired_agents: vec![],
        };
        let row = archived("Ada".into());
        let after = AgentSnapshot {
            finished_today: None,
            lsn: Lsn(2),
            question_history: Vec::new(),
            traces: Vec::new(),
            mail: dudes_journal::projections::MailView::default(),
            retired_agents: vec![row.clone()],
            ..before.clone()
        };
        assert_eq!(delta(&before, &after).retired_agents, Some(vec![row]));
        assert_eq!(delta(&after, &after).retired_agents, None);
        let cleared = AgentSnapshot {
            finished_today: None,
            lsn: Lsn(3),
            ..before
        };
        assert_eq!(
            serde_json::to_value(delta(&after, &cleared)).unwrap()["retired_agents"],
            serde_json::json!([])
        );
    }

    #[test]
    fn archive_frames_have_exact_display_fields_and_omit_private_journal_payloads() {
        const SECRET: &str = "archive-private-sentinel-never-render";
        let id = AgentId::from_parts(1, 1);
        let mut store = dudes_journal::Projections::in_memory().unwrap();
        let before =
            crate::floor::snapshot(&mut store, dudes_types::Timestamp::UNIX_EPOCH).unwrap();
        let records = [
            serde_json::json!({"t":"agent_hired", "id":id, "name":"Ada", "role":"builder", "department":"engineering", "provider":"claude", "cwd":SECRET, "budget":{}, "trusted":false, "workspace":{"id":SECRET, "label":"Parser"}}),
            serde_json::json!({"t":"agent_retired", "id":id, "reason":SECRET}),
        ];
        for (index, record) in records.into_iter().enumerate() {
            let row = dudes_proto::Entry {
                at: dudes_types::Timestamp::UNIX_EPOCH,
                record: serde_json::from_value(record).unwrap(),
            };
            store
                .apply(Lsn(u64::try_from(index).unwrap() + 1), 0, &row)
                .unwrap();
        }
        let after = crate::floor::snapshot(&mut store, dudes_types::Timestamp::UNIX_EPOCH).unwrap();
        let expected = serde_json::json!([{"id":id, "name":"Ada", "role":"builder", "provider":"claude", "workspace_label":"Parser",
            "hired_at":0, "retired_at":0, "turns":0, "spend":{"usd":0, "tokens":0, "work_tokens":0}}]);
        let full = serialize_snapshot(&after).unwrap();
        let wire: Value = serde_json::from_slice(text_payload(&full).unwrap()).unwrap();
        assert_eq!(wire["projection"]["retired_agents"], expected);
        let changes = delta(&before, &after);
        let update = serialize_frame(&DeltaMessage {
            finished_today: None,
            chat: None,
            kind: "floor.delta",
            delta: FloorDeltaView {
                delta: &changes,
                closing_progress: None,
            },
            question_history: None,
            traces: None,
            journal_stopped: None,
            messages: None,
            messages_lsn: "0".into(),
            threads: None,
            threads_lsn: "0".into(),
        })
        .unwrap();
        let wire: Value = serde_json::from_slice(text_payload(&update).unwrap()).unwrap();
        assert_eq!(wire["delta"]["retired_agents"], expected);
        assert!(!String::from_utf8_lossy(&full).contains(SECRET));
        assert!(!String::from_utf8_lossy(&update).contains(SECRET));
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one frame is sized across every reserved array and sibling"
    )]
    fn board_admission_fits_a_complete_frame_with_every_reserved_array_and_sibling_at_its_cap() {
        let mut card = CardView {
            id: CardId::NIL,
            title: String::new(),
            department: "engineering".into(),
            status: dudes_types::CardStatus::Claimed,
            status_by: dudes_proto::Actor::Agent {
                agent: AgentId::NIL,
            },
            seq: dudes_types::Seq(u64::MAX),
            claimed_by: Some(AgentId::NIL),
            deps: vec![],
            ticket: None,
        };
        let empty = FloorProjection {
            lsn: Lsn(u64::MAX),
            cards: vec![card.clone()],
            ..FloorProjection::default()
        };
        let base =
            serde_json::to_vec(&serde_json::json!({"type":"floor.snapshot", "projection":empty, "messages": [], "messages_lsn": u64::MAX.to_string(), "threads": [], "threads_lsn":u64::MAX.to_string()}))
                .unwrap()
                .len();
        card.title = "x".repeat(
            MAX_MESSAGE_BYTES
                - base
                - crate::floor::MAX_MAIL_BYTES
                - dudes_proto::MAX_ACTIVITY_BYTES
                - crate::floor::ARCHIVE_RESERVE_BYTES
                - crate::floor::QUESTION_HISTORY_RESERVE_BYTES
                - crate::floor::TRACE_RESERVE_BYTES
                - 65_536
                - crate::floor_chat::CHAT_RESERVE_BYTES,
        );
        crate::board::admit_cards(vec![], vec![card.clone()]).unwrap();
        let mut over = card.clone();
        over.title.push('x');
        assert_eq!(
            crate::board::admit_cards(vec![], vec![over])
                .unwrap_err()
                .code,
            dudes_proto::rpc::ErrorCode::Refused.code()
        );
        let mut archive = vec![archived("\"\\\u{0001}€".into())];
        let archive_bytes = serde_json::to_vec(&archive).unwrap().len();
        archive[0]
            .name
            .push_str(&"x".repeat(dudes_proto::MAX_RETIRED_AGENT_BYTES - archive_bytes));
        assert_eq!(serde_json::to_vec(&archive).unwrap().len(), 32_768);
        let mut activity = vec![dudes_proto::ActivityView {
            lsn: "1".into(),
            at: dudes_types::Timestamp::UNIX_EPOCH,
            event: dudes_proto::ActivityEvent::TurnEnded {
                agent: dudes_proto::ActivityAgent {
                    id: AgentId::NIL,
                    name: Some(String::new()),
                },
            },
        }];
        let activity_bytes = serde_json::to_vec(&activity).unwrap().len();
        let dudes_proto::ActivityEvent::TurnEnded { agent } = &mut activity[0].event else {
            unreachable!()
        };
        agent.name = Some("x".repeat(dudes_proto::MAX_ACTIVITY_BYTES - activity_bytes));
        assert_eq!(serde_json::to_vec(&activity).unwrap().len(), 32_768);
        let mut messages = vec!["\"\\\u{0001}€".to_owned()];
        let message_bytes = serde_json::to_vec(&messages).unwrap().len();
        messages[0].push_str(&"x".repeat(dudes_proto::MAX_MESSAGES_BYTES - message_bytes));
        assert_eq!(serde_json::to_vec(&messages).unwrap().len(), 65_536);
        let full = FloorProjection {
            lsn: Lsn(u64::MAX),
            agents: vec![],
            cards: vec![card],
            mail: Vec::new(),
            activity,
            retired_agents: archive,
            closing_time: None,
        };
        let sibling_overhead = serde_json::to_vec(&serde_json::json!({"onboarding":""}))
            .unwrap()
            .len()
            - 2;
        let mut history = vec![dudes_proto::AnsweredQuestionView {
            agent_id: AgentId::NIL,
            question_lsn: "1".into(),
            answer_lsn: "2".into(),
            question: "\"\\\u{0001}€".into(),
        }];
        let history_bytes = serde_json::to_vec(&history).unwrap().len();
        history[0]
            .question
            .push_str(&"x".repeat(dudes_proto::MAX_QUESTION_HISTORY_BYTES - history_bytes));
        assert_eq!(serde_json::to_vec(&history).unwrap().len(), 32_768);
        let traces = serde_json::json!(["x".repeat(dudes_proto::MAX_TRACE_BYTES - 4)]);
        assert_eq!(
            serde_json::to_vec(&traces).unwrap().len(),
            dudes_proto::MAX_TRACE_BYTES
        );
        let finished = serde_json::to_value(FloorFinishedToday::Ready {
            local_date: "9999-12-31".to_owned(),
            utc_offset_minutes: -1_560,
            total: u64::MAX,
            through_lsn: u64::MAX,
        })
        .unwrap();
        let finished_bytes = serde_json::to_vec(&finished).unwrap().len();
        assert!(
            finished_bytes <= 256,
            "the sibling fits 256 bytes, not {finished_bytes}"
        );
        // The sibling is charged to the same 65,536-byte reservation the other siblings share.
        let charged = finished_bytes + b",\"finished_today\":".len();
        let message = serde_json::json!({"type":"floor.snapshot", "projection":full, "question_history":history, "traces":traces,
            "messages":messages, "messages_lsn":u64::MAX.to_string(), "threads":[], "threads_lsn":u64::MAX.to_string(),
            "finished_today":finished,
            "onboarding":"x".repeat(65_536 - sibling_overhead - 1 - charged)});
        let frame = serialize_frame(&message).unwrap();
        assert!(text_payload(&frame).unwrap().len() <= MAX_MESSAGE_BYTES);
    }

    #[test]
    fn messages_frame_refuses_exactly_one_serialized_byte_over_the_complete_cap() {
        let mut message = serde_json::json!({
            "type":"floor.snapshot", "projection":FloorProjection::default(),
            "messages":[], "messages_lsn":u64::MAX.to_string(), "threads":[], "threads_lsn":u64::MAX.to_string(), "padding":"",
        });
        let bytes = serde_json::to_vec(&message).unwrap().len();
        message["padding"] = serde_json::json!("x".repeat(MAX_MESSAGE_BYTES - bytes));
        assert_eq!(
            text_payload(&serialize_frame(&message).unwrap())
                .unwrap()
                .len(),
            MAX_MESSAGE_BYTES
        );
        message["padding"] =
            serde_json::json!(format!("{}x", message["padding"].as_str().unwrap()));
        assert!(serialize_frame(&message).is_err());
    }

    #[test]
    fn activity_changes_replace_the_entire_window_including_empty() {
        let mut before = AgentSnapshot {
            finished_today: None,
            lsn: Lsn(1),
            agents: vec![],
            cards: vec![],
            activity: vec![],
            question_history: Vec::new(),
            traces: Vec::new(),
            mail: dudes_journal::projections::MailView::default(),
            retired_agents: Vec::new(),
        };
        let row = dudes_proto::ActivityView {
            lsn: u64::MAX.to_string(),
            at: dudes_types::Timestamp::UNIX_EPOCH,
            event: dudes_proto::ActivityEvent::Pause {
                agent: dudes_proto::ActivityAgent {
                    id: AgentId::NIL,
                    name: Some("Ada".into()),
                },
            },
        };
        let after = AgentSnapshot {
            finished_today: None,
            lsn: Lsn(2),
            activity: vec![row.clone()],
            ..before.clone()
        };
        assert_eq!(delta(&before, &after).activity, Some(vec![row.clone()]));
        assert_eq!(delta(&after, &after).activity, None);
        before.lsn = Lsn(3);
        let clear = serde_json::to_value(delta(&after, &before)).unwrap();
        assert_eq!(clear["activity"], serde_json::json!([]));
        let frame = serialize_snapshot(&after).unwrap();
        let wire: Value = serde_json::from_slice(text_payload(&frame).unwrap()).unwrap();
        assert_eq!(
            wire["projection"]["activity"][0]["lsn"],
            u64::MAX.to_string()
        );
        assert_eq!(wire["projection"]["activity"][0]["kind"], "pause");
    }

    #[test]
    fn activity_frames_omit_planted_private_journal_payloads() {
        const SECRET: &str = "activity-private-sentinel-do-not-render";
        let id = AgentId::from_parts(1, 1);
        let goal = dudes_types::GoalId::from_parts(1, 2);
        let turn = dudes_types::TurnId::from_parts(1, 3);
        let mut store = dudes_journal::Projections::in_memory().unwrap();
        let entry = |record| dudes_proto::Entry {
            at: dudes_types::Timestamp::UNIX_EPOCH,
            record,
        };
        let records = [
            serde_json::json!({"t":"agent_hired", "id":id, "name":"Ada", "role":"builder", "department":"engineering", "provider":"claude", "cwd":SECRET, "budget":{}, "trusted":false}),
            serde_json::json!({"t":"goal_set", "agent":id, "goal":{"id":goal, "text":SECRET, "predicate":{"command":{"argv":[SECRET], "timeout_s":1}}, "budget":{}, "hop_cap":1}}),
            serde_json::json!({"t":"turn_issued", "agent":id, "turn":turn, "goal":goal, "task":SECRET, "mode":"managed", "lsn_of_decision":0}),
            serde_json::json!({"t":"turn_ended", "agent":id, "turn":turn, "mode":"managed", "provenance":"observed", "last_message":SECRET}),
            serde_json::json!({"t":"human_question", "agent":id, "text":SECRET}),
            serde_json::json!({"t":"human_answer", "agent":id, "text":SECRET}),
            serde_json::json!({"t":"mail", "id":dudes_types::MailId::NIL, "from":{"kind":"human"}, "to":{"kind":"agent", "agent":id}, "act":"message", "subject":SECRET, "body":SECRET, "body_len":42, "state":{"state":"durable"}}),
            serde_json::json!({"t":"memory_append", "agent":id, "text":SECRET}),
            serde_json::json!({"t":"agent_retired", "id":id, "reason":SECRET}),
        ];
        store
            .apply(
                Lsn(1),
                0,
                &entry(serde_json::from_value(records[0].clone()).unwrap()),
            )
            .unwrap();
        let before =
            crate::floor::snapshot(&mut store, dudes_types::Timestamp::UNIX_EPOCH).unwrap();
        for (index, record) in records.into_iter().enumerate().skip(1) {
            store
                .apply(
                    Lsn(u64::try_from(index).unwrap() + 1),
                    0,
                    &entry(
                        serde_json::from_value(record)
                            .unwrap_or_else(|error| panic!("record {index}: {error}")),
                    ),
                )
                .unwrap();
        }
        let after = crate::floor::snapshot(&mut store, dudes_types::Timestamp::UNIX_EPOCH).unwrap();
        assert!(after.agents.is_empty());
        assert_eq!(after.activity.len(), 6);
        let frame = serialize_snapshot(&after).unwrap();
        assert!(!String::from_utf8_lossy(text_payload(&frame).unwrap()).contains(SECRET));
        let change = delta(&before, &after);
        let frame = serialize_frame(&DeltaMessage {
            finished_today: None,
            chat: None,
            kind: "floor.delta",
            delta: FloorDeltaView {
                delta: &change,
                closing_progress: None,
            },
            question_history: None,
            traces: None,
            journal_stopped: None,
            messages: None,
            messages_lsn: "0".into(),
            threads: None,
            threads_lsn: "0".into(),
        })
        .unwrap();
        let wire = String::from_utf8_lossy(text_payload(&frame).unwrap());
        assert!(!wire.contains(SECRET));
        assert!(wire.contains("agent_retired"));
        assert!(wire.contains("Ada"));
    }

    #[test]
    fn card_records_reach_the_snapshot_and_changed_card_delta() {
        let mut projected = dudes_journal::Projections::in_memory().unwrap();
        let card = dudes_types::CardId::from_parts(1, 9);
        let by = dudes_proto::Actor::Peer { pid: 7, uid: None };
        let entry = |record| dudes_proto::Entry {
            at: dudes_types::Timestamp::UNIX_EPOCH,
            record,
        };
        projected
            .apply(
                Lsn(1),
                1,
                &entry(dudes_proto::Record::CardCreated {
                    card,
                    title: "Browser board card".to_owned(),
                    department: "engineering".to_owned(),
                    by,
                    deps: vec![],
                }),
            )
            .unwrap();
        projected
            .apply(
                Lsn(2),
                2,
                &entry(dudes_proto::Record::CardCreated {
                    card: CardId::from_parts(1, 10),
                    title: "Unchanged card".to_owned(),
                    department: "engineering".to_owned(),
                    by,
                    deps: vec![],
                }),
            )
            .unwrap();
        let before =
            crate::floor::snapshot(&mut projected, dudes_types::Timestamp::UNIX_EPOCH).unwrap();
        let frame = serialize_snapshot(&before).unwrap();
        let wire: Value = serde_json::from_slice(text_payload(&frame).unwrap()).unwrap();
        assert_eq!(
            wire["projection"]["cards"][0]["title"],
            "Browser board card"
        );
        projected
            .apply(
                Lsn(3),
                3,
                &entry(dudes_proto::Record::CardTransition {
                    card,
                    from: dudes_types::CardStatus::Todo,
                    to: dudes_types::CardStatus::Done,
                    by,
                    expected_seq: dudes_types::Seq::ZERO,
                    reason: None,
                }),
            )
            .unwrap();
        let after =
            crate::floor::snapshot(&mut projected, dudes_types::Timestamp::UNIX_EPOCH).unwrap();
        let change = delta(&before, &after);
        assert_eq!(change.cards.len(), 1);
        assert!(delta(&after, &after).cards.is_empty());
        assert!(change.removed_cards.is_empty());
        let mut gone = after.clone();
        gone.cards.clear();
        let removal = delta(&after, &gone);
        assert_eq!(removal.removed_cards.len(), after.cards.len());
        assert!(removal.removed_cards.contains(&card));
        assert!(removal.cards.is_empty());
        assert!(delta(&gone, &gone).removed_cards.is_empty());
        assert!(
            serde_json::to_value(delta(&after, &after))
                .unwrap()
                .get("removed_cards")
                .is_none(),
            "an unchanged board adds nothing to the wire"
        );
        let wire = serde_json::to_value(change).unwrap();
        assert_eq!(wire["cards"][0]["status"], "done");
        assert_eq!(wire["cards"][0]["seq"], 1);
        assert!(frame.len() < MAX_MESSAGE_BYTES - crate::floor::MAX_MAIL_BYTES - 65_536);
    }

    #[test]
    fn admitted_cards_fit_the_snapshot_and_an_oversized_replay_is_refused() {
        let card = CardView {
            id: CardId::from_parts(1, 9),
            title: "x".repeat(115_468),
            department: "engineering".to_owned(),
            status: dudes_types::CardStatus::Blocked,
            status_by: dudes_proto::Actor::Agent {
                agent: AgentId::NIL,
            },
            seq: dudes_types::Seq(u64::MAX),
            claimed_by: Some(AgentId::NIL),
            deps: Vec::new(),
            ticket: None,
        };
        crate::board::admit_cards(Vec::new(), vec![card.clone()]).unwrap();
        let mut snapshot = AgentSnapshot {
            finished_today: None,
            activity: Vec::new(),
            question_history: Vec::new(),
            traces: Vec::new(),
            mail: dudes_journal::projections::MailView::default(),
            retired_agents: Vec::new(),
            lsn: Lsn(u64::MAX),
            agents: Vec::new(),
            cards: vec![card],
        };
        let frame = serialize_snapshot(&snapshot).unwrap();
        assert!(
            text_payload(&frame).unwrap().len()
                + crate::floor::MAX_MAIL_BYTES
                + dudes_proto::MAX_ACTIVITY_BYTES
                + crate::floor::ARCHIVE_RESERVE_BYTES
                + crate::floor::QUESTION_HISTORY_RESERVE_BYTES
                + crate::floor::TRACE_RESERVE_BYTES
                + 65_536
                <= 1_048_576
        );
        snapshot.cards[0].title = "x".repeat(1_048_576);
        assert!(serialize_snapshot(&snapshot).is_err());
    }

    fn agent(role: String) -> AgentView {
        AgentView {
            character: None,
            accent: None,
            id: AgentId::from_parts(1, 1),
            name: "bounded".to_owned(),
            role,
            look_lsn: None,
            department: "engineering".to_owned(),
            provider: Provider::Claude,
            workspace: None,
            status: AgentStatus::Idle,
            live_activity: None,
            mode: Mode::Managed,
            paused: false,
            halted: false,
            automatic_hold: None,
            breaker: BreakerRung::Healthy,
            goal: None,
            turns: 0,
            spent_today: dudes_proto::Spend::default(),
            unread: 0,
            question: None,
            question_lsn: None,
            control_lsn: None,
            session: None,
            last_turn_ended_at: None,
            worktree: None,
            shares_checkout: false,
        }
    }

    #[test]
    fn a_fresh_subscriber_does_not_receive_cached_expired_activity() {
        let mut row = agent("builder".into());
        row.live_activity = Some(dudes_proto::LiveActivityView {
            kind: dudes_proto::LiveActivityKind::UsingTool,
            at: dudes_types::Timestamp::from_unix_millis(1_000),
            expires_in_ms: 60_000,
        });
        let streams = FloorStreams::new(
            AgentSnapshot {
                finished_today: None,
                lsn: Lsn(7),
                agents: vec![row],
                cards: Vec::new(),
                activity: Vec::new(),
                retired_agents: Vec::new(),
                question_history: Vec::new(),
                traces: Vec::new(),
                mail: dudes_journal::projections::MailView::default(),
            },
            FloorStreamFault::default().without_heartbeat(),
        )
        .unwrap();
        let subscribe_at = |at| {
            let opened = streams
                .subscribe_with_siblings(
                    Siblings::default(),
                    None,
                    dudes_types::Timestamp::from_unix_millis(at),
                )
                .unwrap();
            let frame: Value =
                serde_json::from_slice(text_payload(&opened.snapshot).unwrap()).unwrap();
            frame["projection"]["agents"][0]["live_activity"].clone()
        };
        assert_eq!(
            subscribe_at(31_000),
            serde_json::json!({"kind":"using_tool", "at":1000, "expires_in_ms":30000})
        );
        assert_eq!(subscribe_at(61_000), Value::Null);
    }

    #[test]
    fn queued_activity_expires_before_a_delayed_socket_write() {
        let mut row = agent("builder".into());
        row.live_activity = Some(dudes_proto::LiveActivityView {
            kind: dudes_proto::LiveActivityKind::UsingTool,
            at: dudes_types::Timestamp::from_unix_millis(1_000),
            expires_in_ms: 60_000,
        });
        let initial = AgentSnapshot {
            finished_today: None,
            lsn: Lsn(1),
            agents: vec![row.clone()],
            cards: Vec::new(),
            activity: Vec::new(),
            retired_agents: Vec::new(),
            question_history: Vec::new(),
            traces: Vec::new(),
            mail: dudes_journal::projections::MailView::default(),
        };
        let streams = FloorStreams::new(
            initial.clone(),
            FloorStreamFault::default().without_heartbeat(),
        )
        .unwrap();
        let opened = streams
            .subscribe_with_siblings(
                Siblings::default(),
                None,
                dudes_types::Timestamp::from_unix_millis(1_000),
            )
            .unwrap();
        let delayed = dudes_types::Timestamp::from_unix_millis(61_000);
        let snapshot = refresh_activity_frame(&opened.snapshot, delayed).unwrap();
        let snapshot: Value = serde_json::from_slice(text_payload(&snapshot).unwrap()).unwrap();
        assert_eq!(
            snapshot["projection"]["agents"][0]["live_activity"],
            Value::Null
        );

        row.role = "reviewer".into();
        let mut next = initial;
        next.lsn = Lsn(2);
        next.agents = vec![row];
        streams.publish(next).unwrap();
        let QueueRead::Frame(queued) = opened.subscription.next(Duration::ZERO) else {
            panic!("the changed agent has a queued delta")
        };
        let delta = refresh_activity_frame(&queued, delayed).unwrap();
        let delta: Value = serde_json::from_slice(text_payload(&delta).unwrap()).unwrap();
        assert_eq!(delta["delta"]["agents"][0]["role"], "reviewer");
        assert_eq!(delta["delta"]["agents"][0]["live_activity"], Value::Null);
    }

    #[test]
    fn a_backward_clock_cannot_extend_a_queued_activity_lease() {
        let mut row = agent("builder".into());
        row.live_activity = Some(dudes_proto::LiveActivityView {
            kind: dudes_proto::LiveActivityKind::Working,
            at: dudes_types::Timestamp::from_unix_millis(1_000),
            expires_in_ms: 10_000,
        });
        let frame = serialize_snapshot(&AgentSnapshot {
            finished_today: None,
            lsn: Lsn(3),
            agents: vec![row],
            cards: Vec::new(),
            activity: Vec::new(),
            retired_agents: Vec::new(),
            question_history: Vec::new(),
            traces: Vec::new(),
            mail: dudes_journal::projections::MailView::default(),
        })
        .unwrap();
        let rewritten =
            refresh_activity_frame(&frame, dudes_types::Timestamp::from_unix_millis(11_000))
                .unwrap();
        let rewritten: Value = serde_json::from_slice(text_payload(&rewritten).unwrap()).unwrap();
        assert_eq!(
            rewritten["projection"]["agents"][0]["live_activity"]["expires_in_ms"],
            10_000
        );
    }

    #[test]
    fn a_full_queued_frame_keeps_non_activity_fields_after_lifetime_refresh() {
        let mut row = agent(String::new());
        row.live_activity = Some(dudes_proto::LiveActivityView {
            kind: dudes_proto::LiveActivityKind::UsingTool,
            at: dudes_types::Timestamp::from_unix_millis(1_000),
            expires_in_ms: 60_000,
        });
        let mut snapshot = AgentSnapshot {
            finished_today: None,
            lsn: Lsn(7),
            agents: vec![row],
            cards: Vec::new(),
            activity: Vec::new(),
            retired_agents: Vec::new(),
            question_history: Vec::new(),
            traces: Vec::new(),
            mail: dudes_journal::projections::MailView::default(),
        };
        let overhead = text_payload(&serialize_snapshot(&snapshot).unwrap())
            .unwrap()
            .len();
        let role = "x".repeat(MAX_MESSAGE_BYTES - overhead);
        snapshot.agents[0].role.clone_from(&role);
        let frame = serialize_snapshot(&snapshot).unwrap();
        assert_eq!(text_payload(&frame).unwrap().len(), MAX_MESSAGE_BYTES);

        let refreshed =
            refresh_activity_frame(&frame, dudes_types::Timestamp::from_unix_millis(31_000))
                .unwrap();
        assert!(text_payload(&refreshed).unwrap().len() <= MAX_MESSAGE_BYTES);
        let refreshed: Value = serde_json::from_slice(text_payload(&refreshed).unwrap()).unwrap();
        assert_eq!(refreshed["projection"]["lsn"], 7);
        assert_eq!(refreshed["projection"]["agents"][0]["role"], role);
        assert_eq!(
            refreshed["projection"]["agents"][0]["live_activity"]["expires_in_ms"],
            30_000
        );

        let expired =
            refresh_activity_frame(&frame, dudes_types::Timestamp::from_unix_millis(61_000))
                .unwrap();
        let expired: Value = serde_json::from_slice(text_payload(&expired).unwrap()).unwrap();
        assert_eq!(expired["projection"]["agents"][0]["role"], role);
        assert_eq!(
            expired["projection"]["agents"][0]["live_activity"],
            Value::Null
        );
    }

    #[test]
    fn a_new_ttl_alone_does_not_resend_an_unchanged_agent() {
        let mut row = agent("builder".into());
        row.live_activity = Some(dudes_proto::LiveActivityView {
            kind: dudes_proto::LiveActivityKind::Working,
            at: dudes_types::Timestamp::from_unix_millis(1_000),
            expires_in_ms: 60_000,
        });
        let before = AgentSnapshot {
            finished_today: None,
            lsn: Lsn(1),
            agents: vec![row.clone()],
            cards: Vec::new(),
            activity: Vec::new(),
            retired_agents: Vec::new(),
            question_history: Vec::new(),
            traces: Vec::new(),
            mail: dudes_journal::projections::MailView::default(),
        };
        row.live_activity.as_mut().unwrap().expires_in_ms = 59_000;
        let mut after = before.clone();
        after.lsn = Lsn(2);
        after.agents = vec![row.clone()];
        assert!(delta(&before, &after).agents.is_empty());
        row.role = "reviewer".into();
        after.agents = vec![row];
        assert_eq!(delta(&before, &after).agents[0].role, "reviewer");
        assert_eq!(
            delta(&before, &after).agents[0]
                .live_activity
                .unwrap()
                .expires_in_ms,
            59_000
        );
    }

    #[test]
    fn the_daemon_sibling_joins_as_the_last_key_and_is_left_out_over_the_cap() {
        let frame = serialize_snapshot(&AgentSnapshot {
            finished_today: None,
            cards: Vec::new(),
            activity: Vec::new(),
            question_history: Vec::new(),
            traces: Vec::new(),
            mail: dudes_journal::projections::MailView::default(),
            retired_agents: Vec::new(),
            lsn: Lsn(3),
            agents: vec![agent("engineer".to_owned())],
        })
        .expect("a snapshot");
        let read = |frame: &[u8]| -> Value {
            serde_json::from_slice(text_payload(frame).expect("a text frame")).expect("JSON")
        };
        let joined = with_sibling(&frame, "daemon", br#"{"providers":[]}"#).expect("joined");
        let mut expected = read(&frame);
        expected["daemon"] = serde_json::json!({"providers": []});
        assert_eq!(read(&joined), expected);

        let open = text_payload(&frame).expect("a text frame").len() - 1;
        let fits = vec![b' '; MAX_MESSAGE_BYTES - open - 11];
        assert!(
            with_sibling(&frame, "daemon", &fits).is_some(),
            "control: a join of exactly 1 MiB is sent"
        );
        let over = vec![b' '; MAX_MESSAGE_BYTES - open - 10];
        assert_eq!(with_sibling(&frame, "daemon", &over), None);
    }

    #[test]
    fn browser_message_types_are_closed_and_nested_input_is_bounded() {
        assert_eq!(
            decode_client(r#"{"type":"floor.subscribe"}"#).unwrap(),
            ClientMessage::Subscribe {}
        );
        assert_eq!(
            decode_client(r#"{"type":"floor.unsubscribe"}"#).unwrap(),
            ClientMessage::Unsubscribe {}
        );
        assert!(decode_client(r#"{"type":"floor.write"}"#).is_err());
        assert!(decode_client(r#"{"type":"floor.subscribe","extra":true}"#).is_err());
        assert!(decode_client(&format!("{}{}", "[".repeat(17), "]".repeat(17))).is_err());
        assert!(client_type_is_listed("floor.subscribe"));
        assert!(!client_type_is_listed("floor.write"));
        assert!(SERVER_TYPES.iter().all(|kind| server_type_is_listed(kind)));
    }

    #[test]
    fn output_client_messages_are_exact_read_filters() {
        let agent = AgentId::from_parts(1, 1);
        assert_eq!(
            decode_client(&format!(
                r#"{{"type":"floor.output.select","agent_id":"{agent}"}}"#
            ))
            .unwrap(),
            ClientMessage::OutputSelect {
                agent_id: agent,
                view: OutputView::Output
            }
        );
        assert_eq!(
            decode_client(r#"{"type":"floor.output.clear"}"#).unwrap(),
            ClientMessage::OutputClear {}
        );
        for invalid in [
            r#"{"type":"floor.output.select"}"#,
            r#"{"type":"floor.output.select","agent_id":"bad"}"#,
            r#"{"type":"floor.output.select","agent_id":"0","selection_epoch":1}"#,
            r#"{"type":"floor.output.clear","agent_id":"0"}"#,
            r#"{"type":"floor.output.chunk","text":"write"}"#,
        ] {
            assert!(decode_client(invalid).is_err(), "admitted {invalid}");
        }
        assert!(client_type_is_listed("floor.output.select"));
        assert!(client_type_is_listed("floor.output.clear"));
    }

    /// Each client message is one object: the derived decoder of an internally tagged enum would
    /// also read one from an array led by its tag, so every array is refused first.
    #[test]
    fn only_an_object_is_a_client_message() {
        assert_eq!(
            decode_client(" \n{\"type\":\"floor.subscribe\"}").ok(),
            Some(ClientMessage::Subscribe {}),
            "stream: leading whitespace before the object"
        );
        let agent = AgentId::from_parts(1, 1);
        let admitted: Vec<String> = [
            r#"["floor.subscribe"]"#.to_owned(),
            r#"["floor.unsubscribe"]"#.to_owned(),
            format!(r#"["floor.output.select","{agent}"]"#),
            r#"["floor.output.clear"]"#.to_owned(),
        ]
        .into_iter()
        .filter(|array| decode_client(array).is_ok())
        .collect();
        assert!(admitted.is_empty(), "admitted {admitted:?}");
    }

    fn queued_json(read: QueueRead) -> Value {
        let QueueRead::Frame(frame) = read else {
            panic!("expected a queued browser frame");
        };
        let marker = usize::from(frame[1] & 0x7f);
        let (head, len) = match marker {
            0..=125 => (2, marker),
            126 => (4, usize::from(u16::from_be_bytes([frame[2], frame[3]]))),
            127 => {
                let bytes: [u8; 8] = frame[2..10].try_into().unwrap();
                (10, usize::try_from(u64::from_be_bytes(bytes)).unwrap())
            }
            _ => unreachable!(),
        };
        serde_json::from_slice(&frame[head..head + len]).unwrap()
    }

    fn next_output(subscription: &Subscription) -> Value {
        queued_json(subscription.next(Duration::from_secs(2)))
    }

    fn reply_fixture() -> (
        FloorStreams,
        Arc<OutputRegistry>,
        AgentId,
        TurnId,
        dudes_supervisor::host::AdoptionKey,
    ) {
        let output = Arc::new(OutputRegistry::new());
        let row = agent("reply".into());
        let id = row.id;
        let turn = TurnId::from_parts(200, 1);
        let key = dudes_supervisor::host::AdoptionKey {
            pid: 201,
            start_time: 1,
            boot: None,
        };
        output.replace(id, turn);
        assert!(output.register(id, turn, &key));
        output.confirm(id, turn, &dudes_types::SessionId::new("reply"));
        let mut store = dudes_journal::Projections::in_memory().unwrap();
        let mut snapshot = crate::floor::snapshot(&mut store, Timestamp::UNIX_EPOCH).unwrap();
        snapshot.agents.push(row);
        let streams = FloorStreams::with_output(
            snapshot,
            FloorStreamFault::default().without_heartbeat(),
            Arc::clone(&output),
        )
        .unwrap();
        let snapshot = streams.0.state.lock().unwrap().current.clone();
        streams
            .publish_closing_with_chat_output(
                snapshot,
                None,
                Some(dudes_proto::ChatView::empty()),
                || {},
            )
            .unwrap();
        (streams, output, id, turn, key)
    }

    #[test]
    fn reply_stream_ignores_unrelated_agent_activity_while_live_and_ended() {
        let (streams, output, id, turn, key) = reply_fixture();
        output.capture(&key, "reply", "unchanged reply");
        let other = AgentId::from_parts(210, 1);
        let other_turn = TurnId::from_parts(211, 1);
        let other_key = dudes_supervisor::host::AdoptionKey {
            pid: 212,
            start_time: 1,
            boot: None,
        };
        output.replace(other, other_turn);
        assert!(output.register(other, other_turn, &other_key));
        output.confirm(other, other_turn, &dudes_types::SessionId::new("other"));
        let opened = streams.subscribe().unwrap();
        streams
            .select_output(&opened.subscription, id, OutputView::Reply)
            .unwrap();
        next_output(&opened.subscription);
        for ended in [false, true] {
            if ended {
                output.end(id, turn);
                assert_eq!(next_output(&opened.subscription)["ended"], true);
            }
            for _ in 0..300 {
                output.capture(&other_key, "other", "other agent text");
                assert!(refresh_output(
                    &streams.0,
                    &opened.subscription.queue,
                    id,
                    1
                ));
                let queue = opened.subscription.queue.state.lock().unwrap();
                assert!(
                    queue.frames.is_empty(),
                    "another agent must not republish this reply"
                );
                assert_eq!(queue.terminal, LIVE);
                assert_eq!(
                    queue.selection.unwrap().revision,
                    output.snapshot(id).revision,
                    "the wake cursor must still advance"
                );
            }
        }
    }

    #[test]
    fn reply_read_mode_is_an_exact_optional_selector() {
        let agent = AgentId::from_parts(200, 1);
        assert_eq!(
            decode_client(&format!(
                r#"{{"type":"floor.output.select","agent_id":"{agent}","view":"reply"}}"#
            ))
            .unwrap(),
            ClientMessage::OutputSelect {
                agent_id: agent,
                view: OutputView::Reply
            }
        );
        for view in ["null", "false", "{}", "\"unknown\""] {
            assert!(
                decode_client(&format!(
                    r#"{{"type":"floor.output.select","agent_id":"{agent}","view":{view}}}"#
                ))
                .is_err()
            );
        }
        assert!(server_type_is_listed("floor.output.reply"));
    }

    #[test]
    fn reply_stream_keeps_prefix_after_raw_gap_and_reconnects_after_end() {
        let (streams, output, id, turn, key) = reply_fixture();
        let opened = streams.subscribe().unwrap();
        streams
            .select_output(&opened.subscription, id, OutputView::Reply)
            .unwrap();
        let initial = next_output(&opened.subscription);
        assert_eq!(initial["type"], "floor.output.reply");
        assert_eq!(initial["text"], "");
        streams.0.fault.hold_next_output_wake(id);
        assert!(streams.0.fault.wait_output_wake(Duration::from_secs(2)));
        output.capture(&key, "reply", "prefix ");
        for _ in 0..256 {
            output.capture(&key, "reply", "x");
        }
        streams.0.fault.release_output_wake();
        let live = next_output(&opened.subscription);
        assert_eq!(live["type"], "floor.output.reply");
        assert_eq!(live["agent_id"], serde_json::json!(id));
        assert_eq!(live["turn_id"], serde_json::json!(turn));
        assert_eq!(live["selection_epoch"], 1);
        assert_eq!(live["text"], format!("prefix {}", "x".repeat(256)));
        assert_eq!(live["truncated"], false);
        assert_eq!(live["gap"], Value::Null);
        assert_eq!(live["ended"], false);
        let revision = live["revision"].as_str().unwrap().parse::<u128>().unwrap();
        assert!(
            revision
                > initial["revision"]
                    .as_str()
                    .unwrap()
                    .parse::<u128>()
                    .unwrap()
        );
        assert_eq!(live["revision"], revision.to_string());
        output.end(id, turn);
        let ended = next_output(&opened.subscription);
        assert_eq!(ended["ended"], true);
        assert_eq!(ended["text"], live["text"]);
        assert!(ended["revision"].as_str().unwrap().parse::<u128>().unwrap() > revision);
        let reconnect = streams.subscribe().unwrap();
        streams
            .select_output(&reconnect.subscription, id, OutputView::Reply)
            .unwrap();
        assert_eq!(next_output(&reconnect.subscription), ended);
        streams
            .select_output(&opened.subscription, id, OutputView::Output)
            .unwrap();
        let raw = next_output(&opened.subscription);
        assert_eq!(raw["type"], "floor.output.snapshot");
        assert_eq!(raw["selection_epoch"], 2);
        assert_eq!(raw["first_seq"], 129);
        assert_eq!(raw["next_seq"], 257);
        assert_eq!(raw["chunks"].as_array().unwrap().len(), 128);
        assert_eq!(raw["truncated"], true);
    }

    #[test]
    fn reply_old_wake_cannot_cross_mode_or_turn_selection() {
        let (streams, output, id, _, key) = reply_fixture();
        let opened = streams.subscribe().unwrap();
        streams
            .select_output(&opened.subscription, id, OutputView::Reply)
            .unwrap();
        next_output(&opened.subscription);
        streams.0.fault.hold_next_output_wake(id);
        assert!(streams.0.fault.wait_output_wake(Duration::from_secs(2)));
        output.capture(&key, "reply", "old reply");
        streams
            .select_output(&opened.subscription, id, OutputView::Output)
            .unwrap();
        let raw = next_output(&opened.subscription);
        assert_eq!(raw["type"], "floor.output.snapshot");
        assert_eq!(raw["selection_epoch"], 2);
        let next_turn = TurnId::from_parts(202, 1);
        output.replace(id, next_turn);
        streams
            .select_output(&opened.subscription, id, OutputView::Reply)
            .unwrap();
        let unbound = next_output(&opened.subscription);
        assert_eq!(unbound["reason"], "unbound");
        assert_eq!(unbound["selection_epoch"], 3);
        let new_key = dudes_supervisor::host::AdoptionKey {
            pid: 203,
            start_time: 2,
            boot: None,
        };
        assert!(output.register(id, next_turn, &new_key));
        output.confirm(id, next_turn, &dudes_types::SessionId::new("new"));
        output.capture(&new_key, "new", "new reply");
        streams.0.fault.release_output_wake();
        let new = next_output(&opened.subscription);
        assert_eq!(new["type"], "floor.output.reply");
        assert_eq!(new["selection_epoch"], 3);
        assert_eq!(new["turn_id"], serde_json::json!(next_turn));
        assert_eq!(new["text"], "new reply");
        streams.clear_output(&opened.subscription).unwrap();
        let cleared = next_output(&opened.subscription);
        assert_eq!(cleared["reason"], "cleared");
        assert_eq!(cleared["selection_epoch"], 4);
    }

    #[test]
    fn reply_serialization_and_socket_queue_stay_bounded() {
        let (streams, output, id, _, key) = reply_fixture();
        output.capture(&key, "reply", &"\0".repeat(65_537));
        let opened = streams.subscribe().unwrap();
        streams
            .select_output(&opened.subscription, id, OutputView::Reply)
            .unwrap();
        let QueueRead::Frame(frame) = opened.subscription.next(Duration::from_secs(2)) else {
            panic!("expected reply frame");
        };
        assert!(frame.len() < MAX_MESSAGE_BYTES);
        let reply: Value = serde_json::from_slice(text_payload(&frame).unwrap()).unwrap();
        assert_eq!(reply["text"].as_str().unwrap().len(), 65_536);
        assert_eq!(reply["gap"], "reply_truncation");
        assert_eq!(reply["truncated"], true);
        let mut queue = opened.subscription.queue.state.lock().unwrap();
        while queue
            .enqueue_frame(Arc::clone(&frame), QueueKind::Output)
            .is_ok()
        {
            assert!(queue.bytes <= MAX_QUEUE_BYTES);
            assert!(queue.frames.len() <= MAX_QUEUE_MESSAGES);
        }
        assert_eq!(queue.terminal, OVERFLOWED);
        assert!(queue.bytes <= MAX_QUEUE_BYTES);
    }

    #[test]
    fn provider_omission_is_a_revisioned_live_reply_gap_without_fake_text() {
        let (streams, output, id, _, key) = reply_fixture();
        let opened = streams.subscribe().unwrap();
        streams
            .select_output(&opened.subscription, id, OutputView::Reply)
            .unwrap();
        let initial = next_output(&opened.subscription);
        output.omit(&key, "reply");
        let gap = next_output(&opened.subscription);
        assert_eq!(gap["type"], "floor.output.reply");
        assert_eq!(gap["text"], "");
        assert_eq!(gap["gap"], "provider_limit");
        assert_eq!(gap["truncated"], true);
        assert_eq!(gap["ended"], false);
        assert!(
            gap["revision"].as_str().unwrap().parse::<u128>().unwrap()
                > initial["revision"]
                    .as_str()
                    .unwrap()
                    .parse::<u128>()
                    .unwrap()
        );
        output.capture(&key, "reply", "retained suffix");
        let retained = next_output(&opened.subscription);
        assert_eq!(retained["text"], "retained suffix");
        assert_eq!(retained["gap"], "provider_limit");
    }

    #[test]
    fn output_select_clear_reselect_has_checked_epochs_and_preserves_projection() {
        let output = Arc::new(crate::output::OutputRegistry::new());
        let row = agent("reader".to_owned());
        let id = row.id;
        let streams = FloorStreams::with_output(
            AgentSnapshot {
                finished_today: None,
                cards: Vec::new(),
                activity: Vec::new(),
                question_history: Vec::new(),
                traces: Vec::new(),
                mail: dudes_journal::projections::MailView::default(),
                retired_agents: Vec::new(),
                lsn: Lsn(1),
                agents: vec![row.clone()],
            },
            FloorStreamFault::default().without_heartbeat(),
            output,
        )
        .unwrap();
        let opened = streams.subscribe().unwrap();
        streams
            .select_output(&opened.subscription, id, OutputView::Output)
            .unwrap();
        assert_eq!(
            queued_json(opened.subscription.next(Duration::ZERO)),
            serde_json::json!({
                "type":"floor.output.unavailable",
                "selection_epoch":1,
                "agent_id":id,
                "reason":"no_run_output"
            })
        );
        streams
            .publish(AgentSnapshot {
                finished_today: None,
                cards: Vec::new(),
                activity: Vec::new(),
                question_history: Vec::new(),
                traces: Vec::new(),
                mail: dudes_journal::projections::MailView::default(),
                retired_agents: Vec::new(),
                lsn: Lsn(2),
                agents: vec![row],
            })
            .unwrap();
        streams.clear_output(&opened.subscription).unwrap();
        assert_eq!(
            queued_json(opened.subscription.next(Duration::ZERO))["type"],
            "floor.delta",
            "clearing output must retain projection deltas"
        );
        assert_eq!(
            queued_json(opened.subscription.next(Duration::ZERO)),
            serde_json::json!({
                "type":"floor.output.unavailable",
                "selection_epoch":2,
                "agent_id":id,
                "reason":"cleared"
            })
        );
        streams
            .select_output(&opened.subscription, id, OutputView::Output)
            .unwrap();
        assert_eq!(
            queued_json(opened.subscription.next(Duration::ZERO))["selection_epoch"],
            3
        );
    }

    #[test]
    fn output_notice_does_not_run_when_floor_publication_refuses() {
        let streams = registry();
        let notified = std::cell::Cell::new(false);
        let oversized = AgentSnapshot {
            finished_today: None,
            cards: Vec::new(),
            activity: Vec::new(),
            question_history: Vec::new(),
            traces: Vec::new(),
            mail: dudes_journal::projections::MailView::default(),
            retired_agents: Vec::new(),
            lsn: Lsn(1),
            agents: vec![agent("x".repeat(MAX_MESSAGE_BYTES))],
        };
        assert!(
            streams
                .publish_with_output(oversized, || notified.set(true))
                .is_err()
        );
        assert!(!notified.get(), "fallible preflight precedes output notice");
    }

    #[test]
    fn output_unbound_then_snapshot_chunk_and_ended_are_ordered() {
        use dudes_supervisor::host::AdoptionKey;
        use dudes_types::SessionId;

        let output = Arc::new(OutputRegistry::new());
        let row = agent("live".into());
        let id = row.id;
        let turn = TurnId::from_parts(2, 1);
        let key = AdoptionKey {
            pid: 11,
            start_time: 22,
            boot: None,
        };
        let streams = FloorStreams::with_output(
            AgentSnapshot {
                finished_today: None,
                cards: Vec::new(),
                activity: Vec::new(),
                question_history: Vec::new(),
                traces: Vec::new(),
                mail: dudes_journal::projections::MailView::default(),
                retired_agents: Vec::new(),
                lsn: Lsn(1),
                agents: vec![row],
            },
            FloorStreamFault::default().without_heartbeat(),
            Arc::clone(&output),
        )
        .unwrap();
        let opened = streams.subscribe().unwrap();
        streams
            .select_output(&opened.subscription, id, OutputView::Output)
            .unwrap();
        assert_eq!(next_output(&opened.subscription)["reason"], "no_run_output");

        output.replace(id, turn);
        output.register(id, turn, &key);
        assert_eq!(next_output(&opened.subscription)["reason"], "unbound");
        output.confirm(id, turn, &SessionId::new("owned"));
        let snapshot = next_output(&opened.subscription);
        assert_eq!(snapshot["type"], "floor.output.snapshot");
        assert_eq!(snapshot["turn_id"], serde_json::json!(turn));
        assert_eq!(snapshot["chunks"], serde_json::json!([]));
        assert_eq!(snapshot["selection_epoch"], 1);

        output.capture(&key, "wrong", "refused");
        output.capture(&key, "owned", "visible");
        let chunk = next_output(&opened.subscription);
        assert_eq!(chunk["type"], "floor.output.chunk");
        assert_eq!(chunk["seq"], 0);
        assert_eq!(chunk["text"], "visible");
        assert_eq!(chunk["selection_epoch"], 1);
        output.end(id, turn);
        let ended = next_output(&opened.subscription);
        assert_eq!(ended["type"], "floor.output.ended");
        assert_eq!(ended["turn_id"], serde_json::json!(turn));
    }

    #[test]
    fn output_old_wake_rechecks_ring_after_prepared_floor_commit() {
        use dudes_supervisor::host::AdoptionKey;
        use dudes_types::SessionId;

        let output = Arc::new(OutputRegistry::new());
        let row = agent("replacement".into());
        let id = row.id;
        let first = TurnId::from_parts(7, 1);
        let second = TurnId::from_parts(8, 1);
        let key = AdoptionKey {
            pid: 31,
            start_time: 32,
            boot: None,
        };
        output.replace(id, first);
        output.register(id, first, &key);
        output.confirm(id, first, &SessionId::new("s"));
        output.capture(&key, "s", "first-only");
        let fault = FloorStreamFault::default().without_heartbeat();
        let streams = FloorStreams::with_output(
            AgentSnapshot {
                finished_today: None,
                cards: Vec::new(),
                activity: Vec::new(),
                question_history: Vec::new(),
                traces: Vec::new(),
                mail: dudes_journal::projections::MailView::default(),
                retired_agents: Vec::new(),
                lsn: Lsn(1),
                agents: vec![row.clone()],
            },
            fault.clone(),
            Arc::clone(&output),
        )
        .unwrap();
        let opened = streams.subscribe().unwrap();
        streams
            .select_output(&opened.subscription, id, OutputView::Output)
            .unwrap();
        assert_eq!(
            next_output(&opened.subscription)["type"],
            "floor.output.snapshot"
        );

        fault.hold_next_output_wake(id);
        output.capture(&key, "s", "must-not-cross");
        assert!(fault.wait_output_wake(Duration::from_secs(2)));
        streams
            .publish_with_output(
                AgentSnapshot {
                    finished_today: None,
                    cards: Vec::new(),
                    activity: Vec::new(),
                    question_history: Vec::new(),
                    traces: Vec::new(),
                    mail: dudes_journal::projections::MailView::default(),
                    retired_agents: Vec::new(),
                    lsn: Lsn(2),
                    agents: vec![row],
                },
                || output.replace(id, second),
            )
            .unwrap();
        fault.hold_next_output_refresh();
        fault.release_output_wake();
        fault.wait_output_refresh();
        assert_eq!(next_output(&opened.subscription)["type"], "floor.delta");
        let waiting = next_output(&opened.subscription);
        assert_eq!(waiting["type"], "floor.output.unavailable");
        assert_eq!(waiting["reason"], "unbound");
        assert_eq!(waiting["selection_epoch"], 1);
        assert!(!waiting.to_string().contains("must-not-cross"));
        fault.release_output_refresh();
        let new_key = AdoptionKey {
            pid: 33,
            start_time: 34,
            boot: None,
        };
        output.register(id, second, &new_key);
        output.confirm(id, second, &SessionId::new("new"));
        let snapshot = next_output(&opened.subscription);
        assert_eq!(snapshot["type"], "floor.output.snapshot");
        assert_eq!(snapshot["turn_id"], serde_json::json!(second));
        assert_eq!(snapshot["chunks"], serde_json::json!([]));
    }

    #[test]
    fn output_initial_sequence_overflow_acknowledges_suffix_then_gaps_and_disarms() {
        use dudes_supervisor::host::AdoptionKey;
        use dudes_types::SessionId;

        let output = Arc::new(OutputRegistry::new());
        let row = agent("sealed".into());
        let id = row.id;
        let turn = TurnId::from_parts(14, 1);
        output.replace(id, turn);
        assert!(output.register(
            id,
            turn,
            &AdoptionKey {
                pid: 44,
                start_time: 45,
                boot: None,
            }
        ));
        output.confirm(id, turn, &SessionId::new("sealed"));
        output.seal_sequence_for_test(id);
        let streams = FloorStreams::with_output(
            AgentSnapshot {
                finished_today: None,
                cards: Vec::new(),
                activity: Vec::new(),
                question_history: Vec::new(),
                traces: Vec::new(),
                mail: dudes_journal::projections::MailView::default(),
                retired_agents: Vec::new(),
                lsn: Lsn(1),
                agents: vec![row],
            },
            FloorStreamFault::default().without_heartbeat(),
            output,
        )
        .unwrap();
        let opened = streams.subscribe().unwrap();
        streams
            .select_output(&opened.subscription, id, OutputView::Output)
            .unwrap();
        let snapshot = next_output(&opened.subscription);
        assert_eq!(snapshot["type"], "floor.output.snapshot");
        assert_eq!(snapshot["selection_epoch"], 1);
        assert_eq!(snapshot["truncated"], true);
        assert_eq!(snapshot["chunks"][0]["text"], "retained-suffix");
        let gap = next_output(&opened.subscription);
        assert_eq!(gap["type"], "floor.output.gap");
        assert_eq!(gap["selection_epoch"], 1);
        assert_eq!(gap["reason"], "sequence_overflow");
        assert!(
            opened
                .subscription
                .queue
                .state
                .lock()
                .unwrap()
                .selection
                .is_none()
        );
    }

    #[test]
    fn output_never_drops_a_full_socket_queue_and_watcher_joins_on_close() {
        use dudes_supervisor::host::AdoptionKey;
        use dudes_types::SessionId;

        let output = Arc::new(OutputRegistry::new());
        let row = agent("flood".into());
        let id = row.id;
        let turn = TurnId::from_parts(19, 1);
        let key = AdoptionKey {
            pid: 51,
            start_time: 52,
            boot: None,
        };
        output.replace(id, turn);
        output.register(id, turn, &key);
        output.confirm(id, turn, &SessionId::new("flood"));
        let streams = FloorStreams::with_output(
            AgentSnapshot {
                finished_today: None,
                cards: Vec::new(),
                activity: Vec::new(),
                question_history: Vec::new(),
                traces: Vec::new(),
                mail: dudes_journal::projections::MailView::default(),
                retired_agents: Vec::new(),
                lsn: Lsn(1),
                agents: vec![row],
            },
            FloorStreamFault::default().without_heartbeat(),
            Arc::clone(&output),
        )
        .unwrap();
        let opened = streams.subscribe().unwrap();
        streams
            .select_output(&opened.subscription, id, OutputView::Output)
            .unwrap();
        assert_eq!(
            next_output(&opened.subscription)["type"],
            "floor.output.snapshot"
        );
        let weak = Arc::downgrade(&opened.subscription.queue);
        for count in 1..=MAX_QUEUE_MESSAGES + 1 {
            output.capture(&key, "flood", "piece");
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                let (queued, _, overflowed) = opened.subscription.queue.usage();
                if count <= MAX_QUEUE_MESSAGES && queued == count || overflowed {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "output queue stalled at {queued}/{count}"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        assert!(matches!(
            opened.subscription.next(Duration::ZERO),
            QueueRead::Overflowed
        ));
        let started = std::time::Instant::now();
        drop(opened);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "watcher did not join"
        );
        assert!(
            weak.upgrade().is_none(),
            "the watcher retained its dead socket queue"
        );
    }

    #[test]
    fn output_clear_purges_only_old_output_and_reselects_same_turn() {
        use dudes_supervisor::host::AdoptionKey;
        use dudes_types::SessionId;

        let output = Arc::new(OutputRegistry::new());
        let row = agent("same-turn".into());
        let id = row.id;
        let turn = TurnId::from_parts(25, 1);
        let key = AdoptionKey {
            pid: 61,
            start_time: 62,
            boot: None,
        };
        output.replace(id, turn);
        output.register(id, turn, &key);
        output.confirm(id, turn, &SessionId::new("s"));
        let streams = FloorStreams::with_output(
            AgentSnapshot {
                finished_today: None,
                cards: Vec::new(),
                activity: Vec::new(),
                question_history: Vec::new(),
                traces: Vec::new(),
                mail: dudes_journal::projections::MailView::default(),
                retired_agents: Vec::new(),
                lsn: Lsn(1),
                agents: vec![row.clone()],
            },
            FloorStreamFault::default().without_heartbeat(),
            Arc::clone(&output),
        )
        .unwrap();
        let opened = streams.subscribe().unwrap();
        streams
            .select_output(&opened.subscription, id, OutputView::Output)
            .unwrap();
        assert_eq!(next_output(&opened.subscription)["selection_epoch"], 1);
        output.capture(&key, "s", "held-callback");
        let held = next_output(&opened.subscription);
        assert_eq!(held["selection_epoch"], 1);
        output.capture(&key, "s", "queued-to-purge");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while opened.subscription.queue.usage().0 == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "old chunk never queued"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        streams
            .publish(AgentSnapshot {
                finished_today: None,
                cards: Vec::new(),
                activity: Vec::new(),
                question_history: Vec::new(),
                traces: Vec::new(),
                mail: dudes_journal::projections::MailView::default(),
                retired_agents: Vec::new(),
                lsn: Lsn(2),
                agents: vec![row],
            })
            .unwrap();
        streams.clear_output(&opened.subscription).unwrap();
        assert_eq!(next_output(&opened.subscription)["type"], "floor.delta");
        let cleared = next_output(&opened.subscription);
        assert_eq!(cleared["type"], "floor.output.unavailable");
        assert_eq!(cleared["reason"], "cleared");
        assert_eq!(cleared["selection_epoch"], 2);
        streams
            .select_output(&opened.subscription, id, OutputView::Output)
            .unwrap();
        let again = next_output(&opened.subscription);
        assert_eq!(again["type"], "floor.output.snapshot");
        assert_eq!(again["selection_epoch"], 3);
        assert_eq!(again["turn_id"], serde_json::json!(turn));
        assert_eq!(again["chunks"].as_array().unwrap().len(), 2);
        assert_eq!(held["selection_epoch"], 1);
    }

    #[test]
    fn output_attached_projection_disarms_even_without_registry_notice() {
        let output = Arc::new(OutputRegistry::new());
        let row = agent("mode".into());
        let id = row.id;
        let streams = FloorStreams::with_output(
            AgentSnapshot {
                finished_today: None,
                cards: Vec::new(),
                activity: Vec::new(),
                question_history: Vec::new(),
                traces: Vec::new(),
                mail: dudes_journal::projections::MailView::default(),
                retired_agents: Vec::new(),
                lsn: Lsn(1),
                agents: vec![row.clone()],
            },
            FloorStreamFault::default().without_heartbeat(),
            output,
        )
        .unwrap();
        let opened = streams.subscribe().unwrap();
        streams
            .select_output(&opened.subscription, id, OutputView::Output)
            .unwrap();
        assert_eq!(next_output(&opened.subscription)["reason"], "no_run_output");
        let mut attached = row;
        attached.mode = Mode::Attached;
        streams
            .publish(AgentSnapshot {
                finished_today: None,
                cards: Vec::new(),
                activity: Vec::new(),
                question_history: Vec::new(),
                traces: Vec::new(),
                mail: dudes_journal::projections::MailView::default(),
                retired_agents: Vec::new(),
                lsn: Lsn(2),
                agents: vec![attached],
            })
            .unwrap();
        assert_eq!(next_output(&opened.subscription)["type"], "floor.delta");
        let unavailable = next_output(&opened.subscription);
        assert_eq!(unavailable["reason"], "attached");
        assert_eq!(unavailable["selection_epoch"], 1);
        assert!(
            opened
                .subscription
                .queue
                .state
                .lock()
                .unwrap()
                .selection
                .is_none()
        );
    }

    #[test]
    fn output_unknown_retired_and_attached_selects_reveal_no_turn_or_text() {
        let output = Arc::new(OutputRegistry::new());
        let mut attached = agent("attached".into());
        attached.mode = Mode::Attached;
        let retired = AgentId::from_parts(91, 1);
        let unknown = AgentId::from_parts(92, 1);
        output.retire(retired);
        let streams = FloorStreams::with_output(
            AgentSnapshot {
                finished_today: None,
                cards: Vec::new(),
                activity: Vec::new(),
                question_history: Vec::new(),
                traces: Vec::new(),
                mail: dudes_journal::projections::MailView::default(),
                retired_agents: Vec::new(),
                lsn: Lsn(1),
                agents: vec![attached.clone()],
            },
            FloorStreamFault::default().without_heartbeat(),
            output,
        )
        .unwrap();
        let opened = streams.subscribe().unwrap();
        for (agent, reason, epoch) in [
            (unknown, "unknown", 1),
            (retired, "retired", 2),
            (attached.id, "attached", 3),
        ] {
            streams
                .select_output(&opened.subscription, agent, OutputView::Output)
                .unwrap();
            let frame = next_output(&opened.subscription);
            assert_eq!(frame["type"], "floor.output.unavailable");
            assert_eq!(frame["reason"], reason);
            assert_eq!(frame["selection_epoch"], epoch);
            assert!(frame.get("turn_id").is_none());
            assert!(frame.get("text").is_none());
        }
        assert!(
            opened
                .subscription
                .queue
                .state
                .lock()
                .unwrap()
                .selection
                .is_none()
        );
        assert!(streams.clear_output(&opened.subscription).is_err());
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one ordered waiting-loss, fresh-reselect and replacement-turn regression"
    )]
    fn output_pending_overflow_must_gap_and_disarm_after_confirmation() {
        use dudes_supervisor::host::AdoptionKey;
        use dudes_types::SessionId;

        let output = Arc::new(OutputRegistry::new());
        let row = agent("pending".into());
        let id = row.id;
        let turn = TurnId::from_parts(95, 1);
        let key = AdoptionKey {
            pid: 96,
            start_time: 97,
            boot: None,
        };
        let streams = FloorStreams::with_output(
            AgentSnapshot {
                finished_today: None,
                cards: Vec::new(),
                activity: Vec::new(),
                question_history: Vec::new(),
                traces: Vec::new(),
                mail: dudes_journal::projections::MailView::default(),
                retired_agents: Vec::new(),
                lsn: Lsn(1),
                agents: vec![row],
            },
            FloorStreamFault::default().without_heartbeat(),
            Arc::clone(&output),
        )
        .unwrap();
        let opened = streams.subscribe().unwrap();
        streams
            .select_output(&opened.subscription, id, OutputView::Output)
            .unwrap();
        assert_eq!(next_output(&opened.subscription)["reason"], "no_run_output");
        output.replace(id, turn);
        assert!(output.register(id, turn, &key));
        assert_eq!(next_output(&opened.subscription)["reason"], "unbound");
        for _ in 0..129 {
            output.capture(&key, "s", "pending-piece");
        }
        output.confirm(id, turn, &SessionId::new("s"));
        let snapshot = next_output(&opened.subscription);
        assert_eq!(snapshot["type"], "floor.output.snapshot");
        assert_eq!(snapshot["selection_epoch"], 1);
        assert_eq!(snapshot["turn_id"], serde_json::json!(turn));
        assert_eq!(snapshot["truncated"], true);
        assert_eq!(snapshot["chunks"].as_array().unwrap().len(), 128);
        let gap = next_output(&opened.subscription);
        assert_eq!(gap["type"], "floor.output.gap");
        assert_eq!(gap["selection_epoch"], 1);
        assert_eq!(gap["turn_id"], serde_json::json!(turn));
        assert_eq!(gap["reason"], "pending_overflow");
        assert!(
            opened
                .subscription
                .queue
                .state
                .lock()
                .unwrap()
                .selection
                .is_none()
        );
        streams
            .select_output(&opened.subscription, id, OutputView::Output)
            .unwrap();
        let fresh = next_output(&opened.subscription);
        assert_eq!(fresh["type"], "floor.output.snapshot");
        assert_eq!(fresh["selection_epoch"], 2);
        assert_eq!(fresh["truncated"], true);
        assert_eq!(fresh["chunks"].as_array().unwrap().len(), 128);
        assert!(
            opened
                .subscription
                .queue
                .state
                .lock()
                .unwrap()
                .selection
                .is_some()
        );
        output.capture(&key, "s", "fresh-followup");
        let live = next_output(&opened.subscription);
        assert_eq!(live["type"], "floor.output.chunk");
        assert_eq!(live["selection_epoch"], 2);
        assert_eq!(live["text"], "fresh-followup");

        let replacement = TurnId::from_parts(98, 1);
        let replacement_key = AdoptionKey {
            pid: 99,
            start_time: 100,
            boot: None,
        };
        output.replace(id, replacement);
        assert!(output.register(id, replacement, &replacement_key));
        assert_eq!(next_output(&opened.subscription)["reason"], "unbound");
        for _ in 0..129 {
            output.capture(&replacement_key, "new", "replacement-piece");
        }
        output.confirm(id, replacement, &SessionId::new("new"));
        let replacement_snapshot = next_output(&opened.subscription);
        assert_eq!(replacement_snapshot["type"], "floor.output.snapshot");
        assert_eq!(replacement_snapshot["selection_epoch"], 2);
        assert_eq!(
            replacement_snapshot["turn_id"],
            serde_json::json!(replacement)
        );
        let replacement_gap = next_output(&opened.subscription);
        assert_eq!(replacement_gap["type"], "floor.output.gap");
        assert_eq!(replacement_gap["selection_epoch"], 2);
        assert_eq!(replacement_gap["reason"], "pending_overflow");
        assert!(
            opened
                .subscription
                .queue
                .state
                .lock()
                .unwrap()
                .selection
                .is_none()
        );
    }

    #[test]
    fn output_pending_loss_still_disarms_when_visible_trim_arrives_before_refresh() {
        use dudes_supervisor::host::AdoptionKey;
        use dudes_types::SessionId;

        let output = Arc::new(OutputRegistry::new());
        let row = agent("pending-race".into());
        let id = row.id;
        let turn = TurnId::from_parts(111, 1);
        let key = AdoptionKey {
            pid: 112,
            start_time: 113,
            boot: None,
        };
        let fault = FloorStreamFault::default().without_heartbeat();
        let streams = FloorStreams::with_output(
            AgentSnapshot {
                finished_today: None,
                cards: Vec::new(),
                activity: Vec::new(),
                question_history: Vec::new(),
                traces: Vec::new(),
                mail: dudes_journal::projections::MailView::default(),
                retired_agents: Vec::new(),
                lsn: Lsn(1),
                agents: vec![row],
            },
            fault.clone(),
            Arc::clone(&output),
        )
        .unwrap();
        let opened = streams.subscribe().unwrap();
        streams
            .select_output(&opened.subscription, id, OutputView::Output)
            .unwrap();
        assert_eq!(next_output(&opened.subscription)["reason"], "no_run_output");
        output.replace(id, turn);
        assert!(output.register(id, turn, &key));
        assert_eq!(next_output(&opened.subscription)["reason"], "unbound");
        for _ in 0..129 {
            output.capture(&key, "s", "pending-piece");
        }

        // Engaged before the confirmation: a pass already past its wake check would otherwise read
        // it, gap and disarm, and never reach the hold.
        fault.hold_next_output_wake(id);
        assert!(fault.wait_output_wake(Duration::from_secs(2)));
        output.confirm(id, turn, &SessionId::new("s"));
        output.capture(&key, "s", "visible-before-refresh");
        fault.hold_next_output_refresh();
        fault.release_output_wake();
        fault.wait_output_refresh();

        assert_eq!(
            opened.subscription.queue.usage().0,
            2,
            "the first bound-turn snapshot must be followed by its terminal loss gap"
        );
        let snapshot = next_output(&opened.subscription);
        assert_eq!(snapshot["type"], "floor.output.snapshot");
        assert_eq!(snapshot["selection_epoch"], 1);
        assert_eq!(snapshot["turn_id"], serde_json::json!(turn));
        assert_eq!(snapshot["first_seq"], 1);
        assert_eq!(snapshot["next_seq"], 129);
        assert_eq!(snapshot["truncated"], true);
        assert_eq!(snapshot["chunks"].as_array().unwrap().len(), 128);
        let gap = next_output(&opened.subscription);
        assert_eq!(gap["type"], "floor.output.gap");
        assert_eq!(gap["selection_epoch"], 1);
        assert_eq!(gap["turn_id"], serde_json::json!(turn));
        assert_eq!(gap["next_seq"], 129);
        assert_eq!(gap["reason"], "ring_truncation");
        assert!(
            opened
                .subscription
                .queue
                .state
                .lock()
                .unwrap()
                .selection
                .is_none()
        );
        fault.release_output_refresh();

        streams
            .select_output(&opened.subscription, id, OutputView::Output)
            .unwrap();
        let fresh = next_output(&opened.subscription);
        assert_eq!(fresh["type"], "floor.output.snapshot");
        assert_eq!(fresh["selection_epoch"], 2);
        assert_eq!(fresh["first_seq"], 1);
        assert_eq!(fresh["truncated"], true);
        assert_eq!(fresh["chunks"].as_array().unwrap().len(), 128);
        output.capture(&key, "s", "fresh-live");
        let live = next_output(&opened.subscription);
        assert_eq!(live["type"], "floor.output.chunk");
        assert_eq!(live["selection_epoch"], 2);
        assert_eq!(live["seq"], 129);
        assert_eq!(live["text"], "fresh-live");
    }

    #[test]
    fn output_wake_hold_armed_during_an_in_flight_pass_still_holds_the_confirmation() {
        use dudes_supervisor::host::AdoptionKey;
        use dudes_types::SessionId;

        let output = Arc::new(OutputRegistry::new());
        let row = agent("in-flight".into());
        let id = row.id;
        let turn = TurnId::from_parts(121, 1);
        let key = AdoptionKey {
            pid: 122,
            start_time: 123,
            boot: None,
        };
        let fault = FloorStreamFault::default().without_heartbeat();
        let streams = FloorStreams::with_output(
            AgentSnapshot {
                finished_today: None,
                cards: Vec::new(),
                activity: Vec::new(),
                question_history: Vec::new(),
                traces: Vec::new(),
                mail: dudes_journal::projections::MailView::default(),
                retired_agents: Vec::new(),
                lsn: Lsn(1),
                agents: vec![row],
            },
            fault.clone(),
            Arc::clone(&output),
        )
        .unwrap();
        let opened = streams.subscribe().unwrap();
        streams
            .select_output(&opened.subscription, id, OutputView::Output)
            .unwrap();
        assert_eq!(next_output(&opened.subscription)["reason"], "no_run_output");
        output.replace(id, turn);
        assert!(output.register(id, turn, &key));
        assert_eq!(next_output(&opened.subscription)["reason"], "unbound");
        for _ in 0..129 {
            output.capture(&key, "s", "pending-piece");
        }

        // What load does: one watcher pass is past its wake check and has not yet read the
        // registry when the hold is armed.
        fault.pause_next_output_pass(id);
        assert!(
            fault.wait_output_pass(Duration::from_secs(2)),
            "no output watcher pass passed its wake check"
        );
        fault.hold_next_output_wake(id);
        fault.release_output_pass();
        assert!(
            fault.wait_output_wake(Duration::from_secs(2)),
            "selection before the wake check: {:?}",
            opened.subscription.queue.state.lock().unwrap().selection
        );
        output.confirm(id, turn, &SessionId::new("s"));
        fault.hold_next_output_refresh();
        fault.release_output_wake();
        fault.wait_output_refresh();

        let snapshot = next_output(&opened.subscription);
        assert_eq!(snapshot["type"], "floor.output.snapshot");
        assert_eq!(snapshot["first_seq"], 0);
        assert_eq!(snapshot["next_seq"], 128);
        assert_eq!(snapshot["truncated"], true);
        let gap = next_output(&opened.subscription);
        assert_eq!(gap["type"], "floor.output.gap");
        assert_eq!(gap["reason"], "pending_overflow");
        assert!(
            opened
                .subscription
                .queue
                .state
                .lock()
                .unwrap()
                .selection
                .is_none()
        );
        fault.release_output_refresh();
    }

    #[test]
    fn outbound_serialization_is_bounded() {
        let oversized = "x".repeat(MAX_MESSAGE_BYTES + 128);
        let mut retained = CappedJson::new();
        serde_json::to_writer(&mut retained, &serde_json::json!({"value": oversized})).unwrap();
        assert!(retained.overflowed);
        assert_eq!(retained.bytes.len(), MAX_MESSAGE_BYTES + 1);

        let view = agent("x".repeat(MAX_MESSAGE_BYTES));
        let snapshot = AgentSnapshot {
            finished_today: None,
            cards: Vec::new(),
            activity: Vec::new(),
            question_history: Vec::new(),
            traces: Vec::new(),
            mail: dudes_journal::projections::MailView::default(),
            retired_agents: Vec::new(),
            lsn: Lsn(1),
            agents: vec![view.clone()],
        };
        assert!(serialize_snapshot(&snapshot).is_err());
        assert!(
            serialize_frame(&DeltaMessage {
                finished_today: None,
                chat: None,
                kind: "floor.delta",
                delta: FloorDeltaView {
                    delta: &FloorDelta {
                        lsn: Lsn(2),
                        agents: vec![view],
                        ..FloorDelta::default()
                    },
                    closing_progress: None
                },
                question_history: None,
                traces: None,
                journal_stopped: None,
                messages: None,
                messages_lsn: "0".into(),
                threads: None,
                threads_lsn: "0".into(),
            })
            .is_err()
        );
    }

    #[test]
    fn subscription_churn_without_publication_is_bounded() {
        let streams = FloorStreams::new(
            AgentSnapshot {
                finished_today: None,
                cards: Vec::new(),
                activity: Vec::new(),
                question_history: Vec::new(),
                traces: Vec::new(),
                mail: dudes_journal::projections::MailView::default(),
                retired_agents: Vec::new(),
                lsn: Lsn::ZERO,
                agents: Vec::new(),
            },
            FloorStreamFault::default(),
        )
        .unwrap();
        let live = streams.subscribe().unwrap();
        for _ in 0..1024 {
            drop(streams.subscribe().unwrap());
        }
        assert!(
            streams
                .0
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .subscriptions
                .len()
                <= 2,
            "one live subscription plus at most the latest dead entry"
        );
        drop(live);
        drop(streams.subscribe().unwrap());
        assert_eq!(
            streams
                .0
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .subscriptions
                .len(),
            1,
            "the next insertion prunes every dead historical connection"
        );
    }

    /// A registry with nothing to beat for, over a span far longer than several periods.
    ///
    /// The budget condition, and the assertion is on the **live** count rather than on
    /// the subscription vector being unchanged: the emitter prunes dead `Weak` entries exactly as
    /// `subscribe` and `publish` do, so "the list is unchanged" would fail a correctly pruning
    /// emitter and pass a broken one that never looked.
    #[test]
    fn no_live_subscription_means_no_beat() {
        let streams = registry();
        assert_eq!(
            streams.heartbeat(),
            0,
            "a registry nobody reads beat anyway"
        );

        // A subscription that has been dropped is a dead `Weak`, not an absent one.
        drop(streams.subscribe().unwrap());
        for beat in 0..8 {
            assert_eq!(
                streams.heartbeat(),
                0,
                "beat {beat} reached a subscriber that is gone"
            );
        }
        assert!(
            live_subscriptions(&streams) == 0,
            "the dead entry was not pruned on the way past"
        );
    }

    /// One live subscriber, and the frame is exactly the heartbeat frame.
    #[test]
    fn a_beat_is_the_whole_frame_and_carries_nothing() {
        let streams = registry();
        let opened = streams.subscribe().unwrap();
        assert_eq!(streams.heartbeat(), 1);

        let QueueRead::Frame(frame) = opened.subscription.next(Duration::from_secs(5)) else {
            panic!("the live subscriber received no heartbeat");
        };
        // Byte-equal against the framing of the exact expected payload — not "contains", and
        // not a decode that would accept extra keys. A head `lsn` added on the way past is the
        // field the client must not act on, and this notices it.
        assert_eq!(
            frame.as_ref(),
            text_frame(br#"{"type":"floor.heartbeat"}"#)
                .unwrap()
                .as_slice(),
            "the heartbeat frame is not exactly its documented shape"
        );
        assert!(server_type_is_listed("floor.heartbeat"));
    }

    /// The queue bound applies to heartbeats like any other frame.
    ///
    /// A subscriber that stays registered and never drains fills `MAX_QUEUE_MESSAGES` on beats
    /// alone and is then closed, which is the right direction: a wedged reader is
    /// released rather than held for ever. Driven by calling the emitter directly rather than by
    /// waiting: at the shipped period this is 256 x 2 s = 8 min 32 s, and the bound is what is
    /// under test, not the clock.
    #[test]
    fn a_never_draining_subscriber_is_closed_by_heartbeats_alone() {
        let streams = registry();
        let opened = streams.subscribe().unwrap();

        for beat in 0..MAX_QUEUE_MESSAGES {
            assert_eq!(
                streams.heartbeat(),
                1,
                "beat {beat} of {MAX_QUEUE_MESSAGES} did not reach the subscriber"
            );
        }
        // The next one overflows the queue, which closes it and drops the subscriber.
        assert_eq!(
            streams.heartbeat(),
            0,
            "the queue accepted more than its bound"
        );
        assert_eq!(
            live_subscriptions(&streams),
            0,
            "an overflowed subscriber stayed registered"
        );
        assert!(
            matches!(
                opened.subscription.next(Duration::from_secs(5)),
                QueueRead::Overflowed
            ),
            "the subscriber was not told its stream had overflowed"
        );
    }

    /// The timer beats on its own, and only while something is subscribed.
    #[test]
    fn the_timer_beats_for_a_live_subscriber() {
        let streams = registry();
        let opened = streams.subscribe().unwrap();
        let task = streams.start_heartbeat(Duration::from_millis(5));

        for beat in 0..3 {
            let QueueRead::Frame(frame) = opened.subscription.next(Duration::from_secs(5)) else {
                panic!("beat {beat} never arrived from the timer");
            };
            assert_eq!(
                frame.as_ref(),
                text_frame(br#"{"type":"floor.heartbeat"}"#)
                    .unwrap()
                    .as_slice()
            );
        }
        task.stop();
    }

    /// **the task itself is gone**, not merely that a tick after `end` reaches
    /// nobody.
    ///
    /// Those two are different claims, and the second matters: a timer that looks cancelled but is still scheduled is how a heartbeat
    /// outlives the stream it claims to speak for. So it is observed twice, and deliberately by two mechanisms
    /// that do not share a failure:
    ///
    /// 1. `stop()` returns, which means `JoinHandle::join` returned, which means the thread exited;
    /// 2. a `Weak` to a marker the thread's own frame owns fails to upgrade — an observation that
    ///    holds even if `stop` were ever reduced to a signal with no join.
    ///
    /// Assertion 2 is the one that survives the obvious regression, which is why it is here.
    #[test]
    fn the_timer_task_is_gone_after_the_registry_ends() {
        let streams = registry();
        let opened = streams.subscribe().unwrap();
        let task = streams.start_heartbeat(Duration::from_millis(5));
        let probe = task.alive_probe();

        // Anti-vacuity: a task that never started would pass both assertions below.
        assert!(
            matches!(
                opened.subscription.next(Duration::from_secs(5)),
                QueueRead::Frame(_)
            ),
            "the timer never beat, so this run would prove nothing about stopping it"
        );
        assert!(
            probe.upgrade().is_some(),
            "the thread's marker was already gone while it was still beating"
        );

        streams.end();
        task.stop();

        assert!(
            probe.upgrade().is_none(),
            "the heartbeat thread's frame is still alive after the registry ended and the task was \
             stopped: a timer that is merely idle is still scheduled"
        );
        assert_eq!(
            streams.heartbeat(),
            0,
            "an ended registry beat again, so the per-tick no-op is broken too"
        );
    }

    /// The other way a registry stops: nobody calls `end`, the value is simply dropped.
    ///
    /// This is the path an ordinary shutdown takes — `end()` is reached from one error arm in
    /// `runtime.rs` and from nowhere else — so a timer keyed only to `end` would outlive every
    /// clean stop. The thread holds a `Weak` and notices on its next tick.
    #[test]
    fn the_timer_task_is_gone_after_the_registry_is_dropped() {
        let streams = registry();
        let task = streams.start_heartbeat(Duration::from_millis(5));
        let probe = task.alive_probe();
        assert!(probe.upgrade().is_some());

        drop(streams);
        task.stop();

        assert!(
            probe.upgrade().is_none(),
            "the heartbeat thread outlived the registry it beats for"
        );
    }

    /// A beat reaches a closed socket's subscriber not at all, and drops it.
    ///
    /// The client decides when a stream has gone stale; what this proves is the daemon half of that — a
    /// subscription whose queue has ended receives nothing more, so nothing the page could mistake
    /// for confidence is produced for a stream that is gone.
    #[test]
    fn a_beat_reaches_no_closed_subscriber() {
        let streams = registry();
        let opened = streams.subscribe().unwrap();
        assert_eq!(streams.heartbeat(), 1);
        // Dropping the subscription ends its queue, which is what a closed transport does.
        drop(opened);

        for beat in 0..4 {
            assert_eq!(
                streams.heartbeat(),
                0,
                "beat {beat} reached a subscriber whose stream had closed"
            );
        }
        assert_eq!(live_subscriptions(&streams), 0);
    }

    /// Stopping the timer leaves the registry exactly as it found it.
    ///
    /// The ownership direction runs one way: a registry that ends takes
    /// its timer with it. The inverse — a timer whose stop ends the stream — would mean every
    /// browser lost its floor because the daemon tore down a keepalive. This is what notices.
    #[test]
    fn stopping_the_timer_does_not_end_the_stream() {
        let streams = registry();
        let opened = streams.subscribe().unwrap();
        let task = streams.start_heartbeat(Duration::from_millis(5));
        assert!(
            matches!(
                opened.subscription.next(Duration::from_secs(5)),
                QueueRead::Frame(_)
            ),
            "the timer never beat, so stopping it would prove nothing"
        );

        task.stop();

        // The registry is still open for business on every path that an ended one refuses.
        let after = streams
            .subscribe()
            .expect("stopping the timer ended the registry");
        assert!(
            streams
                .publish(AgentSnapshot {
                    finished_today: None,
                    cards: Vec::new(),
                    activity: Vec::new(),
                    question_history: Vec::new(),
                    traces: Vec::new(),
                    mail: dudes_journal::projections::MailView::default(),
                    retired_agents: Vec::new(),
                    lsn: Lsn(1),
                    agents: Vec::new(),
                })
                .is_ok(),
            "stopping the timer ended the registry's publish path"
        );
        assert!(
            matches!(
                after.subscription.next(Duration::from_secs(5)),
                QueueRead::Frame(_)
            ),
            "a subscriber taken after the timer stopped received nothing"
        );
        assert!(
            !matches!(
                opened.subscription.next(Duration::from_secs(5)),
                QueueRead::Ended
            ),
            "the subscriber that outlived the timer had its stream ended with it"
        );
    }

    /// `stop` returns on the notification, not on the next tick.
    ///
    /// The thread parks on the `Condvar` rather than sleeping, so a long period costs nothing at
    /// shutdown. Asserted against an hour so the margin is not a matter of taste: a `stop` that
    /// waited out its period would take 3 600 s and this takes milliseconds.
    #[test]
    fn stop_returns_without_waiting_out_a_period() {
        let streams = registry();
        let task = streams.start_heartbeat(Duration::from_secs(3_600));
        let probe = task.alive_probe();
        assert!(probe.upgrade().is_some());

        let started = std::time::Instant::now();
        task.stop();
        let waited = started.elapsed();

        assert!(probe.upgrade().is_none(), "the thread outlived its stop");
        assert!(
            waited < Duration::from_secs(30),
            "stopping a timer on an hour-long period took {waited:?}: it waited out the period \
             rather than being woken"
        );
    }

    fn ready(date: &str, offset: i32, total: u64, lsn: u64) -> FloorFinishedToday {
        FloorFinishedToday::Ready {
            local_date: date.to_owned(),
            utc_offset_minutes: offset,
            total,
            through_lsn: lsn,
        }
    }

    fn registry_with(finished_today: Option<FloorFinishedToday>) -> FloorStreams {
        FloorStreams::new(
            AgentSnapshot {
                finished_today,
                ..registry_snapshot()
            },
            FloorStreamFault::default(),
        )
        .unwrap()
    }

    fn queued_snapshot(frame: &[u8]) -> Value {
        serde_json::from_slice(text_payload(frame).expect("a text frame")).expect("snapshot JSON")
    }

    fn snapshot_with(finished_today: Option<FloorFinishedToday>, lsn: u64) -> AgentSnapshot {
        AgentSnapshot {
            finished_today,
            lsn: Lsn(lsn),
            ..registry_snapshot()
        }
    }

    fn registry_snapshot() -> AgentSnapshot {
        AgentSnapshot {
            finished_today: None,
            cards: Vec::new(),
            activity: Vec::new(),
            question_history: Vec::new(),
            traces: Vec::new(),
            mail: dudes_journal::projections::MailView::default(),
            retired_agents: Vec::new(),
            lsn: Lsn::ZERO,
            agents: Vec::new(),
        }
    }

    /// The sibling rides the snapshot, a delta carries it only when it
    /// changed and then replaces it whole, and a stream that has none never invents a zero.
    #[test]
    fn finished_today_rides_the_snapshot_and_a_changed_delta_replaces_it_whole() {
        let streams = registry_with(Some(ready("2026-10-07", 780, 1, 4)));
        let opened = streams.subscribe().unwrap();
        let snapshot = queued_snapshot(&opened.snapshot);
        assert_eq!(
            snapshot["finished_today"],
            serde_json::json!({
                "state": "ready", "local_date": "2026-10-07", "utc_offset_minutes": 780,
                "total": "1", "through_lsn": "4",
            })
        );
        streams
            .publish(snapshot_with(Some(ready("2026-10-07", 780, 2, 9)), 9))
            .unwrap();
        let delta = queued_json(opened.subscription.next(Duration::from_secs(2)));
        assert_eq!(delta["type"], "floor.delta");
        assert_eq!(
            delta["finished_today"],
            serde_json::json!({
                "state": "ready", "local_date": "2026-10-07", "utc_offset_minutes": 780,
                "total": "2", "through_lsn": "9",
            })
        );
        streams
            .publish(snapshot_with(Some(ready("2026-10-07", 780, 2, 9)), 10))
            .unwrap();
        let unchanged = queued_json(opened.subscription.next(Duration::from_secs(2)));
        assert!(
            unchanged.get("finished_today").is_none(),
            "an unchanged sibling is not resent: {unchanged}"
        );
        streams
            .publish(snapshot_with(Some(FloorFinishedToday::Unavailable), 11))
            .unwrap();
        let withdrawn = queued_json(opened.subscription.next(Duration::from_secs(2)));
        assert_eq!(
            withdrawn["finished_today"],
            serde_json::json!({"state": "unavailable"})
        );
        streams
            .publish(snapshot_with(Some(FloorFinishedToday::Unavailable), 12))
            .unwrap();
        let still = queued_json(opened.subscription.next(Duration::from_secs(2)));
        assert!(
            still.get("finished_today").is_none(),
            "an unavailable sibling that stays unavailable is not resent: {still}"
        );
        let late = streams.subscribe().unwrap();
        assert_eq!(
            queued_snapshot(&late.snapshot)["finished_today"],
            serde_json::json!({"state": "unavailable"}),
            "a later subscriber is told it is unavailable, never zero"
        );
    }

    #[test]
    fn a_stream_without_the_sibling_sends_none_and_a_subscription_reads_the_day_afresh() {
        let none = registry_with(None);
        let opened = none.subscribe().unwrap();
        assert!(
            queued_snapshot(&opened.snapshot)
                .get("finished_today")
                .is_none()
        );
        none.publish(snapshot_with(None, 3)).unwrap();
        assert!(
            queued_json(opened.subscription.next(Duration::from_secs(2)))
                .get("finished_today")
                .is_none()
        );

        let stale = registry_with(Some(ready("2026-10-06", 780, 7, 2)));
        let fresh = ready("2026-10-07", 780, 0, 2);
        let opened = stale
            .subscribe_with_siblings(
                Siblings {
                    finished_today: Some(&fresh),
                    ..Siblings::default()
                },
                None,
                Timestamp::UNIX_EPOCH,
            )
            .unwrap();
        assert_eq!(
            queued_snapshot(&opened.snapshot)["finished_today"]["local_date"],
            "2026-10-07",
            "the day the caller read at subscription wins over the cached one"
        );
    }

    fn day(date: &str, offset: i32, start: i64, end: i64) -> crate::local_day::DayWindow {
        crate::local_day::DayWindow {
            local_date: date.to_owned(),
            utc_offset_minutes: offset,
            start: Timestamp::from_unix_millis(start),
            end: Timestamp::from_unix_millis(end),
        }
    }

    fn assert_quiet(opened: &Opened) {
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            matches!(
                opened.subscription.next(Duration::from_millis(1)),
                QueueRead::Empty
            ),
            "nothing is sent while the day is unchanged"
        );
    }

    fn assert_resync(opened: &Opened, why: &str) {
        let QueueRead::Frame(frame) = opened.subscription.next(Duration::from_secs(5)) else {
            panic!("no resync after {why}");
        };
        assert_eq!(
            frame.as_ref(),
            text_frame(br#"{"type":"floor.resync"}"#)
                .unwrap()
                .as_slice(),
            "{why}"
        );
    }

    /// A change in the date, the window bounds or the offset sends the
    /// existing contentless resync exactly once, and a repeated observation sends nothing.
    #[test]
    fn the_day_watch_resyncs_once_when_the_date_the_window_or_the_offset_changes() {
        let streams = registry();
        let opened = streams.subscribe().unwrap();
        let current = Arc::new(Mutex::new(Some(day("2026-10-07", 780, 1_000, 2_000))));
        let source = Arc::clone(&current);
        let task = streams.start_day_watch(
            crate::local_day::LocalDays::from_fn(move || {
                source
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
            }),
            Duration::from_millis(5),
        );
        assert_quiet(&opened);

        *current.lock().unwrap() = Some(day("2026-10-07", 780, 1_000, 2_000));
        assert_quiet(&opened);

        *current.lock().unwrap() = Some(day("2026-10-07", 720, 1_000, 2_000));
        assert_resync(&opened, "an offset change with the same date and bounds");
        assert_quiet(&opened);

        *current.lock().unwrap() = Some(day("2026-10-07", 720, 500, 2_500));
        assert_resync(&opened, "a window change with the same date and offset");
        assert_quiet(&opened);

        *current.lock().unwrap() = Some(day("2026-10-08", 720, 2_500, 3_500));
        assert_resync(&opened, "a new local date");
        assert_quiet(&opened);

        *current.lock().unwrap() = None;
        assert_resync(&opened, "the zone becoming unusable");
        assert_quiet(&opened);
        task.stop();
    }

    /// A subscription that sampled one day and registered after the next began would carry the old
    /// day with no timer broadcast left to correct it. The second sample catches it.
    #[test]
    fn a_subscription_that_straddles_a_day_change_is_told_to_resync() {
        let samples = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&samples);
        let days = crate::local_day::LocalDays::from_fn(move || {
            let call = counter.fetch_add(1, Ordering::SeqCst);
            Some(if call == 0 {
                day("2026-10-07", 780, 1_000, 2_000)
            } else {
                day("2026-10-08", 780, 2_000, 3_000)
            })
        });
        let streams = registry();
        let opened = streams
            .subscribe_on_day(
                &days,
                |sampled| {
                    assert_eq!(sampled.map(|d| d.local_date.as_str()), Some("2026-10-07"));
                    ready("2026-10-07", 780, 0, 1)
                },
                Siblings::default(),
                None,
                Timestamp::UNIX_EPOCH,
            )
            .unwrap();
        assert_eq!(
            queued_snapshot(&opened.snapshot)["finished_today"]["local_date"],
            "2026-10-07"
        );
        assert_resync(&opened, "the day moved while the subscription registered");

        let steady =
            crate::local_day::LocalDays::from_fn(|| Some(day("2026-10-07", 780, 1_000, 2_000)));
        let quiet = streams
            .subscribe_on_day(
                &steady,
                |_| ready("2026-10-07", 780, 0, 1),
                Siblings::default(),
                None,
                Timestamp::UNIX_EPOCH,
            )
            .unwrap();
        assert_quiet(&quiet);
    }

    fn registry() -> FloorStreams {
        registry_with(None)
    }

    fn live_subscriptions(streams: &FloorStreams) -> usize {
        streams
            .0
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .subscriptions
            .iter()
            .filter(|subscription| subscription.strong_count() > 0)
            .count()
    }
}
