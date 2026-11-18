//! Bucketed realized-PnL analysis over recorded trades. Moved verbatim from
//! `shadow.rs`.

use super::*;
use rust_decimal::Decimal;

/// Bucket realized PnL by entry price (tests the entry cap) and by time-left at
/// entry (tests the timing gate). Only trades with a recorded actual PnL count.
pub fn bucket_by_entry(
    records: &[ShadowRecord],
    entry_edges: &[Decimal],
    time_edges: &[Decimal],
) -> (Vec<Bucket>, Vec<Bucket>) {
    let price_labels = labels_for(entry_edges);
    let time_labels = labels_for(time_edges);
    let mut by_price: Vec<Vec<Decimal>> = vec![Vec::new(); price_labels.len()];
    let mut by_time: Vec<Vec<Decimal>> = vec![Vec::new(); time_labels.len()];

    for r in records {
        let Some(pnl) = r.actual_net_pnl else {
            continue;
        };
        let pi = bucket_of(r.entry_price, entry_edges);
        if pi < by_price.len() {
            by_price[pi].push(pnl);
        }
        let tl = r.time_left_at_entry_sec.unwrap_or(Decimal::NEGATIVE_ONE);
        if tl >= Decimal::ZERO {
            let ti = bucket_of(tl, time_edges);
            if ti < by_time.len() {
                by_time[ti].push(pnl);
            }
        }
    }
    (
        price_labels
            .iter()
            .zip(by_price)
            .map(|(l, v)| summarize(l, v))
            .collect(),
        time_labels
            .iter()
            .zip(by_time)
            .map(|(l, v)| summarize(l, v))
            .collect(),
    )
}

#[derive(Debug, Clone)]
pub struct Bucket {
    pub label: String,
    pub n: usize,
    pub sum: Decimal,
    pub median: Decimal,
    pub win_rate: Decimal,
}

fn bucket_of(v: Decimal, edges: &[Decimal]) -> usize {
    for (i, e) in edges.iter().enumerate() {
        if v < *e {
            return i;
        }
    }
    edges.len()
}

fn summarize(label: &str, mut pnls: Vec<Decimal>) -> Bucket {
    let n = pnls.len();
    pnls.sort();
    let sum: Decimal = pnls.iter().copied().sum();
    let median = if n == 0 {
        Decimal::ZERO
    } else if n % 2 == 1 {
        pnls[n / 2]
    } else {
        (pnls[n / 2 - 1] + pnls[n / 2]) / Decimal::TWO
    };
    let wins = pnls.iter().filter(|p| **p > Decimal::ZERO).count();
    Bucket {
        label: label.to_string(),
        n,
        sum,
        median,
        win_rate: if n > 0 {
            Decimal::from(wins) / Decimal::from(n) * Decimal::ONE_HUNDRED
        } else {
            Decimal::ZERO
        },
    }
}

fn labels_for(edges: &[Decimal]) -> Vec<String> {
    let mut out = Vec::new();
    let mut prev: Option<Decimal> = None;
    for e in edges {
        out.push(match prev {
            Some(p) => format!("{p}-{e}"),
            None => format!("<{e}"),
        });
        prev = Some(*e);
    }
    out.push(format!(">={}", prev.unwrap_or(Decimal::ZERO)));
    out
}
