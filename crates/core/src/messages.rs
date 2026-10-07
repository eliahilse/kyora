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
    collections::{BTreeSet, VecDeque},
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
    /// Waited children that were still running.
    pub running: Vec<NodeId>,
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
}
/// Why a mailbox refused a plain message.
pub(crate) enum Refusal {
    Full,
    Closed,
}
/// What an agent with nothing left to do at the end of a turn does next.
pub(crate) enum Idle {
    /// Messages arrived and were taken; continue with them.
    Deliver(Vec<Envelope>),
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
    /// Wakes waiters without changing the queue, after a child publishes its outcome.
    pub(crate) fn touch(&self) {
        self.wake();
    }
    /// Takes every queued envelope in arrival order. Taking is final: the caller
    /// hands them over without an await in between, so nothing taken returns to
    /// the queue and the capacity bound holds.
    pub(crate) fn take_all(&self) -> Vec<Envelope> {
        let mut state = self.lock();
        state.plain = 0;
        state.queue.drain(..).collect()
    }
    /// Takes everything queued from the given children, in arrival order. For a
    /// finished child that is its unread messages followed by its terminal notice,
    /// so the sender's order is kept.
    pub(crate) fn take_from(&self, children: &BTreeSet<NodeId>) -> Vec<Envelope> {
        let mut state = self.lock();
        let (taken, rest): (VecDeque<_>, VecDeque<_>) = state
            .queue
            .drain(..)
            .partition(|envelope| children.contains(&envelope.from));
        state.queue = rest;
        state.plain -= taken
            .iter()
            .filter(|envelope| envelope.kind == MessageKind::Message)
            .count();
        taken.into()
    }
    /// Decides atomically between delivering, waiting and closing, so a message
    /// is either delivered or refused to its sender, never accepted and dropped.
    /// Without `close`, `Done` leaves the mailbox open for an agent that goes on.
    pub(crate) fn idle(&self, close: bool) -> Idle {
        let mut state = self.lock();
        if !state.queue.is_empty() {
            state.plain = 0;
            return Idle::Deliver(state.queue.drain(..).collect());
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
