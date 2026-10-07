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
pub(crate) enum Idle<'a> {
    /// Messages arrived; continue with them.
    Deliver(Taken<'a>),
    /// Children are still running or a send is in flight.
    Wait,
    /// Nothing can arrive any more. The mailbox is now closed.
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
    /// Takes every queued envelope in arrival order.
    pub(crate) fn take_all(&self) -> Taken<'_> {
        let envelopes = {
            let mut state = self.lock();
            state.plain = 0;
            state.queue.drain(..).collect()
        };
        Taken {
            mailbox: self,
            envelopes,
        }
    }
    /// Takes the queued terminal notices of the given children.
    pub(crate) fn take_notices(&self, children: &BTreeSet<NodeId>) -> Taken<'_> {
        let envelopes = {
            let mut state = self.lock();
            let (notices, rest): (VecDeque<_>, VecDeque<_>) =
                state.queue.drain(..).partition(|envelope| {
                    envelope.kind != MessageKind::Message && children.contains(&envelope.from)
                });
            state.queue = rest;
            notices.into()
        };
        Taken {
            mailbox: self,
            envelopes,
        }
    }
    fn requeue(&self, envelopes: Vec<Envelope>) {
        {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            for envelope in envelopes.into_iter().rev() {
                state.plain += usize::from(envelope.kind == MessageKind::Message);
                state.queue.push_front(envelope);
            }
        }
        self.wake();
    }
    /// Decides atomically between delivering, waiting and closing, so a message
    /// is either delivered or refused to its sender, never accepted and dropped.
    pub(crate) fn idle(&self) -> Idle<'_> {
        let mut state = self.lock();
        if !state.queue.is_empty() {
            state.plain = 0;
            let envelopes = state.queue.drain(..).collect();
            return Idle::Deliver(Taken {
                mailbox: self,
                envelopes,
            });
        }
        if state.awaiting.is_empty() && state.reserved == 0 {
            state.closed = true;
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
/// Envelopes taken from a mailbox. Dropped before `finish`, they return to the
/// front of the queue, so a cancelled delivery loses nothing.
pub(crate) struct Taken<'a> {
    mailbox: &'a Mailbox,
    envelopes: Vec<Envelope>,
}
impl Taken<'_> {
    pub(crate) fn is_empty(&self) -> bool {
        self.envelopes.is_empty()
    }
    pub(crate) fn ids(&self) -> Vec<MessageId> {
        self.envelopes.iter().map(|envelope| envelope.id).collect()
    }
    pub(crate) fn finish(mut self) -> Vec<Envelope> {
        std::mem::take(&mut self.envelopes)
    }
}
impl Drop for Taken<'_> {
    fn drop(&mut self) {
        if !self.envelopes.is_empty() {
            self.mailbox.requeue(std::mem::take(&mut self.envelopes));
        }
    }
}
/// A reserved plain message on its way into a mailbox. Dropped before `push`, it
/// is queued anyway, so the reservation is always settled.
pub(crate) struct Pending<'a> {
    mailbox: &'a Mailbox,
    message: Option<Envelope>,
}
impl<'a> Pending<'a> {
    pub(crate) fn new(mailbox: &'a Mailbox, message: Envelope) -> Self {
        Self {
            mailbox,
            message: Some(message),
        }
    }
    pub(crate) fn push(mut self) -> Result<(), Envelope> {
        let message = self.message.take().expect("message pushed once");
        self.mailbox.push(message)
    }
}
impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if let Some(message) = self.message.take() {
            let _ = self.mailbox.push(message);
        }
    }
}
