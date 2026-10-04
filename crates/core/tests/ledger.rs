use kyora_core::{
    Ledger, Limits,
    ledger::{Charge, estimate},
};
use kyora_protocol::{Message, ModelRequest, Usage};
use kyora_providers::AttemptCharge;
use proptest::prelude::*;
use std::sync::{Arc, Barrier};

fn charge(tokens: u64) -> Charge {
    Charge::Usage(Usage {
        input_tokens: tokens,
        ..Usage::default()
    })
}
#[test]
fn two_attempt_counterexample_closes_on_used_plus_reserved() {
    let ledger = Ledger::new(Limits {
        budget_tokens: 10_000,
        ..Limits::default()
    })
    .unwrap();
    let a = ledger.reserve(0, 0, 5000).unwrap();
    let b = ledger.reserve(0, 0, 5000).unwrap();
    let settled = ledger.settle(a, charge(9000));
    assert_eq!(settled.excess, 4000);
    let budget = ledger.snapshot(0);
    assert!(budget.closed);
    assert_eq!(budget.used, 9000);
    assert_eq!(budget.reserved, 5000);
    assert!(ledger.reserve(0, 0, 1).is_err());
    ledger.settle(b, charge(5000));
    assert_eq!(ledger.snapshot(0).used, 14_000);
}
#[test]
fn reservations_fit_every_scope_without_partial_changes() {
    let ledger = Ledger::new(Limits::default()).unwrap();
    let child = ledger.admit(0, true, Some(5000)).unwrap();
    assert!(ledger.reserve(child, 2000, 4096).is_err());
    assert_eq!(ledger.snapshot(0).reserved, 0);
    let r = ledger.reserve(child, 500, 8000).unwrap();
    assert_eq!(r.tokens, 5000);
    assert_eq!(r.max_tokens, 4500);
    ledger.settle(r, Charge::Failed(AttemptCharge::Zero));
    assert_eq!(ledger.snapshot(child).used, 0);
    assert_eq!(ledger.snapshot(0).reserved, 0);
}
#[test]
fn root_counts_and_live_slots_release_only_at_shutdown() {
    let ledger = Ledger::new(Limits {
        max_agents_live: 2,
        max_agents_total: 3,
        ..Limits::default()
    })
    .unwrap();
    let a = ledger.admit(0, true, None).unwrap();
    assert!(ledger.admit(0, true, None).is_err());
    ledger.shutdown(a);
    let b = ledger.admit(0, true, None).unwrap();
    ledger.shutdown(b);
    assert!(ledger.admit(0, true, None).is_err());
    assert!(ledger.admit(a, false, None).is_err());
}
#[test]
fn depth_llm_limits_and_zero_budgets_do_not_mutate_admission() {
    let ledger = Ledger::new(Limits {
        max_depth: 1,
        max_llm_calls: 1,
        ..Limits::default()
    })
    .unwrap();
    assert!(ledger.admit(0, true, Some(0)).is_err());
    let a = ledger.admit(0, true, None).unwrap();
    assert_eq!(a, 1);
    assert!(ledger.admit(a, true, None).is_err());
    let leaf = ledger.admit(a, false, None).unwrap();
    assert_eq!(leaf, 2);
    assert!(ledger.admit(0, false, None).is_err());
}
#[test]
fn estimator_uses_measured_prompt_output_and_only_new_messages() {
    let mut request = ModelRequest {
        system: Some("system".into()),
        messages: vec![Message::user_text("task")],
        max_tokens: 5000,
        ..ModelRequest::default()
    };
    assert_eq!(
        estimate(&request, None),
        6 + 2
            + serde_json::to_vec(&request.messages[0].content)
                .unwrap()
                .len() as u64
            + 16
            + 512
    );
    request
        .messages
        .push(Message::assistant_text("response not re-counted"));
    request.messages.push(Message::user_text("new"));
    assert_eq!(
        estimate(
            &request,
            Some((
                Usage {
                    input_tokens: 200,
                    output_tokens: 100,
                    cache_read_input_tokens: 300,
                    ..Usage::default()
                },
                1
            ))
        ),
        600 + serde_json::to_vec(&request.messages[2].content)
            .unwrap()
            .len() as u64
            + 16
    );
}
proptest! {
    #[test]
    fn settlement_exact_and_overshoot_bounded_at_first_closure(attempts in prop::collection::vec((1u32..8000,0u64..25_000,0u8..3),1..16)) {
        let limit=30_000;
        let ledger=Ledger::new(Limits { budget_tokens:limit,..Limits::default() }).unwrap();
        let mut active=Vec::new();
        for (output,actual,mode) in attempts {
            if let Ok(r)=ledger.reserve(0,100,output) {
                let before=ledger.snapshot(0); prop_assert!(!before.closed); prop_assert!(before.used+before.reserved<=limit);
                let actual=match mode {0=>0,1=>r.tokens,_=>actual}; active.push((r,actual));
            }
        }
        let mut exact=0; let mut closure_bound=None;
        while !active.is_empty() {
            let bound=active.iter().map(|(r,a)| a.saturating_sub(r.tokens)).sum::<u64>();
            let (r,actual)=active.remove(0); let reserved=r.tokens;
            let end=ledger.settle(r,charge(actual)); exact+=actual;
            prop_assert_eq!(end.charged,actual); prop_assert_eq!(end.excess,actual.saturating_sub(reserved));
            let snap=ledger.snapshot(0); prop_assert_eq!(snap.used,exact);
            if snap.closed && closure_bound.is_none() {closure_bound=Some(bound);}
            if snap.closed {prop_assert!(ledger.reserve(0,0,1).is_err());} else {prop_assert!(snap.used+snap.reserved<=limit);}
        }
        let snap=ledger.snapshot(0); prop_assert_eq!(snap.reserved,0);
        prop_assert!(snap.used.saturating_sub(limit)<=closure_bound.unwrap_or(0));
    }
    #[test]
    fn concurrent_admissions_reservations_failures_and_settlements(ops in prop::collection::vec((any::<bool>(),0u64..20_000,0u8..3),2..17)) {
        let ledger=Arc::new(Ledger::new(Limits {budget_tokens:20_000,max_agents_live:4,max_agents_total:5,max_inflight_requests:16,..Limits::default()}).unwrap());
        let barrier=Arc::new(Barrier::new(ops.len()));
        let mut threads=Vec::new();
        for (agent,actual,mode) in ops {
            let ledger=ledger.clone(); let barrier=barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                let Ok(node)=ledger.admit(0,agent,Some(12_000)) else {return (0,0)};
                let mut charged=0; let mut excess=0;
                if let Ok(r)=ledger.reserve(node,500,5000) {
                    let snap=ledger.snapshot(0); assert!(snap.closed || snap.used+snap.reserved<=snap.limit);
                    let policy=match mode {0=>Charge::Failed(AttemptCharge::Zero),1=>Charge::Failed(AttemptCharge::Reserved),_=>charge(actual)};
                    let end=ledger.settle(r,policy); charged=end.charged; excess=end.excess;
                }
                ledger.shutdown(node); (charged,excess)
            }));
        }
        let sums=threads.into_iter().map(|t|t.join().unwrap()).fold((0,0),|(a,b),(c,d)|(a+c,b+d));
        let snap=ledger.snapshot(0); prop_assert_eq!(snap.used,sums.0); prop_assert_eq!(snap.reserved,0);
        prop_assert!(snap.used.saturating_sub(snap.limit)<=sums.1);
        if snap.closed {prop_assert!(ledger.reserve(0,0,1).is_err());}
    }
}
