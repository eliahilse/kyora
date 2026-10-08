//! Agent mailboxes: message envelopes, bounded queues and delivery bookkeeping.
//!
//! Every agent node owns one mailbox. Plain messages are bounded by
//! `Limits::mailbox_capacity`: a send to a full mailbox fails and queues nothing.
//! Terminal notices of node-owned children bypass the bound, since each child
//! produces exactly one and the agent limits already bound the children.
use crate::{AgentOutcome, NodeId, Status};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Mutex, MutexGuard},
};
use tokio::sync::watch;

/// Session-unique message identifier, allocated in send order.
pub type MessageId = u64;

/// What an envelope carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// Text sent by an agent.
    Message,
    /// A child completed; the body is its answer.
    Result,
    /// A child ended without completing; the status says why.
    Error,
    /// A child was cancelled.
    Cancelled,
}
impl MessageKind {
    /// Kind of the terminal notice for a child that ended with `status`.
    pub fn for_status(status: Status) -> Self {
        match status {
            Status::Completed => Self::Result,
            Status::Cancelled => Self::Cancelled,
            _ => Self::Error,
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Result => "result",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}
/// How a message left its recipient's mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Appended to the conversation at a turn boundary.
    Turn,
    /// Returned by a receive call.
    Receive,
    /// Consumed by a wait call that returned the child's outcome.
    Wait,
    /// Consumed by the cancel call that stopped the child, which returned its outcome.
    Cancel,
}
/// One message between two agents of the same tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Session-unique identifier.
    pub id: MessageId,
    /// Sending agent.
    pub from: NodeId,
    /// Receiving agent.
    pub to: NodeId,
    /// Message, result, error or cancelled.
    pub kind: MessageKind,
    /// Text body, bounded by `Limits::message_chars`.
    pub body: String,
    /// Time the sender handed the message over.
    pub sent_at: DateTime<Utc>,
    /// The spawned child this message belongs to, as its node id: set for traffic
    /// between a parent and its child in either direction, including the child's
    /// terminal notice, and absent between siblings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn: Option<NodeId>,
    /// Terminal status on result, error and cancelled notices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
}
impl Envelope {
    /// Formats the envelope as conversation text: a bracketed header naming the
    /// sender (and the status of a notice), then the body on the following lines.
    pub fn render(&self, sender: &str) -> String {
        render(self.kind, self.from, sender, self.status, &self.body)
    }
}
pub(crate) fn render(
    kind: MessageKind,
    from: NodeId,
    sender: &str,
    status: Option<Status>,
    body: &str,
) -> String {
    let mut text = format!("[{} from agent {from}", kind.as_str());
    if !sender.is_empty() {
        text.push_str(&format!(" ({sender})"));
    }
    if let Some(status) = status {
        let status = serde_json::to_value(status).expect("serializable status");
        text.push_str(&format!(": {}", status.as_str().unwrap_or("failed")));
    }
    text.push(']');
    if !body.is_empty() {
        text.push('\n');
        text.push_str(body);
    }
    text
}
/// Children that finished and children still running when a wait returns.
#[derive(Debug, Clone, Default)]
pub struct Waited {
    /// What the finished children had queued: their unread messages and terminal
    /// notices, in arrival order.
    pub messages: Vec<Envelope>,
    /// Outcomes of the waited children that finished, in node id order.
    pub finished: Vec<AgentOutcome>,
    /// Finished children whose messages did not all fit the delivery budget, with
    /// how many are still queued. Their notice is among them, last, so their order
    /// is kept; the next delivery hands them over.
    pub deferred: BTreeMap<NodeId, usize>,
    /// Waited children that were still running.
    pub running: Vec<NodeId>,
}
/// One delivery taken from a mailbox.
pub(crate) struct Batch {
    /// Envelopes handed over, in arrival order.
    pub(crate) taken: Vec<Envelope>,
    /// Selected envelopes that did not fit, by sender.
    pub(crate) left: BTreeMap<NodeId, usize>,
    /// Envelopes still queued, from any sender.
    pub(crate) queued: usize,
}
fn take(
    state: &mut State,
    budget: usize,
    pick: impl Fn(&Envelope) -> bool,
    size: impl Fn(&Envelope) -> usize,
) -> Batch {
    let mut taken = Vec::new();
    let mut left = BTreeMap::new();
    let mut rest = VecDeque::new();
    // Every delivery before the next model request shares one budget. The first
    // message of a turn always goes, so an oversized one is not stuck.
    let fresh = state.turn == 0;
    let mut used = state.turn;
    let mut full = false;
    for envelope in state.queue.drain(..) {
        if pick(&envelope) {
            if !full {
                let cost = size(&envelope);
                if (fresh && taken.is_empty()) || used + cost <= budget {
                    used += cost;
                    taken.push(envelope);
                    continue;
                }
                // Later envelopes stay behind this one, even smaller ones.
                full = true;
            }
            *left.entry(envelope.from).or_default() += 1;
        }
        rest.push_back(envelope);
    }
    state.queue = rest;
    state.turn = used;
    state.plain -= taken
        .iter()
        .filter(|envelope| envelope.kind == MessageKind::Message)
        .count();
    Batch {
        taken,
        left,
        queued: state.queue.len(),
    }
}
/// Tells a model how many messages are still waiting after a delivery.
pub(crate) fn more(count: usize) -> String {
    if count == 1 {
        "1 more message waiting; it follows at your next turn".into()
    } else {
        format!("{count} more messages waiting; they follow at your next turn")
    }
}
/// Tells a model that a child's messages, ending with `last`, did not all fit.
pub(crate) fn deferred(agent: NodeId, count: usize, last: &str) -> String {
    if count == 1 {
        format!("agent {agent} finished, but its {last} did not fit; it follows at your next turn")
    } else {
        format!(
            "agent {agent} finished, but {count} of its messages, its {last} last, did not fit; they follow at your next turn"
        )
    }
}

/// One agent's queue. Every change bumps a watch counter that wakes waiters.
pub(crate) struct Mailbox {
    state: Mutex<State>,
    changed: watch::Sender<u64>,
}
#[derive(Default)]
struct State {
    queue: VecDeque<Envelope>,
    /// Queued plain messages; together with `reserved` bounded by the capacity.
    plain: usize,
    /// Accepted plain messages whose send record is still being written.
    reserved: usize,
    /// Node-owned children whose terminal notice has not been queued yet.
    awaiting: BTreeSet<NodeId>,
    closed: bool,
    /// Characters of messages handed over since the agent's last model request.
    turn: usize,
    /// Children whose terminal entry has been queued, by them or on their behalf.
    concluded: BTreeSet<NodeId>,
}
/// Why a mailbox refused a plain message.
pub(crate) enum Refusal {
    Full,
    Closed,
}
/// What an agent with nothing left to do at the end of a turn does next.
pub(crate) enum Idle {
    /// Messages arrived and one delivery was taken; continue with it.
    Deliver(Batch),
    /// Children are still running or a send is in flight.
    Wait,
    /// Nothing can arrive any more. The mailbox is now closed if closing was asked.
    Done,
}
impl Default for Mailbox {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            changed: watch::Sender::new(0),
        }
    }
}
impl Mailbox {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("mailbox mutex poisoned")
    }
    fn wake(&self) {
        self.changed
            .send_modify(|version| *version = version.wrapping_add(1));
    }
    /// Subscribe before inspecting the mailbox so no change is missed.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }
    /// Reserves room for one plain message.
    pub(crate) fn reserve(&self, capacity: usize) -> Result<(), Refusal> {
        let mut state = self.lock();
        if state.closed {
            return Err(Refusal::Closed);
        }
        if state.plain + state.reserved >= capacity {
            return Err(Refusal::Full);
        }
        state.reserved += 1;
        Ok(())
    }
    /// Registers a child whose terminal notice this mailbox will receive.
    pub(crate) fn expect(&self, child: NodeId) {
        self.lock().awaiting.insert(child);
        self.wake();
    }
    /// Queues an envelope, or returns it when the mailbox is closed. A plain message
    /// consumes its reservation and a notice clears its child either way.
    pub(crate) fn push(&self, envelope: Envelope) -> Result<(), Envelope> {
        let result = {
            let mut state = self.lock();
            let plain = envelope.kind == MessageKind::Message;
            if plain {
                state.reserved -= 1;
            } else {
                state.awaiting.remove(&envelope.from);
                state.concluded.insert(envelope.from);
            }
            if state.closed {
                Err(envelope)
            } else {
                state.plain += usize::from(plain);
                state.queue.push_back(envelope);
                Ok(())
            }
        };
        self.wake();
        result
    }
    /// Whether a terminal entry for `child` has been queued.
    pub(crate) fn is_concluded(&self, child: NodeId) -> bool {
        self.lock().concluded.contains(&child)
    }
    /// Queues a terminal entry on behalf of a child that posts none of its own,
    /// behind everything it has queued. Returns false if one was queued before or
    /// the mailbox is closed.
    pub(crate) fn conclude(&self, notice: Envelope) -> bool {
        {
            let mut state = self.lock();
            if state.closed || !state.concluded.insert(notice.from) {
                return false;
            }
            state.queue.push_back(notice);
        }
        self.wake();
        true
    }
    /// Charges text that reports messages outside an envelope against this turn's
    /// budget. Returns false, charging nothing, when it does not fit; the first
    /// report of a turn always fits.
    pub(crate) fn charge(&self, cost: usize, budget: usize) -> bool {
        let mut state = self.lock();
        if state.turn > 0 && state.turn + cost > budget {
            return false;
        }
        state.turn += cost;
        true
    }
    /// Starts a new turn's delivery budget, right before the agent's next model request.
    pub(crate) fn new_turn(&self) {
        self.lock().turn = 0;
    }
    /// Wakes waiters without changing the queue, after a child publishes its outcome.
    pub(crate) fn touch(&self) {
        self.wake();
    }
    /// Takes one delivery: the queued envelopes `pick` selects, whole and in arrival
    /// order, until the next would push their `size` past `budget`; always at least
    /// one. Taking is final: the caller hands them over without an await in between,
    /// so nothing taken returns to the queue and the capacity bound holds. What does
    /// not fit stays queued, keeps its place and still counts against the capacity.
    pub(crate) fn take(
        &self,
        budget: usize,
        pick: impl Fn(&Envelope) -> bool,
        size: impl Fn(&Envelope) -> usize,
    ) -> Batch {
        take(&mut self.lock(), budget, pick, size)
    }
    /// Decides atomically between delivering, waiting and closing, so a message
    /// is either delivered or refused to its sender, never accepted and dropped.
    /// Without `close`, `Done` leaves the mailbox open for an agent that goes on.
    /// While anything is queued, even after a partial delivery, the agent goes on.
    pub(crate) fn idle(
        &self,
        close: bool,
        budget: usize,
        size: impl Fn(&Envelope) -> usize,
    ) -> Idle {
        let mut state = self.lock();
        if !state.queue.is_empty() {
            return Idle::Deliver(take(&mut state, budget, |_| true, size));
        }
        if state.awaiting.is_empty() && state.reserved == 0 {
            state.closed |= close;
            return Idle::Done;
        }
        Idle::Wait
    }
    /// Closes the mailbox and returns the envelopes nobody will deliver.
    pub(crate) fn close(&self) -> Vec<Envelope> {
        let envelopes = {
            let mut state = self.lock();
            state.closed = true;
            state.plain = 0;
            state.queue.drain(..).collect()
        };
        self.wake();
        envelopes
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.lock().closed
    }
    pub(crate) fn len(&self) -> usize {
        self.lock().queue.len()
    }
    pub(crate) fn is_awaiting(&self, child: NodeId) -> bool {
        self.lock().awaiting.contains(&child)
    }
    /// Children whose result has not been delivered: still running, or queued.
    pub(crate) fn outstanding(&self) -> BTreeSet<NodeId> {
        let state = self.lock();
        state
            .queue
            .iter()
            .filter(|envelope| envelope.kind != MessageKind::Message)
            .map(|envelope| envelope.from)
            .chain(state.awaiting.iter().copied())
            .collect()
    }
}
