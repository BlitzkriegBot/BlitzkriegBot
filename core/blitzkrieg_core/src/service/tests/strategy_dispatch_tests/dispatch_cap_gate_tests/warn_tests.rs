//! #1164: declared-but-NOT-accepted WARN rate-limited per
//! (account, strategy, reason) — same account + same strategy + same reason
//! prints once (the memo remembers the last reason), a changed reason prints
//! once more, and (#189) a DIFFERENT account's verdict is never suppressed by
//! another account's memo cell. Memos are bucketed by (account, strategy) so
//! tests are isolated. Moved verbatim from `dispatch_cap_gate_tests.rs` (pure
//! code motion) + the #189 account-dimension teeth.

use super::super::*;
use super::helpers::core_with_cross_venue_arb;

#[derive(Default, Clone)]
struct ArbWarnCapture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

struct WarnMessageVisitor(String);

impl tracing::field::Visit for WarnMessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0.push_str(&format!("{value:?} "));
        } else {
            self.0.push_str(&format!("{}={:?} ", field.name(), value));
        }
    }
}

impl<S> tracing_subscriber::layer::Layer<S> for ArbWarnCapture
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().level() != &tracing::Level::WARN {
            return;
        }
        let mut visitor = WarnMessageVisitor(String::new());
        event.record(&mut visitor);
        self.0.lock().expect("warn capture mutex").push(visitor.0);
    }
}

#[test]
fn declared_unaccepted_warn_is_rate_limited_to_the_verdict() {
    let name = "arb-1164-memo-probe"; // 独立备忘桶：进程级 static 与其它测试隔离
    let account = crate::model::AccountId::from("default");
    use tracing_subscriber::layer::SubscriberExt as _;
    let capture = ArbWarnCapture::default();
    let got = capture.0.clone();
    tracing::subscriber::with_default(tracing_subscriber::registry().with(capture), || {
        let warn = crate::service::helpers::arb_declared_unaccepted_warn_on_change;
        warn(&account, name, "atomic_batch not declared");
        warn(&account, name, "atomic_batch not declared"); // 同原因复读：备忘吃掉
        warn(&account, name, "no live PairBook entry under this strategy"); // 原因变化：再一条
        warn(&account, name, "no live PairBook entry under this strategy"); // 新原因复读：备忘吃掉
    });
    let lines = got.lock().expect("warn capture mutex").clone();
    assert_eq!(
        lines.len(),
        2,
        "first sight + each CHANGED verdict, repeats are silence: {lines:?}"
    );
    assert!(
        lines[0].contains("declared but NOT accepted") && lines[0].contains("atomic_batch"),
        "the first line is the missing-atomic_batch verdict: {lines:?}"
    );
    assert!(
        lines[1].contains("PairBook"),
        "the changed verdict prints once: {lines:?}"
    );
}

/// #189 验收：account A 的告警不得抑制 account B 的同类告警 ——
/// 同一策略名、两个账户、同一原因：两条都打（各自 first sight）；
/// 各自复读静默；B 的原因变化再打一条。这是「按名字去重」时代
/// 做不到的：旧 memo 让 A 的 cell 吃掉 B 的整行（或 B 的变因行）。
#[test]
fn account_b_warn_is_not_suppressed_by_account_a() {
    let name = "arb-189-account-dim-probe"; // 独立备忘桶
    let a = crate::model::AccountId::from("acct-189-a");
    let b = crate::model::AccountId::from("acct-189-b");
    use tracing_subscriber::layer::SubscriberExt as _;
    let capture = ArbWarnCapture::default();
    let got = capture.0.clone();
    tracing::subscriber::with_default(tracing_subscriber::registry().with(capture), || {
        let warn = crate::service::helpers::arb_declared_unaccepted_warn_on_change;
        // A first sight (one line), then B first sight — the SAME reason.
        // A name-keyed memo would eat B's line here; the account dimension
        // must not.
        warn(&a, name, "atomic_batch not declared");
        warn(&b, name, "atomic_batch not declared");
        // Each account's repeat is silence (same-(account, strategy) dedup
        // unchanged).
        warn(&a, name, "atomic_batch not declared");
        warn(&b, name, "atomic_batch not declared");
        // A verdict FLIP on B prints exactly one more line, A's bucket is
        // untouched by it.
        warn(&b, name, "no live PairBook entry under this strategy");
        warn(&b, name, "no live PairBook entry under this strategy");
    });
    let lines = got.lock().expect("warn capture mutex").clone();
    assert_eq!(
        lines.len(),
        3,
        "A first sight + B first sight + B's changed verdict (repeats silent): {lines:?}"
    );
    assert!(
        lines[0].contains("acct-189-a") && lines[1].contains("acct-189-b"),
        "the first two lines are the two accounts' first sights, in call order: {lines:?}"
    );
    assert!(
        lines[2].contains("acct-189-b") && lines[2].contains("PairBook"),
        "B's changed verdict carries B's account id: {lines:?}"
    );
    assert!(
        !lines[2].contains("acct-189-a"),
        "A's bucket must not leak into B's line: {lines:?}"
    );
}

/// #1164 验收①：未启用策略零输出 —— 关闸后的 cross_venue_arb 行（声明
/// 在场、atomic_batch 在场、配对簿无条目：恰是操作员报告里每周期刷 4+
/// 条 WARN 的那个状态）不进 arb 面评估：无 arb* 键、无 guard WARN（连续
/// 多次 readout 均静默）；行本身保留（平计数与历史不消失，enabled=false）。
#[test]
fn disabled_arb_row_is_not_evaluated_and_prints_no_warn() {
    let mut c = core_with_cross_venue_arb(None);
    // fixture premise：包声明 consumes_pair_book_stats + atomic_batch，
    // 配对簿空 ⇒ 启用状态下这就是一个会打 WARN 的判定态。
    assert!(c.set_strategy_enabled("cross_venue_arb", false));
    use tracing_subscriber::layer::SubscriberExt as _;
    let capture = ArbWarnCapture::default();
    let got = capture.0.clone();
    tracing::subscriber::with_default(tracing_subscriber::registry().with(capture), || {
        for round in 0..3 {
            let stats = c.strategy_stats();
            let g = strategy_entry(&stats, "cross_venue_arb");
            assert_eq!(g["enabled"], false, "round {round}");
            assert!(
                g.get("arbNetPnlUsd").is_none() && g.get("arbActivePairs").is_none(),
                "a disabled row keeps its plain counters and no arb* face (round {round}): {g}"
            );
        }
    });
    let lines = got.lock().expect("warn capture mutex").clone();
    assert!(
        lines.is_empty(),
        "a disabled strategy must stay silent, no guard warn may print: {lines:?}"
    );
}
