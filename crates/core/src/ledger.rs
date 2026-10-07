//! Atomic admission and accounting for the node scope tree.
use crate::{
    RecursionError,
    defaults::{Limits, MIN_OUTPUT_TOKENS},
};
use anyhow::{Result, bail};
use kyora_protocol::{ModelRequest, Usage};
use kyora_providers::AttemptCharge;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Mutex};

/// Node and budget scope identifier, allocated monotonically.
pub type NodeId = u32;
/// Scope counters, including outstanding reservations.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct BudgetSnapshot {
    /// Configured scope budget.
    pub limit: u64,
    /// Settled charges on the subtree.
    pub used: u64,
    /// Unsettled reservations on the subtree.
    pub reserved: u64,
    /// Once closed, this scope never admits further requests.
    pub closed: bool,
}
impl BudgetSnapshot {
    /// Remaining dispatch headroom, zero for closed scopes.
    pub fn remaining(&self) -> u64 {
        if self.closed {
            0
        } else {
            self.limit
                .saturating_sub(self.used.saturating_add(self.reserved))
        }
    }
}
struct Scope {
    parent: Option<NodeId>,
    depth: u32,
    agent: bool,
    alive: bool,
    admission_closed: bool,
    budget: BudgetSnapshot,
    usage: Usage,
    own: Usage,
}
struct State {
    scopes: Vec<Scope>,
    total: u32,
    live: u32,
    llm: u32,
    next_attempt: u64,
    attempts: BTreeMap<u64, (NodeId, u64)>,
}
/// One mutex protects scopes, counters and outstanding attempts.
pub struct Ledger {
    limits: Limits,
    state: Mutex<State>,
}
/// An admitted reservation; settlement consumes it exactly once.
#[derive(Debug)]
pub struct Reservation {
    /// Session-unique attempt identifier.
    pub id: u64,
    /// Reserved processed tokens.
    pub tokens: u64,
    /// Output cap after reduction to fit headroom.
    pub max_tokens: u32,
}
/// Accounting result suitable for an attempt_end record.
#[derive(Debug, Clone, Copy)]
pub struct Settlement {
    /// Exact recorded charge.
    pub charged: u64,
    /// Charge above the attempt's estimate.
    pub excess: u64,
}
/// Charge selected by the runtime after one attempt.
pub enum Charge {
    /// A completed request's measured usage.
    Usage(Usage),
    /// Provider error policy when final usage is unavailable.
    Failed(AttemptCharge),
}
impl Ledger {
    /// Creates the root scope and counts the root as one live agent.
    pub fn new(limits: Limits) -> Result<Self> {
        limits.validate()?;
        let root = Scope {
            parent: None,
            depth: 0,
            agent: true,
            alive: true,
            admission_closed: false,
            budget: BudgetSnapshot {
                limit: limits.budget_tokens,
                ..BudgetSnapshot::default()
            },
            usage: Usage::default(),
            own: Usage::default(),
        };
        Ok(Self {
            limits,
            state: Mutex::new(State {
                scopes: vec![root],
                total: 1,
                live: 1,
                llm: 0,
                next_attempt: 0,
                attempts: BTreeMap::new(),
            }),
        })
    }
    /// Atomically admits a child. Leaf LLM nodes do not consume agent depth or slots.
    pub fn admit(
        &self,
        parent: NodeId,
        agent: bool,
        limit: Option<u64>,
    ) -> std::result::Result<NodeId, RecursionError> {
        let mut s = self.state.lock().expect("ledger mutex poisoned");
        let p = s
            .scopes
            .get(parent as usize)
            .ok_or_else(|| RecursionError::InvalidRequest("unknown parent".into()))?;
        if !p.alive || p.admission_closed {
            return Err(RecursionError::Cancelled);
        }
        let depth = p.depth + u32::from(agent);
        for (exceeded, limit) in [
            (agent && depth > self.limits.max_depth, "depth"),
            (
                agent && s.live >= self.limits.max_agents_live,
                "agents_live",
            ),
            (
                agent && s.total >= self.limits.max_agents_total,
                "agents_total",
            ),
            (!agent && s.llm >= self.limits.max_llm_calls, "llm_calls"),
        ] {
            if exceeded {
                return Err(RecursionError::LimitExceeded { limit });
            }
        }
        if limit == Some(0) {
            return Err(RecursionError::InvalidRequest(
                "budget must be positive".into(),
            ));
        }
        let path = path(&s, parent);
        if path.iter().any(|id| s.scopes[*id].budget.remaining() == 0) {
            return Err(RecursionError::BudgetExceeded);
        }
        let budget = limit
            .unwrap_or(self.limits.budget_tokens)
            .min(p.budget.limit);
        let id = u32::try_from(s.scopes.len())
            .map_err(|_| RecursionError::InvalidRequest("node identifiers exhausted".into()))?;
        s.scopes.push(Scope {
            parent: Some(parent),
            depth,
            agent,
            alive: true,
            admission_closed: false,
            budget: BudgetSnapshot {
                limit: budget,
                ..BudgetSnapshot::default()
            },
            usage: Usage::default(),
            own: Usage::default(),
        });
        if agent {
            s.total += 1;
            s.live += 1;
        } else {
            s.llm += 1;
        }
        Ok(id)
    }
    /// Reserves on every ancestor atomically. Caller must already hold a model slot.
    pub fn reserve(
        &self,
        node: NodeId,
        prompt_estimate: u64,
        requested: u32,
    ) -> Result<Reservation> {
        if requested == 0 {
            bail!("max_tokens must be positive");
        }
        let mut s = self.state.lock().expect("ledger mutex poisoned");
        if !s
            .scopes
            .get(node as usize)
            .is_some_and(|n| n.alive && !n.admission_closed)
        {
            bail!("node is shut down");
        }
        let path = path(&s, node);
        let headroom = path
            .iter()
            .map(|id| s.scopes[*id].budget.remaining())
            .min()
            .unwrap_or(0);
        let available = headroom.saturating_sub(prompt_estimate);
        let cap = u64::from(requested).min(available) as u32;
        if cap < MIN_OUTPUT_TOKENS.min(requested) || headroom < prompt_estimate {
            bail!("budget exhausted");
        }
        let tokens = prompt_estimate + u64::from(cap);
        for id in path {
            s.scopes[id].budget.reserved += tokens;
        }
        let id = s.next_attempt;
        s.next_attempt += 1;
        s.attempts.insert(id, (node, tokens));
        Ok(Reservation {
            id,
            tokens,
            max_tokens: cap,
        })
    }
    /// Settles exactly once and closes any scope whose used plus reserved exceeds its limit.
    pub fn settle(&self, reservation: Reservation, charge: Charge) -> Settlement {
        let mut s = self.state.lock().expect("ledger mutex poisoned");
        let (node, tokens) = s
            .attempts
            .remove(&reservation.id)
            .expect("reservation already settled");
        let (charged, usage) = match charge {
            Charge::Usage(u) => (u.total(), u),
            Charge::Failed(AttemptCharge::Zero) => (0, Usage::default()),
            Charge::Failed(AttemptCharge::Reserved) => (
                tokens,
                Usage {
                    input_tokens: tokens,
                    ..Usage::default()
                },
            ),
        };
        for id in path(&s, node) {
            let n = &mut s.scopes[id];
            n.budget.reserved -= tokens;
            n.budget.used += charged;
            n.usage += usage;
            if n.budget.used.saturating_add(n.budget.reserved) > n.budget.limit {
                n.budget.closed = true;
            }
        }
        s.scopes[node as usize].own += usage;
        Settlement {
            charged,
            excess: charged.saturating_sub(tokens),
        }
    }
    /// Closes admission without releasing the live slot or discarding reservations.
    pub fn close_admission(&self, node: NodeId) {
        self.state.lock().expect("ledger mutex poisoned").scopes[node as usize].admission_closed =
            true;
    }
    /// Releases a live-agent slot only at node shutdown. Idempotent.
    pub fn shutdown(&self, node: NodeId) {
        let mut s = self.state.lock().expect("ledger mutex poisoned");
        let n = &mut s.scopes[node as usize];
        if n.alive {
            n.alive = false;
            if n.agent {
                s.live -= 1;
            }
        }
    }
    /// Returns one scope's budget counters.
    pub fn snapshot(&self, node: NodeId) -> BudgetSnapshot {
        self.state.lock().expect("ledger mutex poisoned").scopes[node as usize].budget
    }
    /// Returns measured or conservatively recorded usage, self then subtree.
    pub fn usage(&self, node: NodeId) -> (Usage, Usage) {
        let s = self.state.lock().expect("ledger mutex poisoned");
        let n = &s.scopes[node as usize];
        (n.own, n.usage)
    }
}
fn path(s: &State, node: NodeId) -> Vec<usize> {
    let mut result = Vec::new();
    let mut next = Some(node);
    while let Some(id) = next {
        result.push(id as usize);
        next = s.scopes[id as usize].parent;
    }
    result
}
/// Estimates prompt tokens before adding max_tokens, following D10.2.
/// `previous` is the last completed request and the number of history messages it sent.
pub fn estimate(request: &ModelRequest, previous: Option<(Usage, usize)>) -> u64 {
    if let Some((usage, sent)) = previous {
        // The response's assistant message is represented by measured output tokens.
        let new = request.messages.get(sent + 1..).unwrap_or_default();
        usage.total()
            + new
                .iter()
                .map(|m| {
                    serde_json::to_vec(&m.content)
                        .expect("serializable message")
                        .len() as u64
                })
                .sum::<u64>()
            + 16 * new.len() as u64
    } else {
        request.system.as_ref().map_or(0, |s| s.len() as u64)
            + serde_json::to_vec(&request.tools)
                .expect("serializable tools")
                .len() as u64
            + request
                .messages
                .iter()
                .map(|m| {
                    serde_json::to_vec(&m.content)
                        .expect("serializable message")
                        .len() as u64
                })
                .sum::<u64>()
            + 16 * request.messages.len() as u64
            + 512
    }
}
