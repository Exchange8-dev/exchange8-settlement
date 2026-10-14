// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Exchange8:Settlement
// SPDX-License-Identifier: Apache-2.0

//! Ex8 crypto extension cores + margin rails.
//!
//! - **Per-instrument precision**: the `fixed` crate carries
//!   `Price { raw, scale }` / `Qty { raw, scale }` — this module asserts
//!   the ×10⁸ crypto scale semantics and mixed-scale arithmetic safety.
//! - **Continuous market mode**: the 24/7 market state machine
//!   (no calendar phases; halt/resume only).
//! - **CustodyAdapter**: the custody integration trait + a
//!   deterministic in-memory reference implementation (deposits,
//!   withdrawals, balances) with secret-ref credentials.
//! - **Atomic/batched settlement events**: the settlement batch
//!   builder (atomic group — all legs settle or none).
//! - **Withdrawal step-up auth + per-asset notional limits**.
//! - **Margin rails**: fiat margin via Bank8:Ledger ledger-entries,
//!   crypto margin via the custody adapter — the transfer shapes.
//!
//! Design: `services/settlement/DESIGN.md` §2, §3, §8.

use std::collections::HashMap;

use exchange8::fixed::{Price, Qty};

// ---------------------------------------------------------------------------
// C1.1 — precision semantics (crypto ×10⁸)
// ---------------------------------------------------------------------------

/// Crypto scale constants.
pub const CRYPTO_PRICE_SCALE: u8 = 8;
pub const CRYPTO_QTY_SCALE: u8 = 8;

/// Build a crypto price from raw ×10⁸.
pub fn crypto_price(raw: i128) -> Price {
    Price {
        raw,
        scale: CRYPTO_PRICE_SCALE,
    }
}

/// Build a crypto qty from raw ×10⁸.
pub fn crypto_qty(raw: u64) -> Qty {
    Qty {
        raw,
        scale: CRYPTO_QTY_SCALE,
    }
}

/// Mixed-scale notional guard: prices/quantities of different scales must
/// not be multiplied without explicit rescaling — returns None on mismatch
/// (the precision safety contract).
pub fn checked_notional_same_scale(p: Price, q: Qty) -> Option<i128> {
    if p.scale + q.scale != CRYPTO_PRICE_SCALE + CRYPTO_QTY_SCALE && p.scale != 4 && p.scale != 8 {
        return None;
    }
    // Require the instrument-consistent pair: (4,0) equity or (8,8) crypto.
    match (p.scale, q.scale) {
        (4, 0) | (8, 8) => p.checked_notional(q),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Continuous market mode
// ---------------------------------------------------------------------------

/// Continuous (24/7) market state — no calendar phases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContinuousState {
    Running,
    Halted,
}

/// The continuous market state machine.
pub struct ContinuousMarket {
    state: ContinuousState,
}

impl ContinuousMarket {
    pub fn new() -> Self {
        ContinuousMarket {
            state: ContinuousState::Running,
        }
    }

    pub fn state(&self) -> ContinuousState {
        self.state
    }

    /// Operational halt (incident / circuit breaker).
    pub fn halt(&mut self) {
        self.state = ContinuousState::Halted;
    }

    pub fn resume(&mut self) {
        self.state = ContinuousState::Running;
    }

    /// Is matching permitted?
    pub fn trading_allowed(&self) -> bool {
        self.state == ContinuousState::Running
    }
}

impl Default for ContinuousMarket {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Custody adapter
// ---------------------------------------------------------------------------

/// A custody movement request.
#[derive(Clone, Debug, PartialEq)]
pub enum CustodyRequest {
    Deposit {
        account: String,
        asset: String,
        qty: Qty,
        tx_ref: String,
    },
    Withdraw {
        account: String,
        asset: String,
        qty: Qty,
        tx_ref: String,
    },
}

/// Movement outcome.
#[derive(Clone, Debug, PartialEq)]
pub enum CustodyOutcome {
    /// Movement applied (new balance).
    Applied(Qty),
    /// Insufficient balance.
    Insufficient,
    /// Unknown account/asset.
    Unknown,
    /// Duplicate tx_ref (idempotency).
    Duplicate,
}

/// The custody integration contract. Real impls wrap a custodian
/// API with secret-ref credentials; the reference impl below is in-memory.
pub trait CustodyAdapter: Send + Sync {
    /// Credentials secret reference (never plaintext).
    fn creds_secret_ref(&self) -> &str;
    /// Apply one movement.
    fn apply(&mut self, req: &CustodyRequest) -> CustodyOutcome;
    /// Balance query.
    fn balance(&self, account: &str, asset: &str) -> Option<Qty>;
}

/// In-memory reference custody implementation.
pub struct MemoryCustody {
    creds_ref: String,
    balances: HashMap<(String, String), Qty>,
    seen_tx_refs: std::collections::HashSet<String>,
}

impl MemoryCustody {
    pub fn new(creds_ref: &str) -> Self {
        MemoryCustody {
            creds_ref: creds_ref.into(),
            balances: HashMap::new(),
            seen_tx_refs: std::collections::HashSet::new(),
        }
    }
}

impl CustodyAdapter for MemoryCustody {
    fn creds_secret_ref(&self) -> &str {
        &self.creds_ref
    }

    fn apply(&mut self, req: &CustodyRequest) -> CustodyOutcome {
        let (account, asset, qty, tx_ref, is_deposit) = match req {
            CustodyRequest::Deposit {
                account,
                asset,
                qty,
                tx_ref,
            } => (account, asset, *qty, tx_ref, true),
            CustodyRequest::Withdraw {
                account,
                asset,
                qty,
                tx_ref,
            } => (account, asset, *qty, tx_ref, false),
        };
        if !self.seen_tx_refs.insert(tx_ref.clone()) {
            return CustodyOutcome::Duplicate;
        }
        let key = (account.clone(), asset.clone());
        let bal = match self.balances.get(&key) {
            Some(b) => *b,
            None => {
                // Accounts must exist (provisioned upstream).
                if !self.balances.contains_key(&key) {
                    // First deposit creates the balance row.
                    if is_deposit {
                        self.balances.insert(key.clone(), Qty::ZERO);
                        Qty::ZERO
                    } else {
                        return CustodyOutcome::Unknown;
                    }
                } else {
                    Qty::ZERO
                }
            }
        };
        let new_bal = if is_deposit {
            Qty {
                raw: bal.raw + qty.raw,
                scale: qty.scale,
            }
        } else {
            if bal.raw < qty.raw {
                return CustodyOutcome::Insufficient;
            }
            Qty {
                raw: bal.raw - qty.raw,
                scale: qty.scale,
            }
        };
        self.balances.insert(key, new_bal);
        CustodyOutcome::Applied(new_bal)
    }

    fn balance(&self, account: &str, asset: &str) -> Option<Qty> {
        self.balances.get(&(account.into(), asset.into())).copied()
    }
}

// ---------------------------------------------------------------------------
// Atomic/batched settlement
// ---------------------------------------------------------------------------

/// One settlement leg.
#[derive(Clone, Debug, PartialEq)]
pub struct SettlementLeg {
    pub account: String,
    pub asset: String,
    /// Signed movement (+ receive / − deliver).
    pub qty: i128,
}

/// A settlement batch (atomic group).
#[derive(Clone, Debug, PartialEq)]
pub struct SettlementBatch {
    pub batch_id: u64,
    pub legs: Vec<SettlementLeg>,
}

/// Batch evaluation: atomic — valid only when every account/asset balance
/// can cover its deliver legs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchVerdict {
    Settle,
    /// Some leg cannot cover — the whole batch rejects (atomicity).
    RejectInsufficient,
}

/// Evaluate a batch against balances (atomicity gate).
pub fn evaluate_batch(batch: &SettlementBatch, balances: &dyn CustodyAdapter) -> BatchVerdict {
    // Aggregate per (account, asset) — net within the batch first.
    let mut net: HashMap<(&str, &str), i128> = HashMap::new();
    for leg in &batch.legs {
        *net.entry((leg.account.as_str(), leg.asset.as_str()))
            .or_insert(0) += leg.qty;
    }
    for ((account, asset), qty) in &net {
        if *qty < 0 {
            let need = (*qty).unsigned_abs();
            match balances.balance(account, asset) {
                Some(bal) if bal.raw as u128 >= need => {}
                _ => return BatchVerdict::RejectInsufficient,
            }
        }
    }
    BatchVerdict::Settle
}

// ---------------------------------------------------------------------------
// Withdrawal step-up auth + per-asset limits
// ---------------------------------------------------------------------------

/// Per-asset withdrawal limits.
#[derive(Clone, Debug, PartialEq)]
pub struct WithdrawLimits {
    /// asset → (24h notional cap in asset raw units, min withdrawal).
    pub per_asset: HashMap<String, (u64, u64)>,
}

/// Withdrawal authorization context.
#[derive(Clone, Debug, PartialEq)]
pub struct WithdrawRequest {
    pub account: String,
    pub asset: String,
    pub qty: Qty,
    /// Step-up auth token present? (Withdrawals require fresh
    /// step-up authentication — a normal session is insufficient.)
    pub step_up_token: bool,
}

/// Withdrawal decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WithdrawDecision {
    Authorized,
    /// Step-up auth missing.
    RequiresStepUp,
    /// Below minimum.
    BelowMinimum,
    /// Above 24h cap.
    AboveCap,
}

/// Authorize a withdrawal (pure policy; the shell verifies the step-up
/// token with Sentin8:Auth and supplies its presence here).
pub fn authorize_withdrawal(
    req: &WithdrawRequest,
    limits: &WithdrawLimits,
    already_withdrawn_24h: u64,
) -> WithdrawDecision {
    if !req.step_up_token {
        return WithdrawDecision::RequiresStepUp;
    }
    let Some((cap, min)) = limits.per_asset.get(&req.asset) else {
        // No limit row → no withdrawal configured for the asset.
        return WithdrawDecision::BelowMinimum;
    };
    if req.qty.raw < *min {
        return WithdrawDecision::BelowMinimum;
    }
    if already_withdrawn_24h.saturating_add(req.qty.raw) > *cap {
        return WithdrawDecision::AboveCap;
    }
    WithdrawDecision::Authorized
}

// ---------------------------------------------------------------------------
// Margin rails
// ---------------------------------------------------------------------------

/// Fiat margin rail: a Bank8:Ledger ledger-entry request (the shape the
/// shell posts to `/proxy/ledger/v1/entries`).
#[derive(Clone, Debug, PartialEq)]
pub struct FiatMarginEntry {
    pub account: String,
    /// Raw cents.
    pub amount: i128,
    /// Margin-collateral ledger code.
    pub code: String,
}

/// Crypto margin rail: move collateral to the margin custody account.
#[derive(Clone, Debug, PartialEq)]
pub struct CryptoMarginTransfer {
    pub account: String,
    pub asset: String,
    pub qty: Qty,
    /// true = transfer in (collateralize), false = release.
    pub into_margin: bool,
}

/// Build the custody request for a crypto margin transfer.
/// `into_margin` withdraws from the trading account; the shell credits the
/// margin sub-account (and reverse for release).
pub fn margin_transfer_request(t: &CryptoMarginTransfer) -> CustodyRequest {
    if t.into_margin {
        CustodyRequest::Withdraw {
            account: t.account.clone(),
            asset: t.asset.clone(),
            qty: t.qty,
            tx_ref: format!("margin-in:{}", t.account),
        }
    } else {
        CustodyRequest::Withdraw {
            account: format!("margin:{}", t.account),
            asset: t.asset.clone(),
            qty: t.qty,
            tx_ref: format!("margin-out:{}", t.account),
        }
    }
}

// Test-local alias to keep the idempotency assertion readable.
#[cfg(test)]
use CustodyOutcome as CustVerdict;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crypto_precision_scales() {
        let p = crypto_price(950_000_000); // 9.5 @ ×10⁸
        assert_eq!(p.scale, 8);
        let q = crypto_qty(350_000_000); // 3.5
        assert_eq!(q.scale, 8);
        // Consistent pair multiplies.
        assert!(checked_notional_same_scale(p, q).is_some());
        // Equity pair works.
        assert!(checked_notional_same_scale(Price::equity(10_000), Qty::units(100)).is_some());
        // Mixed-scale pair rejected.
        assert!(checked_notional_same_scale(p, Qty::units(100)).is_none());
    }

    #[test]
    fn continuous_market_halt_resume() {
        let mut m = ContinuousMarket::new();
        assert!(m.trading_allowed());
        m.halt();
        assert!(!m.trading_allowed());
        assert_eq!(m.state(), ContinuousState::Halted);
        m.resume();
        assert!(m.trading_allowed());
    }

    #[test]
    fn custody_deposit_withdraw_idempotent() {
        let mut c = MemoryCustody::new("secret://exchange8/custody/prod");
        assert_eq!(c.creds_secret_ref(), "secret://exchange8/custody/prod");
        // Deposit creates the balance.
        let d = CustodyRequest::Deposit {
            account: "A".into(),
            asset: "BTC".into(),
            qty: crypto_qty(1_000_000_000),
            tx_ref: "tx1".into(),
        };
        assert_eq!(
            c.apply(&d),
            CustodyOutcome::Applied(crypto_qty(1_000_000_000))
        );
        // Duplicate tx_ref → Duplicate (idempotency).
        assert_eq!(c.apply(&d), CustVerdict::Duplicate);
        // Withdraw within balance.
        let w = CustodyRequest::Withdraw {
            account: "A".into(),
            asset: "BTC".into(),
            qty: crypto_qty(400_000_000),
            tx_ref: "tx2".into(),
        };
        assert_eq!(
            c.apply(&w),
            CustodyOutcome::Applied(crypto_qty(600_000_000))
        );
        // Overdraw → Insufficient.
        let w2 = CustodyRequest::Withdraw {
            account: "A".into(),
            asset: "BTC".into(),
            qty: crypto_qty(700_000_000),
            tx_ref: "tx3".into(),
        };
        assert_eq!(c.apply(&w2), CustodyOutcome::Insufficient);
        // Withdraw from unknown account → Unknown.
        let w3 = CustodyRequest::Withdraw {
            account: "NOPE".into(),
            asset: "BTC".into(),
            qty: crypto_qty(1),
            tx_ref: "tx4".into(),
        };
        assert_eq!(c.apply(&w3), CustodyOutcome::Unknown);
        assert_eq!(c.balance("A", "BTC"), Some(crypto_qty(600_000_000)));
        assert_eq!(c.balance("A", "ETH"), None);
    }

    #[test]
    fn settlement_batch_atomic() {
        let mut c = MemoryCustody::new("secret://x");
        c.apply(&CustodyRequest::Deposit {
            account: "A".into(),
            asset: "BTC".into(),
            qty: crypto_qty(1_000_000_000),
            tx_ref: "d1".into(),
        });
        c.apply(&CustodyRequest::Deposit {
            account: "B".into(),
            asset: "BTC".into(),
            qty: crypto_qty(500_000_000),
            tx_ref: "d2".into(),
        });
        // A delivers 6, receives 2 from B (net −4 ≤ 10) — settle.
        let ok = SettlementBatch {
            batch_id: 1,
            legs: vec![
                SettlementLeg {
                    account: "A".into(),
                    asset: "BTC".into(),
                    qty: -600_000_000,
                },
                SettlementLeg {
                    account: "A".into(),
                    asset: "BTC".into(),
                    qty: 200_000_000,
                },
                SettlementLeg {
                    account: "B".into(),
                    asset: "BTC".into(),
                    qty: -200_000_000,
                },
            ],
        };
        assert_eq!(evaluate_batch(&ok, &c), BatchVerdict::Settle);
        // B delivers more than it has → whole batch rejects.
        let bad = SettlementBatch {
            batch_id: 2,
            legs: vec![
                SettlementLeg {
                    account: "B".into(),
                    asset: "BTC".into(),
                    qty: -900_000_000,
                },
                SettlementLeg {
                    account: "A".into(),
                    asset: "BTC".into(),
                    qty: 900_000_000,
                },
            ],
        };
        assert_eq!(evaluate_batch(&bad, &c), BatchVerdict::RejectInsufficient);
    }

    #[test]
    fn withdrawal_step_up_and_limits() {
        let mut limits = WithdrawLimits {
            per_asset: HashMap::new(),
        };
        limits
            .per_asset
            .insert("BTC".into(), (1_000_000_000, 100_000));
        let mk = |step_up: bool, qty: u64| WithdrawRequest {
            account: "A".into(),
            asset: "BTC".into(),
            qty: crypto_qty(qty),
            step_up_token: step_up,
        };
        // No step-up → denied even for a valid amount.
        assert_eq!(
            authorize_withdrawal(&mk(false, 500_000_000), &limits, 0),
            WithdrawDecision::RequiresStepUp
        );
        // Below min.
        assert_eq!(
            authorize_withdrawal(&mk(true, 50_000), &limits, 0),
            WithdrawDecision::BelowMinimum
        );
        // Over 24h cap (600M already out, cap 1B).
        assert_eq!(
            authorize_withdrawal(&mk(true, 500_000_000), &limits, 600_000_000),
            WithdrawDecision::AboveCap
        );
        // Fine.
        assert_eq!(
            authorize_withdrawal(&mk(true, 400_000_000), &limits, 600_000_000),
            WithdrawDecision::Authorized
        );
        // Unknown asset.
        let unk = WithdrawRequest {
            account: "A".into(),
            asset: "DOGE".into(),
            qty: crypto_qty(1),
            step_up_token: true,
        };
        assert_eq!(
            authorize_withdrawal(&unk, &limits, 0),
            WithdrawDecision::BelowMinimum
        );
    }

    #[test]
    fn fiat_margin_entry_shape() {
        let e = FiatMarginEntry {
            account: "A".into(),
            amount: 1_000_000,
            code: "margin-collateral".into(),
        };
        assert_eq!(e.amount, 1_000_000);
        assert_eq!(e.code, "margin-collateral");
    }

    #[test]
    fn crypto_margin_transfer_shapes() {
        let t = CryptoMarginTransfer {
            account: "A".into(),
            asset: "BTC".into(),
            qty: crypto_qty(100_000_000),
            into_margin: true,
        };
        let r = margin_transfer_request(&t);
        match r {
            CustodyRequest::Withdraw {
                account,
                asset,
                qty,
                tx_ref,
            } => {
                assert_eq!(account, "A");
                assert_eq!(asset, "BTC");
                assert_eq!(qty.raw, 100_000_000);
                assert!(tx_ref.starts_with("margin-in:"));
            }
            _ => panic!("expected withdraw-shaped request"),
        }
        let out = CryptoMarginTransfer {
            into_margin: false,
            ..t
        };
        match margin_transfer_request(&out) {
            CustodyRequest::Withdraw {
                account, tx_ref, ..
            } => {
                assert!(account.starts_with("margin:"));
                assert!(tx_ref.starts_with("margin-out:"));
            }
            _ => panic!("expected withdraw-shaped request"),
        }
    }
}
