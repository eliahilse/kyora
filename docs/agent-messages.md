# Agent messages

Agents in one tree talk to each other asynchronously. A parent spawns a child and keeps working; the child's result arrives in the parent's mailbox as a message. The parent can send follow-ups to a running child, a child can report progress or ask its parent a question without ending, and siblings can message each other. Everything runs inside one process on top of the ownership, ledger and shutdown rules of the recursive runtime.

## Mailboxes

Every agent node (the root and each child agent) owns one mailbox. Leaf `llm` calls have none. A child's mailbox exists from the moment it is admitted, so a parent can message a child right after spawning it, before the child's first request.

A mailbox holds at most `Limits::mailbox_capacity` undelivered plain messages (default 64). A send to a full mailbox fails at once with `MailboxFull`: nothing is queued, nothing already queued is dropped, and the sender never blocks. Terminal notices of children are not counted against the bound. Each child produces exactly one, so the agent limits already bound them, and a result is never refused because a chatty sibling filled the mailbox.

Bodies are text and at most `Limits::message_chars` characters (default 20,000). Longer sends are refused. A child's answer in its terminal notice is shortened to the same cap, head and tail kept; the full answer stays in the child's `node_end` record and on its handle.

## Envelope

| Field | Meaning |
|---|---|
| `id` | Session-unique message id, allocated in send order. |
| `from`, `to` | Sending and receiving agent node ids. |
| `kind` | `message` (sent by an agent), `result` (child completed), `error` (child ended with another status), `cancelled` (child was cancelled). |
| `body` | Text. For notices, the child's answer, possibly partial. |
| `sent_at` | UTC time the sender handed the message over. |
| `spawn` | The spawned child the message belongs to, as its node id: set on traffic between a parent and its child in either direction and on the child's notice; absent between siblings. |
| `status` | The child's terminal status, on notices only. |

## Addressing

An agent can address its parent, its children and its siblings, and nothing else: not its grandparent, not an uncle or cousin, not a leaf call, not itself. An address is `parent`, a node id (`3` or `#3`), or the name given at spawn. Ids win over names. A name is looked up among the parent, the children and the siblings; an ambiguous name is refused and the caller must use the id.

## Delivery

A send returns a message id once the message is queued in the recipient's mailbox, or an error, in which case nothing was queued. Inside the process an accepted message leaves its mailbox exactly once, in one of two ways:

- delivered: appended to the recipient's conversation at a turn boundary, returned by `receive`, or (for a notice) consumed by `wait`;
- undelivered: still queued when the recipient ended, and recorded in the trace.

Nothing is duplicated and nothing disappears silently. Delivery into a conversation is at most once. The mailbox is a single FIFO queue, so messages from one sender arrive in send order, and a child's progress messages always arrive before its own result.

### Turn boundaries

Pending messages enter a conversation as user-role text, one text block per message, only when the runtime builds a user message that another model request will carry:

- the first user message, after the task;
- the tool-results message, after every `tool_result` block of the turn, never between them and never while a tool is running;
- the `continue` message after a `max_tokens` stop without tools;
- a new user message when an idle agent wakes up (below).

Messages are not taken when the turn is the last one (a committed final answer, cancellation, the deadline, the turn cap, or a stop reason that ends the agent). A `pause_turn` without tool calls re-sends the request unchanged, so its pending messages wait for the next boundary.

Each block starts with a header line naming the sender and, for notices, the status:

```
[message from agent 0 (root)]
also check the second file
[result from agent 2 (researcher): completed]
three dates found, listed below
[error from agent 3: max_turns]
partial notes
```

The name is omitted when the agent has none.

### Idle agents

When a model ends its turn (`end_turn`) while node-owned children are still running, or while messages are queued or being sent to it, the agent does not end. It waits without holding a model slot or a reservation, and starts its next turn with whatever arrived. It ends when nothing more can arrive: no child is running, no message is queued and no send is in flight. That check and the closing of the mailbox happen atomically, so a message is either delivered or refused to its sender, never accepted and then dropped. Each wait is bounded by the children's deadlines and the agent's own deadline. An agent that has reached its turn cap ends at once. A final answer committed by a tool also ends the agent at once and cancels its running children.

## Termination and cancellation

An agent's mailbox closes when its loop ends, before its children are cancelled. Later sends to it fail with `AgentFinished`. Messages still queued are recorded as `message_undelivered`. Node shutdown then runs in this order:

1. the agent loop ends and the mailbox closes; queued messages are recorded as undelivered;
2. admission closes, children are cancelled and joined; their notices find the mailbox closed and are recorded as undelivered;
3. `node_end` is written and the live-agent slot is released;
4. the node's handle resolves, then its own notice is posted to its parent.

Ownership decides who hears about a child's ending:

- A node-owned child (`Owner::Node`, the `spawn_agent` tool) is persistent. Its result, error or cancellation is posted to its parent's mailbox. Cancelling it through its handle produces a `cancelled` notice.
- A cell-owned child (`Owner::Cell`) belongs to the code that started it. That code reads the result from the handle, so no notice is posted. When the cell ends, the child is cancelled.

Both kinds can send, receive and be messaged.

## Budget and accounting

Messages consume no tokens, no model slots and no agent slots by themselves. Delivered text becomes part of the recipient's next request, so the reservation estimate counts its bytes and the provider charges it as input like any other user content. An idle agent holds no reservation. The ledger arithmetic, subtree budgets and per-node usage are unchanged.

## Deadlock freedom

Sends never block. A wait is only ever on the waiting agent's own children, so wait edges follow the node tree. `receive` is bounded by its `yield_after`. An idle agent waits only while its own children run or a send to it is in flight. Every wait is also bounded by the node deadline.

## Model-facing tools

`kyora_core::agent_tools::tools()` returns four tools. By default a child receives them when its parent holds them (see `defaults::SUBAGENT_TOOLS`).

| Tool | Arguments | Result |
|---|---|---|
| `spawn_agent` | `task`, optional `name`, `tools`, `budget`, `timeout` (seconds) | Starts a node-owned child and returns `started agent 3 (name)` at once. |
| `send_message` | `to` (`parent`, an id or a name), `body` | `sent message 7 to agent 3`, or an error such as a full mailbox or a finished agent. |
| `receive` | optional `yield_after` (seconds, default 0) | The pending messages, rendered as above; waits up to `yield_after` for the first one; `no messages` otherwise. |
| `wait` | optional `agents` (ids or names), optional `timeout` (seconds) | The results of the named children, or of every child whose result has not been delivered yet, plus the ones still running when the timeout passed. |

## Rust API

The tools are thin adapters over `NodeCtx`, which embedders use directly. Values are plain data and `Envelope` serializes to JSON, so a REPL supervisor can map each operation to one request over its own transport.

```rust
NodeCtx::spawn_agent(&self, spec: ChildSpec, owner: Owner) -> Result<AgentHandle, RecursionError>
NodeCtx::resolve(&self, address: &str) -> Result<NodeId, RecursionError>
NodeCtx::send(&self, to: NodeId, body: impl Into<String>)
    -> impl Future<Output = Result<MessageId, RecursionError>>
NodeCtx::receive(&self, yield_after: Duration)
    -> impl Future<Output = Result<Vec<Envelope>, RecursionError>>
NodeCtx::wait(&self, agents: Option<&[NodeId]>, timeout: Option<Duration>)
    -> impl Future<Output = Result<Waited, RecursionError>>
NodeCtx::pending_messages(&self) -> usize
NodeCtx::render(&self, message: &Envelope) -> String
NodeCtx::render_outcome(&self, outcome: &AgentOutcome) -> String
Envelope::render(&self, sender: &str) -> String
```

`Waited` lists the outcomes of the waited children that finished and the ids of those still running. `wait` takes the queued notices of the finished children, so their results are not delivered again at the next turn; `AgentHandle::result` is a plain observer and takes nothing. Errors: `MailboxFull` and `AgentFinished` for the recipient's state, `InvalidRequest` for unknown, ambiguous or unrelated addresses, messages to oneself and oversized bodies, and `Cancelled` when the calling agent has ended or is cancelled.

## Trace events

| Event | Fields |
|---|---|
| `message_sent` | The envelope, flattened. Written before the message can be delivered. |
| `message_delivered` | `node` (recipient), `messages` (ids in delivery order), `via` (`turn`, `receive` or `wait`). |
| `message_undelivered` | The envelope, flattened, and a `reason`. |

`message_sent` is written before the message can be delivered, so it precedes the matching `message_delivered`. A notice's `message_sent` follows the child's `node_end`, and every record about a child's messages to its parent precedes the parent's `node_end`. A send refused at once (full mailbox, finished or unknown recipient) writes no record; the caller sees the error. Tree reconstruction ignores message events, so a live view can draw them as edges between existing nodes.
