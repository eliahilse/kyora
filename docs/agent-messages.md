# Agent messages

Agents in one tree talk to each other asynchronously. A parent spawns a child and keeps working; the child's result arrives in the parent's mailbox as a message. The parent can send follow-ups to a running child, a child can report progress or ask its parent a question without ending, and siblings can message each other. Everything runs inside one process on top of the ownership, ledger and shutdown rules of the recursive runtime.

## How an agent ends

1. **Someone cancels it.** An ancestor's `cancel_agent`, its handle, its owning cell or its parent's shutdown cancels it together with its whole subtree, through the ordered shutdown below.
2. **Open-ended task.** It keeps running while it has work: ending a turn with children still running, or with messages pending, makes it wait for them (see idle agents). When nothing more can arrive it completes, and its parent receives the result as a message.
3. **Fixed structured output.** A child spawned with an output schema finishes when it submits a matching result: it ends at once, its running children are cancelled, and the JSON reaches the parent as the result.

A deadline, the turn cap or the budget can still end any agent, with the matching status.

## Mailboxes

Every agent node (the root and each child agent) owns one mailbox. Leaf `llm` calls have none. A child's mailbox exists from the moment it is admitted, so a parent can message a child right after spawning it, before the child's first request.

A mailbox holds at most `Limits::mailbox_capacity` undelivered plain messages (default 64). A send to a full mailbox fails at once with `MailboxFull`: nothing is queued, nothing already queued is dropped, and the sender never blocks. Terminal notices of children are not counted against the bound. Each child produces exactly one, so the agent limits already bound them, and a result is never refused because a chatty sibling filled the mailbox.

Bodies are text and at most `Limits::message_chars` characters (default 20,000). Messages are never cut once accepted: the results of `receive`, `wait` and `cancel_agent` hold whole messages and are exempt from `Limits::tool_output_chars`. The delivery budget below bounds them instead. Longer sends are refused. A child's answer in its terminal notice is shortened to the same cap, head and tail kept; the full answer stays in the child's `node_end` record and on its handle.

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

A send returns a message id once the message is queued in the recipient's mailbox, or an error, in which case nothing was queued. Recording and queueing an accepted send run as owned work that the sender's shutdown joins, so a caller that stops waiting cannot leave it half done: it is still recorded, then queued or recorded as undelivered. A sender's accepted sends are queued one after another in acceptance order, whatever order their records are acknowledged in. Inside the process an accepted message leaves its mailbox exactly once, in one of two ways:

- delivered: appended to the recipient's conversation at a turn boundary, returned by `receive`, or taken by `wait` or `cancel_agent`, which return a finished child's unread messages together with its notice, so the child's order is kept;
- undelivered: still queued when the recipient ended, and recorded in the trace.

Nothing is duplicated and nothing disappears silently. Taking messages from the mailbox and handing them over (into the conversation, or as the result of `receive`, `wait` or `cancel_agent`) is one step without a pause, so a caller that stops waiting cannot lose what was taken, and the mailbox never holds more than its capacity. The `message_delivered` record is then written by owned work that the recipient's shutdown joins, in the recipient's delivery order; at a turn boundary it is written before the conversation record that carries the text. Delivery into a conversation is at most once. The mailbox is a single FIFO queue, so messages from one sender arrive in send order, and a child's progress messages always arrive before its own result.

### Delivery budget

Each delivery hands over at most `Limits::delivery_chars` characters of rendered messages (default 60,000), counting each message's header and body. A delivery is one turn boundary, one `receive`, one `wait` or one `cancel_agent`. It takes whole messages in arrival order until the next would exceed the budget, and always at least one, so a single message larger than the budget still goes through whole. It never skips ahead to a smaller message, so the order is kept across deliveries.

What does not fit stays queued in its place, still counts against the mailbox capacity, and goes out at the next delivery. The model is told how many are left: a turn boundary appends `[3 more messages waiting; they follow at your next turn]`, and `receive` ends its result with the same line. An agent with messages still queued does not end at `end_turn`: it takes the next delivery and goes on.

`wait` and `cancel_agent` take only the finished child's messages, its notice last. When the budget cuts in between, the notice stays queued behind the messages that did not fit, and the result says so, for example `agent 3 finished, but 2 of its messages, its result last, did not fit; they follow at your next turn`. For `cancel_agent` this is the one case where the canceller later receives the child's notice as a message.

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

### Structured results

`ChildSpec::output` (the `output` argument of `spawn_agent`) takes a JSON schema of type object. The child then has a `submit_result` tool whose input schema is that schema; the parent cannot hand it out any other way, and children without a schema do not get it.

- A valid submission commits the object as the child's structured answer (`Answer::Value`), ends the child at once and cancels its running children. Once accepted it is final: a cancellation that arrives while it is being recorded does not replace it. The reverse also holds for every agent: once an agent is cancelled or past its deadline, a final answer committed afterwards, for example by a tool that finishes its cleanup after the cancellation, is refused and the agent ends cancelled or timed out. The parent receives the JSON text as the body of the `result` notice, and the handle's outcome holds the raw JSON.
- The runtime enforces the contract wherever a final answer is committed, not only in `submit_result`. Any other tool that tries to finish the child must commit a JSON value that matches the schema and fits `message_chars`; otherwise the answer is refused, the tool's result ends with `final answer not accepted:` and the reason, and the child goes on.
- An invalid submission returns an error naming the mismatch, for example `result does not match the output schema: missing property: dates`, and the child can try again. A result longer than `Limits::message_chars` is refused the same way, so the parent always receives the whole JSON.
- If the child ends a turn without submitting, it first waits for its own children and messages like any agent. When nothing more can arrive, it is reminded once with a user message. If it ends a turn again without submitting, it fails: status `failed`, a trace error `ended without calling submit_result`, and an answer that says so and quotes its last reply. Running out of turns without a submission ends it with `max_turns`.

Schemas are checked with the runtime's light validator, which enforces `type` (`object`, `array`, `string`, `integer`, `number`, `boolean`, `null`), `enum`, `required`, `properties`, `additionalProperties: false` and `items`, nested. A schema using a type it cannot enforce, such as a list of types, is refused at spawn. Other keywords are shown to the model but not enforced.

## Termination and cancellation

An agent's mailbox closes when its loop ends, before its children are cancelled. Later sends to it fail with `AgentFinished`. Messages still queued are recorded as `message_undelivered`. Node shutdown then runs in this order:

1. the agent loop ends and the mailbox closes; queued messages are recorded as undelivered;
2. admission closes, children are cancelled and joined; their notices find the mailbox closed and are recorded as undelivered;
3. `node_end` is written and the live-agent slot is released;
4. the node's handle resolves, then its own notice is posted to its parent.

Shutdown cancels every child and leaf task it owns directly and synchronously before joining them, so a task that has not started yet begins cancelled and dispatches nothing.

`cancel_agent` cancels a descendant of the caller (a child, or a child of a child) and its subtree, and returns once it has stopped, with its outcome. Parents, siblings and the caller itself cannot be cancelled this way. Each node-owned child still produces exactly one terminal notice:

- When the canceller is the child's parent, `cancel_agent` takes that notice itself (recorded as delivered `via: cancel`) and returns the outcome, so the parent gets no separate message, unless the child's queued messages exceed the delivery budget (see above).
- When the canceller is further up, the notice goes to the child's own parent as usual.
- Cancelling an agent that has already finished changes nothing and reports its outcome; if its notice was still queued, the call takes it.

Ownership decides who hears about a child's ending:

- A node-owned child (`Owner::Node`, the `spawn_agent` tool) is persistent. Its result, error or cancellation is posted to its parent's mailbox. Cancelling it through its handle produces a `cancelled` notice.
- A cell-owned child (`Owner::Cell`) belongs to the code that started it. That code reads the result from the handle, so no notice is posted. When the cell ends, the child is cancelled.

Both kinds can send, receive and be messaged.

## Budget and accounting

Messages consume no tokens, no model slots and no agent slots by themselves. Delivered text becomes part of the recipient's next request, so the reservation estimate counts its bytes and the provider charges it as input like any other user content. An idle agent holds no reservation. The ledger arithmetic, subtree budgets and per-node usage are unchanged.

## Deadlock freedom

Sends never block. A wait is only ever on the waiting agent's own children, so wait edges follow the node tree. `receive` is bounded by its `yield_after`. An idle agent waits only while its own children run or a send to it is in flight. Every wait is also bounded by the node deadline.

## Model-facing tools

`kyora_core::agent_tools::tools()` returns five tools. By default a child receives them when its parent holds them (see `defaults::SUBAGENT_TOOLS`). The runtime adds `submit_result` to children spawned with an output schema.

| Tool | Arguments | Result |
|---|---|---|
| `spawn_agent` | `task`, optional `name`, `tools`, `budget`, `timeout` (seconds), `output` (JSON schema) | Starts a node-owned child and returns `started agent 3 (name)` at once. |
| `send_message` | `to` (`parent`, an id or a name), `body` | `sent message 7 to agent 3`, or an error such as a full mailbox or a finished agent. |
| `receive` | optional `yield_after` (seconds, default 0) | One delivery of pending messages, rendered as above, and how many still wait; waits up to `yield_after` for the first one; `no messages` otherwise. |
| `wait` | optional `agents` (ids or names), optional `timeout` (seconds) | One delivery of the finished children's messages and results, children whose results did not fit, and the ones still running when the timeout passed. Waits for the named children, or for every child whose result has not been delivered yet. |
| `cancel_agent` | `to` (an id or a name) | The descendant's outcome once it has stopped, or `agent 3 had already finished` followed by its outcome. |
| `submit_result` | the output schema | `result submitted`, ending the child, or what to fix. Only for children with an output schema. |

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
NodeCtx::cancel_agent(&self, agent: NodeId)
    -> impl Future<Output = Result<CancelOutcome, RecursionError>>
NodeCtx::pending_messages(&self) -> usize
NodeCtx::render(&self, message: &Envelope) -> String
NodeCtx::render_outcome(&self, outcome: &AgentOutcome) -> String
Envelope::render(&self, sender: &str) -> String
```

`ChildSpec::output` sets the output schema. `CancelOutcome` carries the stopped agent's outcome and whether it had already finished. `Waited` holds one delivery of what the finished children had queued (their unread messages and notices, in arrival order), their outcomes, `deferred` (children whose messages did not all fit, with how many wait), and the ids of the children still running. `CancelOutcome::messages` and `CancelOutcome::remaining` hold the same for a cancelled direct child. `receive`, `wait` and `cancel_agent` apply the delivery budget for embedders too. `wait` takes the queued notices of the finished children, so their results are not delivered again at the next turn; `AgentHandle::result` is a plain observer and takes nothing. Errors: `MailboxFull` and `AgentFinished` for the recipient's state, `InvalidRequest` for unknown, ambiguous or unrelated addresses, messages to oneself and oversized bodies, and `Cancelled` when the calling agent has ended or is cancelled.

## Trace events

| Event | Fields |
|---|---|
| `message_sent` | The envelope, flattened. Written before the message can be delivered. |
| `message_delivered` | `node` (recipient), `messages` (ids in delivery order), `via` (`turn`, `receive`, `wait` or `cancel`). |
| `message_undelivered` | The envelope, flattened, and a `reason`. |

`message_sent` is written before the message can be delivered, so it precedes the matching `message_delivered`. A notice's `message_sent` follows the child's `node_end`, and every record about a child's messages to its parent precedes the parent's `node_end`. A send refused at once (full mailbox, finished or unknown recipient) writes no record; the caller sees the error. Tree reconstruction ignores message events, so a live view can draw them as edges between existing nodes.
