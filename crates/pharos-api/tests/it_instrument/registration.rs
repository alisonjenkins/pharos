//! T013: every RED series exists at zero before the first call, so `rate()`
//! works from the first scrape. Disarm check: an empty `register_metrics`
//! fails this.

use pharos_api::labels::Outcome;
use pharos_api::server::INTERFACES;

use crate::support::observe;

#[test]
fn every_method_outcome_series_is_registered_at_zero() {
    let ((), o) = observe("off", pharos_api::register_metrics);

    let calls = o.counters_named("pharos_rpc_calls_total");
    let cancelled = o.counters_named("pharos_rpc_cancelled_total");
    let mut expected_calls = 0;
    let mut expected_cancelled = 0;
    for i in INTERFACES {
        for m in i.methods {
            expected_cancelled += 1;
            let has = |series: &[(Vec<(String, String)>, u64)], outcome: Option<&str>| {
                series.iter().any(|(labels, v)| {
                    *v == 0
                        && labels.contains(&("interface".into(), m.interface.into()))
                        && labels.contains(&("method".into(), m.method.into()))
                        && outcome.is_none_or(|o| labels.contains(&("outcome".into(), o.into())))
                })
            };
            for outcome in Outcome::ALL {
                expected_calls += 1;
                assert!(
                    has(&calls, Some(outcome.label())),
                    "no zero series for {}.{} outcome={}",
                    m.interface,
                    m.method,
                    outcome.label()
                );
            }
            assert!(
                has(&cancelled, None),
                "no cancel series for {}.{}",
                m.interface,
                m.method
            );
        }
    }
    assert!(expected_calls > 0);
    assert_eq!(calls.len(), expected_calls, "unexpected extra series");
    assert_eq!(cancelled.len(), expected_cancelled);
}
