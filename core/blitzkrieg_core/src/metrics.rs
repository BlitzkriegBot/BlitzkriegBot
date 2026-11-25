//! 指标口径的单一事实源（issue #181）。
//!
//! 缺陷形态：同一个「胜率」在上游有好几套实现，分母各按自己的理解取——
//! `trade_db::fold_into` 与 `trade_summary` 用 `wins + losses`（flat 不进分母），
//! `backtest::trade_stats` 用 `trades.len()`（flat 进分母），shadow 用
//! `sample_count`（flat 进分母）；再乘/不乘 100 又分成两套单位（wire 上
//! 同时是 0–100 的百分数与 0–1 的小数，前端 `format.ts` 注释记录了 7330%
//! 的二次放大事故）。
//!
//! 本模块只做两件事，且都只有一份实现：
//! 1. [`Outcome`] —— 一笔成交按净利符号归入胜/负/平的**判定**；
//! 2. [`Tally`] —— `(wins, losses) → 胜率` 的**公式**（`flats` 一等携带，
//!    但永不进分母）。
//!
//! 任何出口要报胜率都必须先拿到这两个东西之一，出口自己只决定「怎么序列化
//! 这个 Option」，不再决定「怎么算」。
//!
//! 口径（操作者裁决，本仓唯一）：
//! - 净利 `> 0` 记一胜，`< 0` 记一负，**`== 0` 记平手**；
//! - 平手既不胜也不负，**不进分母**；
//! - 没有任何分出胜负的笔 → `None`（「未知」，不是 0）。调用方要一个数字
//!   时自己决定 `unwrap_or`，但语义上「0 笔已决」与「胜率 0%」是两件事，
//!   本模块不替调用方抹平。

use rust_decimal::Decimal;

/// 一笔已平仓成交的胜负归类。判定**只在这里**：净利符号的比较走
/// `Decimal`（不是 f64），因为金额的 0 判定必须是精确语义
/// （`0.000000000000000001` 是胜，不是平手），否则一笔「看起来是 0」的
/// 成交会被记成平手并从分母里消失。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Win,
    Loss,
    /// 净利恰为 0：既不胜也不负，不进胜率分母。
    Flat,
}

impl Outcome {
    pub fn of_net(net: Decimal) -> Self {
        match net.cmp(&Decimal::ZERO) {
            std::cmp::Ordering::Greater => Outcome::Win,
            std::cmp::Ordering::Less => Outcome::Loss,
            std::cmp::Ordering::Equal => Outcome::Flat,
        }
    }

    /// f64 域的同一判定。只给**已经把净利读成 f64** 的出口用
    /// （`trade_summary` 的 JSON 行折叠：行是 wire JSON，`netPnlUsd` 到那儿
    /// 已经是 f64，再绕回 `Decimal` 只是给同一个 IEEE 值做一次有损往返）。
    ///
    /// 与 [`Self::of_net`] 的分工是数值域，不是口径：两者都是「>0 胜、<0 负、
    /// ==0 平」，且都放在本模块——口径仍然只有这一处可改。f64 的 `-0.0`
    /// 按 `total_cmp` 归为 `Equal`（`-0.0 == 0.0` 在 IEEE 里成立，金额上也是
    /// 平手）；非有限值走不到这里——`serde_json::Number::as_f64` 只返回有限值，
    /// 调用方的 `unwrap_or(0.0)` 把缺失/非数字行折成 0.0（既有行为，本 issue
    /// 不改它的口径）。
    pub fn of_net_f64(net: f64) -> Self {
        match net.total_cmp(&0.0) {
            std::cmp::Ordering::Greater => Outcome::Win,
            std::cmp::Ordering::Less => Outcome::Loss,
            std::cmp::Ordering::Equal => Outcome::Flat,
        }
    }
}

/// 一次胜/负/平计数。`flats` 是**必须携带**的一等字段：胜率的输入是三元组，
/// 少了 `flats` 的实现自然会各自决定平手算什么（这正是分叉的根因，见模块注释）。
///
/// `u32` 是快照形状（wire DTO）。累计计数器是 u64，直接用
/// [`Tally::win_rate_of`] / [`Tally::win_rate_pct_of`]，不要绕回这里截断。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tally {
    pub wins: u32,
    pub losses: u32,
    pub flats: u32,
}

impl Tally {
    /// 唯一胜率公式：`wins / (wins + losses)`，平手不进分母。
    ///
    /// `None` = 没有任何分出胜负的笔（不是 0.0）：把「0 笔已决」报成 0%
    /// 会让「还没交易」与「交易了但全输」在面板上长得一样。
    pub fn win_rate(&self) -> Option<f64> {
        Self::win_rate_of(u64::from(self.wins), u64::from(self.losses))
    }

    /// 公式本体，按 `u64` 计数。累计汇总（`TradeSummary.wins` /
    /// `TradeStats.wins`）是 u64 计数器，`Tally` 的 u32 只是快照形状——
    /// 让出口把 u64 截成 u32 再算才是在制造一个静默的 4.29e9 上限。
    /// 同一个公式只有这一份实现，[`Tally::win_rate`] 只是它的 u32 视图。
    ///
    /// 逐位可复现：只有同一条 `wins as f64 / decided as f64` 表达式，
    /// 相同整数计数在任何出口都得到同一个 f64 位型。
    pub fn win_rate_of(wins: u64, losses: u64) -> Option<f64> {
        let decided = wins + losses;
        if decided > 0 {
            Some(wins as f64 / decided as f64)
        } else {
            None
        }
    }

    /// 同一条公式的百分数投影（0–100），供字段是 `Decimal` 且历史上就叫
    /// `winRatePct` 的出口（回测报告）使用。**不是第二个公式**：先算小数，
    /// 再乘 100，除数与分母仍是 `win_rate_of` 那一条表达式。
    ///
    /// 两个精度陷阱都在这里堵掉（`tests/metrics_ssot.rs` 抓到过两次）：
    /// 1. `Decimal` 精确除法再 ×100 会四舍五入到 28 位定点，回到 f64 与
    ///    `40.0/70.0` 差 1 ULP——所以先算 f64 小数，只乘一次 100；
    /// 2. `Decimal::from_f64` 会裁到 ~15.95 位有效数字，把它当百分数读回
    ///    f64 再 `/100` 会丢 2–4 ULP——所以用 `from_f64_retain` 保留该 f64
    ///    的完整十进制展开（47 位截到 `Decimal` 的 28 位，仍是原 f64 的
    ///    正确舍入，读回 f64 恰好还原）。
    ///
    /// 注意**读侧**：`pct / Decimal::ONE_HUNDRED` 再转 f64 会高 1 ULP
    /// （28 位定点先舍入）。消费方的实际路径是「把百分数读成 f64，再
    /// `/100.0`」，那条路径逐位还原小数——`tests/metrics_ssot.rs` 钉的就是
    /// 它。不要在 `Decimal` 域把这个除法当无损运算。
    ///
    /// 比值不是金额，`f64` 在这里不违反 DEV §7 的 Decimal 金额纪律（金额字段
    /// 仍然全程 `Decimal`；这里只是同一个比值的定点渲染）。
    pub fn win_rate_pct_of(wins: u64, losses: u64) -> Option<Decimal> {
        Self::win_rate_of(wins, losses).map(|fraction| {
            // fraction ∈ [0,1] 且有限 ⇒ ×100 仍有限 ⇒ 必有 Some；
            // unwrap_or_default 只为满足类型（不可达分支不改变任何数字）。
            Decimal::from_f64_retain(fraction * 100.0).unwrap_or_default()
        })
    }

    /// 并入一笔。判定委托 [`Outcome::of_net`]，这里不再重复比较符号。
    pub fn fold_net(&mut self, net: Decimal) {
        match Outcome::of_net(net) {
            Outcome::Win => self.wins += 1,
            Outcome::Loss => self.losses += 1,
            Outcome::Flat => self.flats += 1,
        }
    }

    /// 折叠一串净利。多个出口（成交日志、成交折叠、回测）都从这里出发，
    /// 保证「同一组成交 → 同一个 Tally」。
    pub fn of_net_pnls<I>(nets: I) -> Self
    where
        I: IntoIterator<Item = Decimal>,
    {
        let mut t = Self::default();
        for n in nets {
            t.fold_net(n);
        }
        t
    }

    /// 已决笔数（胜 + 负）。`total` 是另一件事，故不提供同名方法。
    pub fn decided(&self) -> u32 {
        self.wins + self.losses
    }

    /// 全部计入笔数（胜 + 负 + 平）。
    pub fn total(&self) -> u32 {
        self.wins + self.losses + self.flats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// 平手不进分母：3 胜 / 2 负 / 5 平 → 3/5，不是 3/10。
    #[test]
    fn flat_trades_stay_out_of_the_denominator() {
        let t = Tally {
            wins: 3,
            losses: 2,
            flats: 5,
        };
        assert_eq!(t.win_rate(), Some(0.6));
        assert_eq!(t.total(), 10);
        assert_eq!(t.decided(), 5);
    }

    /// 只有平手 = 「未知」，不是 0%：0 笔已决与全输必须在类型上可分。
    #[test]
    fn only_flat_trades_report_unknown_not_zero() {
        let t = Tally {
            wins: 0,
            losses: 0,
            flats: 7,
        };
        assert_eq!(t.win_rate(), None);
        assert_eq!(Tally::default().win_rate(), None);
        assert_eq!(Tally::win_rate_of(0, 0), None);
        assert_eq!(Tally::win_rate_pct_of(0, 0), None);
    }

    /// 净利符号判定走 Decimal 精确比较：一个远小于 f64 精度、但确实为正
    /// 的净利仍是胜，不能被浮点折成 0 从而掉进平手桶。
    #[test]
    fn outcome_is_decimal_exact_not_float() {
        assert_eq!(Outcome::of_net(dec!(0.000000000000000001)), Outcome::Win);
        assert_eq!(Outcome::of_net(dec!(-0.000000000000000001)), Outcome::Loss);
        assert_eq!(Outcome::of_net(Decimal::ZERO), Outcome::Flat);
        let mut t = Tally::default();
        t.fold_net(dec!(0.000000000000000001));
        t.fold_net(dec!(-0.000000000000000001));
        t.fold_net(Decimal::ZERO);
        assert_eq!(
            t,
            Tally {
                wins: 1,
                losses: 1,
                flats: 1
            }
        );
        assert_eq!(t.win_rate(), Some(0.5));
    }

    /// 百分数投影与小数的比值一致：`×100` 只做一次、且从那个 f64 小数出发，
    /// 不引入第二个除法。`3/5` 这类精确可表示的例子两边都对得上。
    #[test]
    fn percent_projection_is_the_same_ratio_times_one_hundred() {
        assert_eq!(Tally::win_rate_pct_of(3, 2), Some(dec!(60)));
        assert_eq!(Tally::win_rate_of(3, 2), Some(0.6));
        assert_eq!(
            Tally::win_rate_pct_of(1, 1),
            Some(dec!(50)),
            "1 胜 1 负 → 50%，投影无取整"
        );
        assert_eq!(
            Tally::win_rate_pct_of(0, 3),
            Some(dec!(0)),
            "全负 → 0%，且这是「已决但全输」，与 (0,0) 的 None 不同"
        );
        // 100 笔含 40 flat：分母是 70 → 57.142857…%，与短除的小数同源。
        // 读侧走消费方的实际路径（百分数读成 f64 再 /100.0）——不是在
        // `Decimal` 域除以 100（那条路 28 位定点先舍入，高 1 ULP）。
        let pct = Tally::win_rate_pct_of(40, 30).expect("decided");
        let as_f64 = f64::try_from(pct).expect("fits");
        assert_eq!(
            (as_f64 / 100.0).to_bits(),
            (40.0_f64 / 70.0).to_bits(),
            "{pct}% 读回 f64 再 /100 必须还原同一个小数位型"
        );
        assert_eq!(as_f64.to_bits(), (40.0_f64 / 70.0 * 100.0).to_bits());
    }
}
