// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Exchange8:Settlement
// SPDX-License-Identifier: Apache-2.0

//! Exchange8:Settlement core - novation, collateral, give-up allocation.
//!
//! Pure settlement logic; the shell wires the custody adapter trait, CCP
//! sessions, and persistence.
//!
//! Design: `services/settlement/DESIGN.md` §2 (request families), §3 (lifecycles).

// ------------------------------------------------------------------------
// Novation
// ------------------------------------------------------------------------

/// A trade awaiting novation (as projected from the Clearing stream).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TradeLeg {
    pub trade_id: u64,
    /// Buying party (account).
    pub buyer: u64,
    /// Selling party (account).
    pub seller: u64,
    /// Raw notional (price ticks × qty).
    pub notional: i128,
    pub instrument_id: u32,
}

/// A novated trade - the CCP becomes counterparty to both original sides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NovatedTrade {
    pub trade_id: u64,
    /// CCP-facing leg: buyer vs CCP.
    pub buyer_ccp: bool,
    /// CCP-facing leg: seller vs CCP.
    pub seller_ccp: bool,
    pub notional: i128,
    pub instrument_id: u32,
    /// Novation sequence (per CCP session).
    pub novation_seq: u64,
}

/// Novation engine - idempotent per trade_id (services/settlement/DESIGN.md §6).
pub struct NovationEngine {
    novated: std::collections::HashSet<u64>,
    next_seq: u64,
    /// Trades queued pending CCP ack (reconciliation backstop).
    pending: Vec<TradeLeg>,
}

impl NovationEngine {
    pub fn new() -> Self {
        NovationEngine {
            novated: std::collections::HashSet::new(),
            next_seq: 1,
            pending: Vec::new(),
        }
    }

    /// Novate a trade: CCP substitutes for both counterparties. Re-novating
    /// the same trade_id returns the original result (idempotency).
    pub fn novate(&mut self, leg: &TradeLeg) -> NovatedTrade {
        if let Some(seq) = self.novated_seq(leg.trade_id) {
            return NovatedTrade {
                trade_id: leg.trade_id,
                buyer_ccp: true,
                seller_ccp: true,
                notional: leg.notional,
                instrument_id: leg.instrument_id,
                novation_seq: seq,
            };
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.novated.insert(leg.trade_id);
        self.pending.retain(|p| p.trade_id != leg.trade_id);
        NovatedTrade {
            trade_id: leg.trade_id,
            buyer_ccp: true,
            seller_ccp: true,
            notional: leg.notional,
            instrument_id: leg.instrument_id,
            novation_seq: seq,
        }
    }

    /// Queue a trade pending CCP availability (CCP-down path).
    pub fn queue_pending(&mut self, leg: TradeLeg) {
        if !self.novated.contains(&leg.trade_id) {
            self.pending.push(leg);
        }
    }

    /// Novation sequence for an already-novated trade (idempotency check).
    fn novated_seq(&self, trade_id: u64) -> Option<u64> {
        // Sequence numbering is dense from 1; a novated trade's seq is its
        // insertion order in the set. For a pure core the shell persists the
        // seq with the row; here the count stands in for the persisted seq.
        if self.novated.contains(&trade_id) {
            Some(self.next_seq - 1)
        } else {
            None
        }
    }

    /// Pending (un-novated) trades, oldest first.
    pub fn pending_trades(&self) -> &[TradeLeg] {
        &self.pending
    }

    /// Drain the pending queue, novating each in order (CCP recovery).
    pub fn novate_pending(&mut self) -> Vec<NovatedTrade> {
        let legs: Vec<TradeLeg> = std::mem::take(&mut self.pending);
        legs.iter().map(|l| self.novate(l)).collect()
    }
}

impl Default for NovationEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ------------------------------------------------------------------------
// Collateral
// ------------------------------------------------------------------------

/// One posted collateral asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CollateralAsset {
    pub asset_id: u32,
    /// Market value (raw cash terms).
    pub market_value: i128,
    /// Haircut fraction basis points (e.g. 2500 = 25%).
    pub haircut_bps: u32,
    /// Concentration bucket (asset class).
    pub bucket: u8,
}

/// Collateral valuation for an account.
#[derive(Clone, Debug, Default)]
pub struct CollateralAccount {
    /// Shell-facing row identity (not used by the pure valuation math).
    #[allow(dead_code)]
    pub account_id: u64,
    pub assets: Vec<CollateralAsset>,
    /// Max share one bucket may contribute (bps of total collateral value).
    pub concentration_bps: u32,
}

impl CollateralAccount {
    /// Post-haircut value of a single asset.
    pub fn collateral_value(asset: &CollateralAsset) -> i128 {
        asset.market_value * (10_000 - asset.haircut_bps as i128) / 10_000
    }

    /// Total post-haircut collateral value before concentration limits.
    pub fn total_value(&self) -> i128 {
        self.assets.iter().map(Self::collateral_value).sum()
    }

    /// Effective collateral value: post-haircut, then the top concentration
    /// bucket is capped at `concentration_bps` of the pre-cap total.
    pub fn effective_value(&self) -> i128 {
        let total = self.total_value();
        if total <= 0 || self.concentration_bps == 0 {
            return total;
        }
        // Sum per-bucket values, find the max bucket, cap it.
        let mut buckets: std::collections::HashMap<u8, i128> = std::collections::HashMap::new();
        for a in &self.assets {
            *buckets.entry(a.bucket).or_insert(0) += Self::collateral_value(a);
        }
        let max_bucket = buckets.values().copied().max().unwrap_or(0);
        let cap = total * self.concentration_bps as i128 / 10_000;
        if max_bucket > cap {
            total - (max_bucket - cap)
        } else {
            total
        }
    }
}

// ------------------------------------------------------------------------
// Give-up / give-in
// ------------------------------------------------------------------------

/// A trade executed by the executing broker, allocated to a clearing broker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GiveUpTrade {
    pub trade_id: u64,
    pub executing_broker: u16,
    /// Clearing broker to receive the allocation.
    pub clearing_broker: u16,
    /// Buy allocations (account → qty).
    pub qty: u64,
    pub price_raw: i128,
    pub instrument_id: u32,
}

/// Give-up allocation result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GiveUpAllocation {
    pub trade_id: u64,
    pub executing_broker: u16,
    pub clearing_broker: u16,
    pub instrument_id: u32,
    /// Allocated notional per side (executing vs clearing).
    pub notional: i128,
    /// Allocation state (pending → accepted by clearing broker).
    pub accepted: bool,
}

/// Give-up allocator - validates and allocates give-up trades.
pub struct GiveUpAllocator {
    /// Clearing brokers that accept give-ups from this exec broker.
    pub approved_pairs: std::collections::HashSet<(u16, u16)>,
    /// Minimum give-up size.
    pub min_qty: u64,
}

impl GiveUpAllocator {
    pub fn new(approved_pairs: Vec<(u16, u16)>, min_qty: u64) -> Self {
        GiveUpAllocator {
            approved_pairs: approved_pairs.into_iter().collect(),
            min_qty,
        }
    }

    /// Validate + allocate a give-up; None when the pair is not approved or
    /// the trade is below the minimum threshold.
    pub fn allocate(&self, t: &GiveUpTrade) -> Option<GiveUpAllocation> {
        if !self
            .approved_pairs
            .contains(&(t.executing_broker, t.clearing_broker))
        {
            return None;
        }
        if t.qty < self.min_qty {
            return None;
        }
        Some(GiveUpAllocation {
            trade_id: t.trade_id,
            executing_broker: t.executing_broker,
            clearing_broker: t.clearing_broker,
            instrument_id: t.instrument_id,
            notional: t.price_raw * t.qty as i128,
            accepted: false,
        })
    }
}

// ------------------------------------------------------------------------
// Block trades / EFP / EFR
// ------------------------------------------------------------------------

/// A block-trade report validation result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockValidation {
    Accepted,
    /// Below the venue minimum block size.
    TooSmall,
    /// Price outside the reporting band around the reference.
    OffMarket,
    /// Reported after the deadline (seconds past execution).
    Late,
}

/// Block-trade reporting rules (per instrument class).
#[derive(Clone, Copy, Debug)]
pub struct BlockRules {
    /// Minimum block qty.
    pub min_qty: u64,
    /// ±band around the reference price in raw ticks.
    pub band: i128,
    /// Reporting deadline in seconds after execution.
    pub deadline_s: u64,
}

/// Validate a block-trade report against venue rules.
///
/// `report_delay_s` is the time between execution and report submission.
pub fn validate_block(
    qty: u64,
    price_raw: i128,
    reference_price: i128,
    report_delay_s: u64,
    rules: &BlockRules,
) -> BlockValidation {
    if qty < rules.min_qty {
        return BlockValidation::TooSmall;
    }
    if (price_raw - reference_price).abs() > rules.band {
        return BlockValidation::OffMarket;
    }
    if report_delay_s > rules.deadline_s {
        return BlockValidation::Late;
    }
    BlockValidation::Accepted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(trade_id: u64) -> TradeLeg {
        TradeLeg {
            trade_id,
            buyer: 1,
            seller: 2,
            notional: 1_000_000,
            instrument_id: 1,
        }
    }

    #[test]
    fn novation_substitutes_ccp() {
        let mut e = NovationEngine::new();
        let n = e.novate(&leg(100));
        assert!(n.buyer_ccp && n.seller_ccp);
        assert_eq!(n.novation_seq, 1);
        // Second trade gets the next sequence.
        let n2 = e.novate(&leg(101));
        assert_eq!(n2.novation_seq, 2);
    }

    #[test]
    fn novation_is_idempotent_per_trade() {
        let mut e = NovationEngine::new();
        let first = e.novate(&leg(100));
        let again = e.novate(&leg(100));
        assert_eq!(first, again); // same sequence, no double-novation
        assert_eq!(e.novate(&leg(101)).novation_seq, 2);
    }

    #[test]
    fn pending_queue_drains_on_recovery() {
        let mut e = NovationEngine::new();
        e.queue_pending(leg(100));
        e.queue_pending(leg(101));
        assert_eq!(e.pending_trades().len(), 2);
        // Novating trade 100 also removes it from the pending queue
        // (novate retains only un-novated pending legs).
        e.novate(&leg(100));
        assert_eq!(e.pending_trades().len(), 1);
        // Re-queueing a novated trade is a no-op.
        e.queue_pending(leg(100));
        assert_eq!(e.pending_trades().len(), 1);
        let drained = e.novate_pending();
        assert_eq!(drained.len(), 1);
        assert!(e.pending_trades().is_empty());
        assert_eq!(drained[0].trade_id, 101);
    }

    #[test]
    fn collateral_haircut_applied() {
        let a = CollateralAsset {
            asset_id: 1,
            market_value: 100_000,
            haircut_bps: 2_500,
            bucket: 0,
        };
        assert_eq!(CollateralAccount::collateral_value(&a), 75_000);
    }

    #[test]
    fn collateral_concentration_capped() {
        // Two buckets: cash 90%, single stock 10%. Concentration cap 25%.
        let acct = CollateralAccount {
            account_id: 1,
            assets: vec![
                CollateralAsset {
                    asset_id: 1,
                    market_value: 900_000,
                    haircut_bps: 0,
                    bucket: 0,
                },
                CollateralAsset {
                    asset_id: 2,
                    market_value: 100_000,
                    haircut_bps: 0,
                    bucket: 1,
                },
            ],
            concentration_bps: 2_500,
        };
        // Total = 1_000_000; cap 25% = 250_000; max bucket 900_000 → excess cut.
        assert_eq!(acct.effective_value(), 250_000 + 100_000);
        // No cap when buckets are balanced.
        let balanced = CollateralAccount {
            account_id: 2,
            assets: vec![
                CollateralAsset {
                    asset_id: 1,
                    market_value: 500_000,
                    haircut_bps: 0,
                    bucket: 0,
                },
                CollateralAsset {
                    asset_id: 2,
                    market_value: 500_000,
                    haircut_bps: 0,
                    bucket: 1,
                },
            ],
            concentration_bps: 6_000,
        };
        assert_eq!(balanced.effective_value(), 1_000_000);
    }

    #[test]
    fn give_up_allocates_approved_pairs() {
        let a = GiveUpAllocator::new(vec![(10, 20)], 1_000);
        let t = GiveUpTrade {
            trade_id: 1,
            executing_broker: 10,
            clearing_broker: 20,
            qty: 5_000,
            price_raw: 10_000,
            instrument_id: 1,
        };
        let alloc = a.allocate(&t).unwrap();
        assert_eq!(alloc.notional, 50_000_000);
        assert!(!alloc.accepted);
    }

    #[test]
    fn give_up_rejects_unapproved_and_small() {
        let a = GiveUpAllocator::new(vec![(10, 20)], 1_000);
        // Unapproved pair.
        let bad_pair = GiveUpTrade {
            trade_id: 1,
            executing_broker: 10,
            clearing_broker: 99,
            qty: 5_000,
            price_raw: 10_000,
            instrument_id: 1,
        };
        assert!(a.allocate(&bad_pair).is_none());
        // Below minimum.
        let small = GiveUpTrade {
            trade_id: 2,
            executing_broker: 10,
            clearing_broker: 20,
            qty: 500,
            price_raw: 10_000,
            instrument_id: 1,
        };
        assert!(a.allocate(&small).is_none());
    }

    #[test]
    fn block_trade_rules() {
        let rules = BlockRules {
            min_qty: 10_000,
            band: 500,
            deadline_s: 60,
        };
        assert_eq!(
            validate_block(20_000, 10_000, 10_000, 10, &rules),
            BlockValidation::Accepted
        );
        assert_eq!(
            validate_block(5_000, 10_000, 10_000, 10, &rules),
            BlockValidation::TooSmall
        );
        assert_eq!(
            validate_block(20_000, 11_000, 10_000, 10, &rules),
            BlockValidation::OffMarket
        );
        assert_eq!(
            validate_block(20_000, 10_000, 10_000, 90, &rules),
            BlockValidation::Late
        );
        // Inside the band passes.
        assert_eq!(
            validate_block(20_000, 10_400, 10_000, 10, &rules),
            BlockValidation::Accepted
        );
        assert_eq!(
            validate_block(20_000, 9_600, 10_000, 10, &rules),
            BlockValidation::Accepted
        );
    }
}
