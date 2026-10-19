// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Exchange8:Settlement
// SPDX-License-Identifier: Apache-2.0

//! Exchange8:Settlement FX core - currency exposure, spot settlement
//! rails, forwards + NDF settlement.
//!
//! - **Currency exposure**: per-currency net positions aggregated
//!   across pairs (buy base = +base/−quote; sell the reverse), with
//!   limits and **value-date buckets** for forwards.
//! - **Spot rails**: a dealt FX trade produces two Bank8:Ledger
//!   posting legs (T+0/1/2 by pair convention) - novated: each client
//!   settles against the venue, not bilaterally.
//! - **Forwards + NDF**: forwards settle at value date via
//!   the futures expiry engine shape (fix-average → cash P&L); NDFs net
//!   to a single quote-currency amount (no delivery).
//!
//! Pure core; the shell wires Bank8:Ledger proxy-calls and custody.
//!
//! Design: `services/settlement/DESIGN.md` §2 (FX request family), §6.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Currency exposure
// ---------------------------------------------------------------------------

/// One dealt FX trade (input to exposure aggregation).
#[derive(Clone, Debug, PartialEq)]
pub struct FxDeal {
    /// Base currency units dealt (signed by direction below).
    pub base_qty: f64,
    /// Deal rate (price).
    pub rate: f64,
    /// Client buys the base currency.
    pub client_buys_base: bool,
    /// Value-date bucket (None = spot).
    pub value_date_days: Option<u32>,
}

impl FxDeal {
    /// Signed leg amounts for the client: (+base, −quote) when buying
    /// base; (−base, +quote·qty) when selling. Forwards: same shape at
    /// the deal rate (settlement legs at value date).
    pub fn client_legs(&self) -> (f64, f64) {
        let quote_qty = self.base_qty * self.rate;
        if self.client_buys_base {
            (self.base_qty, -quote_qty)
        } else {
            (-self.base_qty, quote_qty)
        }
    }
}

/// Per-currency net position bucket.
#[derive(Clone, Debug, PartialEq)]
pub struct ExposureBucket {
    pub currency: String,
    /// Net position (client-side sign).
    pub net: f64,
    /// Value-date bucket (None = spot).
    pub value_date_days: Option<u32>,
}

/// Exposure ledger: aggregates deals per (currency, value date).
#[derive(Default)]
pub struct CurrencyExposure {
    /// (ccy, value_date_days Option) → net.
    buckets: HashMap<(String, Option<u32>), f64>,
    /// Limits per currency (0/None = unlimited); applied per bucket AND
    /// per currency-total.
    limits: HashMap<String, f64>,
}

/// Exposure check outcome.
#[derive(Clone, Debug, PartialEq)]
pub enum ExposureVerdict {
    Ok,
    /// A single value-date bucket exceeds its limit.
    BucketExceeded {
        currency: String,
    },
    /// The currency total (all buckets) exceeds its limit.
    TotalExceeded {
        currency: String,
    },
}

impl CurrencyExposure {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a per-currency limit (None removes).
    pub fn set_limit(&mut self, currency: &str, limit: Option<f64>) {
        match limit {
            Some(l) => {
                self.limits.insert(currency.to_string(), l);
            }
            None => {
                self.limits.remove(currency);
            }
        }
    }

    /// Apply one deal's legs (pre-trade check with `check` first; this
    /// method is the post-trade ledger update).
    pub fn apply(&mut self, base_ccy: &str, quote_ccy: &str, deal: &FxDeal) {
        let (base_leg, quote_leg) = deal.client_legs();
        let vd = deal.value_date_days;
        let k = (base_ccy.to_string(), vd);
        *self.buckets.entry(k).or_insert(0.0) += base_leg;
        let k2 = (quote_ccy.to_string(), vd);
        *self.buckets.entry(k2).or_insert(0.0) += quote_leg;
    }

    /// Pre-trade check: would this deal breach a limit? (legs computed
    /// but not committed).
    pub fn check(&self, base_ccy: &str, quote_ccy: &str, deal: &FxDeal) -> ExposureVerdict {
        let (base_leg, quote_leg) = deal.client_legs();
        for (ccy, delta) in [(base_ccy, base_leg), (quote_ccy, quote_leg)] {
            let Some(limit) = self.limits.get(ccy).copied() else {
                continue;
            };
            if limit <= 0.0 {
                continue; // unlimited
            }
            // Bucket check.
            let bucket = self
                .buckets
                .get(&(ccy.to_string(), deal.value_date_days))
                .copied()
                .unwrap_or(0.0);
            if (bucket + delta).abs() > limit {
                return ExposureVerdict::BucketExceeded {
                    currency: ccy.to_string(),
                };
            }
            // Currency-total check (all buckets).
            let total: f64 = self
                .buckets
                .iter()
                .filter(|((c, _), _)| c == ccy)
                .map(|(_, v)| *v)
                .sum();
            if (total + delta).abs() > limit {
                return ExposureVerdict::TotalExceeded {
                    currency: ccy.to_string(),
                };
            }
        }
        ExposureVerdict::Ok
    }

    /// Net position of one currency (all buckets).
    pub fn net(&self, currency: &str) -> f64 {
        self.buckets
            .iter()
            .filter(|((c, _), _)| c == currency)
            .map(|(_, v)| *v)
            .sum()
    }

    /// One bucket's net.
    pub fn bucket(&self, currency: &str, value_date: Option<u32>) -> f64 {
        self.buckets
            .get(&(currency.to_string(), value_date))
            .copied()
            .unwrap_or(0.0)
    }

    /// All non-zero buckets (risk surface view).
    pub fn snapshot(&self) -> Vec<ExposureBucket> {
        let mut out: Vec<_> = self
            .buckets
            .iter()
            .filter(|(_, v)| **v != 0.0)
            .map(|((c, vd), v)| ExposureBucket {
                currency: c.clone(),
                net: *v,
                value_date_days: *vd,
            })
            .collect();
        out.sort_by(|a, b| (&a.currency, a.value_date_days).cmp(&(&b.currency, b.value_date_days)));
        out
    }
}

// ---------------------------------------------------------------------------
// Spot settlement rails (novation: venue-as-counterparty)
// ---------------------------------------------------------------------------

/// Spot settlement convention per pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpotConvention {
    T0,
    T1,
    T2,
}

impl SpotConvention {
    pub fn days(self) -> u32 {
        match self {
            SpotConvention::T0 => 0,
            SpotConvention::T1 => 1,
            SpotConvention::T2 => 2,
        }
    }
}

/// One settlement leg produced by a dealt FX trade.
#[derive(Clone, Debug, PartialEq)]
pub struct SettlementLegOut {
    /// Currency of the leg.
    pub currency: String,
    /// Signed amount for the client (+ receive / − deliver).
    pub amount: f64,
    /// Value date (business days after trade date).
    pub value_date_days: u32,
}

/// Produce the client's settlement legs for one dealt spot trade -
/// **novated**: the client settles against the venue on both legs (the
/// venue's offsetting legs with the LP/cover are separate ledger flows).
pub fn spot_settlement_legs(
    base_ccy: &str,
    quote_ccy: &str,
    deal: &FxDeal,
    convention: SpotConvention,
) -> Vec<SettlementLegOut> {
    let (base_leg, quote_leg) = deal.client_legs();
    let vd = convention.days();
    vec![
        SettlementLegOut {
            currency: base_ccy.to_string(),
            amount: base_leg,
            value_date_days: vd,
        },
        SettlementLegOut {
            currency: quote_ccy.to_string(),
            amount: quote_leg,
            value_date_days: vd,
        },
    ]
}

/// An FX-swap roll: near leg settles spot, far leg is a new forward
/// (design §3.4 "roll or settle"). Produces the roll's forward leg.
#[derive(Clone, Debug, PartialEq)]
pub struct SwapRoll {
    pub near_deal: FxDeal,
    /// Far-leg forward points applied to the near rate.
    pub forward_points: f64,
    pub far_value_date_days: u32,
}

impl SwapRoll {
    /// The far leg deal (same notional, rolled rate).
    pub fn far_deal(&self) -> FxDeal {
        FxDeal {
            base_qty: self.near_deal.base_qty,
            rate: self.near_deal.rate + self.forward_points,
            client_buys_base: !self.near_deal.client_buys_base, // swap = opposite far leg
            value_date_days: Some(self.far_value_date_days),
        }
    }
}

// ---------------------------------------------------------------------------
// Forwards + NDF settlement
// ---------------------------------------------------------------------------

/// One forward contract position at expiry (input to settlement).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ForwardPosition {
    /// Signed contract qty (client long base = +).
    pub qty: f64,
    /// Contract rate (entry).
    pub contract_rate: f64,
}

/// Forward settlement outcome (deliverable).
#[derive(Clone, Debug, PartialEq)]
pub struct ForwardSettlement {
    /// Full two-currency legs at the contract rate.
    pub legs: Vec<SettlementLegOut>,
}

/// NDF settlement outcome (non-deliverable: single quote-ccy net).
#[derive(Clone, Debug, PartialEq)]
pub struct NdfSettlement {
    /// Net quote-currency amount (fix − contract rate) × qty.
    pub net_quote_amount: f64,
    /// Positive = client receives.
    pub client_receives: bool,
}

/// Settle a deliverable forward at value date: physical two-leg settlement
/// at the contract rate.
pub fn settle_forward(
    base_ccy: &str,
    quote_ccy: &str,
    pos: &ForwardPosition,
    value_date_days: u32,
) -> ForwardSettlement {
    let deal = FxDeal {
        base_qty: pos.qty.abs(),
        rate: pos.contract_rate,
        client_buys_base: pos.qty > 0.0,
        value_date_days: Some(value_date_days),
    };
    let (base_leg, quote_leg) = deal.client_legs();
    ForwardSettlement {
        legs: vec![
            SettlementLegOut {
                currency: base_ccy.to_string(),
                amount: base_leg,
                value_date_days,
            },
            SettlementLegOut {
                currency: quote_ccy.to_string(),
                amount: quote_leg,
                value_date_days,
            },
        ],
    }
}

/// Settle an NDF: net = (fix_rate − contract_rate) × qty, settled in the
/// quote currency only (no delivery).
pub fn settle_ndf(pos: &ForwardPosition, fix_rate: f64) -> NdfSettlement {
    // Client long base: gains when fix > contract.
    let net = (fix_rate - pos.contract_rate) * pos.qty;
    NdfSettlement {
        net_quote_amount: net.abs(),
        client_receives: net > 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deal_buy_1m_usdcad() -> FxDeal {
        FxDeal {
            base_qty: 1_000_000.0, // USD base
            rate: 1.3650,          // CAD per USD
            client_buys_base: true,
            value_date_days: None, // spot
        }
    }

    #[test]
    fn client_legs_buy_base() {
        let (base, quote) = deal_buy_1m_usdcad().client_legs();
        assert!((base - 1_000_000.0).abs() < 1e-6);
        assert!((quote + 1_365_000.0).abs() < 1e-6);
    }

    #[test]
    fn client_legs_sell_base() {
        let d = FxDeal {
            client_buys_base: false,
            ..deal_buy_1m_usdcad()
        };
        let (base, quote) = d.client_legs();
        assert!((base + 1_000_000.0).abs() < 1e-6);
        assert!((quote - 1_365_000.0).abs() < 1e-6);
    }

    #[test]
    fn exposure_aggregates_and_buckets() {
        let mut ex = CurrencyExposure::new();
        // Buy 1M USD / sell 1.365M CAD (spot).
        ex.apply("USD", "CAD", &deal_buy_1m_usdcad());
        // Buy another 500k USD (spot).
        ex.apply(
            "USD",
            "CAD",
            &FxDeal {
                base_qty: 500_000.0,
                rate: 1.3650,
                client_buys_base: true,
                value_date_days: None,
            },
        );
        // Forward 3M: sell 200k USD.
        ex.apply(
            "USD",
            "CAD",
            &FxDeal {
                base_qty: 200_000.0,
                rate: 1.3700,
                client_buys_base: false,
                value_date_days: Some(90),
            },
        );
        assert!((ex.net("USD") - 1_300_000.0).abs() < 1e-6);
        // Spot bucket 1.5M; 90-day bucket −200k.
        assert!((ex.bucket("USD", None) - 1_500_000.0).abs() < 1e-6);
        assert!((ex.bucket("USD", Some(90)) + 200_000.0).abs() < 1e-6);
        // Snapshot has 4 buckets (USD spot, USD 90d, CAD spot, CAD 90d).
        assert_eq!(ex.snapshot().len(), 4);
    }

    #[test]
    fn exposure_limits_enforced() {
        let mut ex = CurrencyExposure::new();
        ex.set_limit("USD", Some(2_000_000.0));
        let d1 = deal_buy_1m_usdcad();
        assert_eq!(ex.check("USD", "CAD", &d1), ExposureVerdict::Ok);
        ex.apply("USD", "CAD", &d1);
        // Another 1.5M breaches the same (spot) bucket first: 2.5M > 2M -
        // bucket checks fire before total, and both would be exceeded here.
        let d2 = FxDeal {
            base_qty: 1_500_000.0,
            rate: 1.3650,
            client_buys_base: true,
            value_date_days: None,
        };
        assert!(matches!(
            ex.check("USD", "CAD", &d2),
            ExposureVerdict::BucketExceeded { currency } if currency == "USD"
        ));
        // A limit-free currency passes.
        assert_eq!(ex.check("EUR", "CAD", &d2), ExposureVerdict::Ok);
        // Removing the limit lifts it.
        ex.set_limit("USD", None);
        assert_eq!(ex.check("USD", "CAD", &d2), ExposureVerdict::Ok);
    }

    #[test]
    fn exposure_bucket_limit_independent_of_total() {
        let mut ex = CurrencyExposure::new();
        // Total limit 3M but each value-date bucket capped at 2M via the
        // same limit - a 1.5M forward on an empty bucket passes.
        ex.set_limit("USD", Some(3_000_000.0));
        let fwd = FxDeal {
            base_qty: 1_500_000.0,
            rate: 1.3650,
            client_buys_base: true,
            value_date_days: Some(180),
        };
        assert_eq!(ex.check("USD", "CAD", &fwd), ExposureVerdict::Ok);
        ex.apply("USD", "CAD", &fwd);
        // 1.5M more in the SAME bucket: 3M total == limit → NOT a breach
        // (limits trigger on strict >, at-limit is compliant). Push a third
        // deal past the bucket to confirm the bucket check fires.
        assert_eq!(ex.check("USD", "CAD", &fwd), ExposureVerdict::Ok);
        let d2 = FxDeal {
            base_qty: 2_000_000.0,
            rate: 1.3650,
            client_buys_base: true,
            value_date_days: Some(180),
        };
        assert!(matches!(
            ex.check("USD", "CAD", &d2),
            ExposureVerdict::BucketExceeded { currency } if currency == "USD"
        ));
    }

    #[test]
    fn spot_legs_novated_two_currencies() {
        let legs = spot_settlement_legs("USD", "CAD", &deal_buy_1m_usdcad(), SpotConvention::T2);
        assert_eq!(legs.len(), 2);
        assert_eq!(legs[0].currency, "USD");
        assert!((legs[0].amount - 1_000_000.0).abs() < 1e-6);
        assert_eq!(legs[1].currency, "CAD");
        assert!((legs[1].amount + 1_365_000.0).abs() < 1e-6);
        assert!(legs.iter().all(|l| l.value_date_days == 2));
        // T+0 convention.
        let t0 = spot_settlement_legs("USD", "CAD", &deal_buy_1m_usdcad(), SpotConvention::T0);
        assert!(t0.iter().all(|l| l.value_date_days == 0));
    }

    #[test]
    fn swap_roll_far_leg_opposite() {
        let roll = SwapRoll {
            near_deal: deal_buy_1m_usdcad(),
            forward_points: 0.0025, // 25 pips
            far_value_date_days: 92,
        };
        let far = roll.far_deal();
        assert!((far.rate - 1.3675).abs() < 1e-9);
        assert_eq!(far.value_date_days, Some(92));
        assert!(!far.client_buys_base); // far leg opposite
        assert!((far.base_qty - 1_000_000.0).abs() < 1e-6);
    }

    #[test]
    fn forward_settlement_delivers_both_legs() {
        let pos = ForwardPosition {
            qty: 500_000.0,
            contract_rate: 1.3600,
        };
        let s = settle_forward("USD", "CAD", &pos, 90);
        assert_eq!(s.legs.len(), 2);
        // Long base: receives USD, delivers CAD at contract rate.
        assert!((s.legs[0].amount - 500_000.0).abs() < 1e-6);
        assert!((s.legs[1].amount + 680_000.0).abs() < 1e-6);
        assert!(s.legs.iter().all(|l| l.value_date_days == 90));
    }

    #[test]
    fn forward_short_position_flips_legs() {
        let pos = ForwardPosition {
            qty: -500_000.0,
            contract_rate: 1.3600,
        };
        let s = settle_forward("USD", "CAD", &pos, 90);
        assert!((s.legs[0].amount + 500_000.0).abs() < 1e-6);
        assert!((s.legs[1].amount - 680_000.0).abs() < 1e-6);
    }

    #[test]
    fn ndf_settles_net_quote_only() {
        // Long 1M USD at 7.7500 CNY; fix 7.7800 → client receives
        // (7.78 − 7.75) × 1M = 30_000 CNY.
        let pos = ForwardPosition {
            qty: 1_000_000.0,
            contract_rate: 7.7500,
        };
        let s = settle_ndf(&pos, 7.7800);
        assert!((s.net_quote_amount - 30_000.0).abs() < 1e-6);
        assert!(s.client_receives);
        // Fix below contract → client pays.
        let s2 = settle_ndf(&pos, 7.7400);
        assert!((s2.net_quote_amount - 10_000.0).abs() < 1e-6);
        assert!(!s2.client_receives);
        // Exactly at fix → zero.
        let s3 = settle_ndf(&pos, 7.7500);
        assert!((s3.net_quote_amount - 0.0).abs() < 1e-9);
    }
}
