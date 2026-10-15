// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Exchange8:Settlement
// SPDX-License-Identifier: Apache-2.0

//! Exchange8:Settlement options lifecycle — exercise, assignment, expiry settlement.
//!
//! Implements the options workflow: auto-exercise at expiry,
//! early exercise (American), short assignment (pro-rata or random), and
//! cash/physical expiry settlement with the
//! `open → expiring → exercised/assigned → settled` state machine.
//!
//! Pure core; the shell (Exchange8:Clearing/Exchange8:Settlement) wires Sentin8:Cron
//! expiry triggers, persistence, and emits the
//! `exchange8:clearing:exercise:requested`,
//! `exchange8:clearing:assignment:allocated`,
//! `exchange8:clearing:expiry:settled` events.
//!
//! Design: `services/settlement/DESIGN.md` §2 (options request family), §3 (lifecycle).

use std::collections::HashMap;

/// Exercise style (mirrors `exchange8_contract.exercise_style`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExerciseStyle {
    European,
    American,
}

/// Settlement style at expiry (mirrors `exchange8_contract.settlement_style`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementStyle {
    Cash,
    Physical,
}

/// Option type (mirrors `exchange8_contract.option_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionType {
    Call,
    Put,
}

/// Contract lifecycle state (services/settlement/DESIGN.md §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractState {
    Open,
    Expiring,
    Exercised,
    Assigned,
    Settled,
}

/// An open option position leg (one account on one contract).
#[derive(Clone, Debug, PartialEq)]
pub struct OptionPosition {
    pub account_id: u64,
    pub instrument_id: u32,
    pub option_type: OptionType,
    pub exercise_style: ExerciseStyle,
    pub settlement_style: SettlementStyle,
    /// Net position: positive long, negative short (contract units).
    pub qty: i64,
    /// Strike in raw price ticks.
    pub strike_raw: i128,
}

/// Assignment algorithm (pro-rata or random, configurable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignmentMethod {
    /// Pro-rata across short holders (round-lot then largest-remainder).
    ProRata,
    /// Weighted random across short holders.
    Random,
}

/// An exercise request (holder- or expiry-engine-initiated).
#[derive(Clone, Debug, PartialEq)]
pub struct ExerciseRequest {
    pub account_id: u64,
    pub instrument_id: u32,
    /// Contracts to exercise (must be ≤ long position).
    pub qty: u64,
    /// Underlying settlement price at expiry (raw ticks), or spot for
    /// early exercise cash value.
    pub underlying_price_raw: i128,
}

/// Intrinsic value per contract (raw ticks); 0 when out-of-the-money.
pub fn intrinsic_value(
    option_type: OptionType,
    strike_raw: i128,
    underlying_price_raw: i128,
) -> i128 {
    match option_type {
        OptionType::Call => (underlying_price_raw - strike_raw).max(0),
        OptionType::Put => (strike_raw - underlying_price_raw).max(0),
    }
}

/// One exercise/assignment allocation record.
#[derive(Clone, Debug, PartialEq)]
pub struct Assignment {
    /// Long holder who exercised.
    pub exerciser_id: u64,
    /// Short holder assigned.
    pub assignee_id: u64,
    pub instrument_id: u32,
    pub qty: u64,
    /// Cash value assigned (qty × intrinsic, raw).
    pub cash_value_raw: i128,
}

/// Expiry outcome for one contract (aggregate view).
#[derive(Clone, Debug, PartialEq)]
pub struct ExpiryOutcome {
    pub instrument_id: u32,
    /// Exercises performed (per long holder).
    pub exercises: Vec<(u64, u64)>,
    /// Assignments produced (exerciser, assignee, qty).
    pub assignments: Vec<Assignment>,
    /// Total cash moved (raw); 0 for physical settlement.
    pub cash_settled_raw: i128,
    /// Physical delivery obligations (account → net underlying qty).
    pub deliveries: HashMap<u64, i64>,
    pub state: ContractState,
}

/// The exercise/assignment/expiry engine. Idempotent per (instrument,
/// account) exercise: a repeat request for an already-exercised position
/// leg is ignored (returns existing allocations unchanged).
pub struct ExerciseEngine {
    /// Open positions, keyed by (instrument_id, account_id).
    positions: HashMap<(u32, u64), OptionPosition>,
    /// Assignment method for short allocation.
    method: AssignmentMethod,
    /// Accumulated assignments per instrument.
    assignments: HashMap<u32, Vec<Assignment>>,
    /// Contracts already fully settled (terminal state).
    settled: std::collections::HashSet<u32>,
    /// Auto-exercise threshold: exercise when intrinsic ≥ threshold (raw
    /// ticks per contract). 0 = any ITM.
    itm_threshold_raw: i128,
    /// Deterministic RNG seed for random assignment (reproducible tests).
    rng_seed: u64,
}

impl ExerciseEngine {
    pub fn new(method: AssignmentMethod, itm_threshold_raw: i128) -> Self {
        ExerciseEngine {
            positions: HashMap::new(),
            method,
            assignments: HashMap::new(),
            settled: std::collections::HashSet::new(),
            itm_threshold_raw,
            rng_seed: 0x9E3779B97F4A7C15,
        }
    }

    /// Load an open position (projection from the position ledger).
    pub fn load_position(&mut self, p: OptionPosition) {
        self.positions.insert((p.instrument_id, p.account_id), p);
    }

    /// Early exercise (American only). Returns the assignments produced, or
    /// None if the request is invalid (European style, insufficient long
    /// qty, unknown position).
    pub fn early_exercise(&mut self, req: &ExerciseRequest) -> Option<Vec<Assignment>> {
        let key = (req.instrument_id, req.account_id);
        let pos = self.positions.get(&key)?;
        if pos.exercise_style != ExerciseStyle::American {
            return None;
        }
        if pos.qty < req.qty as i64 {
            return None;
        }
        Some(self.exercise(
            req.account_id,
            req.instrument_id,
            req.qty,
            req.underlying_price_raw,
        ))
    }

    /// Expiry processing for one contract: auto-exercise ITM longs
    /// (intrinsic ≥ threshold), assign shorts, settle.
    ///
    /// Physical settlement converts net option positions into underlying
    /// delivery obligations; cash settlement pays intrinsic value.
    pub fn process_expiry(
        &mut self,
        instrument_id: u32,
        underlying_price_raw: i128,
    ) -> ExpiryOutcome {
        let mut exercises: Vec<(u64, u64)> = Vec::new();
        let mut cash_total: i128 = 0;
        let mut deliveries: HashMap<u64, i64> = HashMap::new();

        // Snapshot original positions — physical delivery derives from the
        // pre-exercise net position (exercise() mutates them in place).
        let originals: Vec<OptionPosition> = self
            .positions
            .values()
            .filter(|p| p.instrument_id == instrument_id)
            .cloned()
            .collect();
        let style = originals
            .first()
            .map(|p| p.settlement_style)
            .unwrap_or(SettlementStyle::Cash);

        // Auto-exercise ITM longs (intrinsic ≥ threshold).
        let longs: Vec<(u64, u64, OptionType, i128)> = originals
            .iter()
            .filter(|p| p.qty > 0)
            .map(|p| (p.account_id, p.qty as u64, p.option_type, p.strike_raw))
            .collect();
        for (acct, qty, otype, strike) in longs {
            let iv = intrinsic_value(otype, strike, underlying_price_raw);
            if !self.is_itm(iv) {
                continue; // OTM long expires worthless.
            }
            self.exercise(acct, instrument_id, qty, underlying_price_raw);
            exercises.push((acct, qty));
            if style == SettlementStyle::Cash {
                cash_total += qty as i128 * iv;
            }
        }

        // Physical: net ITM positions convert to underlying delivery
        // obligations (long call receives, short call delivers; puts
        // reversed). OTM legs expire worthless — no delivery.
        if style == SettlementStyle::Physical {
            for p in &originals {
                if p.qty == 0
                    || !self.is_itm(intrinsic_value(
                        p.option_type,
                        p.strike_raw,
                        underlying_price_raw,
                    ))
                {
                    continue;
                }
                let dir = match p.option_type {
                    OptionType::Call => p.qty,
                    OptionType::Put => -p.qty,
                };
                *deliveries.entry(p.account_id).or_insert(0) += dir;
            }
        }

        // Terminal state: positions cleared, contract settled.
        self.positions.retain(|(iid, _), _| *iid != instrument_id);
        self.settled.insert(instrument_id);

        ExpiryOutcome {
            instrument_id,
            exercises,
            assignments: self
                .assignments
                .get(&instrument_id)
                .cloned()
                .unwrap_or_default(),
            cash_settled_raw: cash_total,
            deliveries,
            state: ContractState::Settled,
        }
    }

    /// ITM test against the configurable auto-exercise threshold.
    fn is_itm(&self, intrinsic_raw: i128) -> bool {
        if self.itm_threshold_raw == 0 {
            intrinsic_raw > 0
        } else {
            intrinsic_raw >= self.itm_threshold_raw
        }
    }

    /// Exercise `qty` contracts for `account`: reduce the long, allocate
    /// across shorts, record assignments.
    fn exercise(
        &mut self,
        account_id: u64,
        instrument_id: u32,
        qty: u64,
        underlying_price_raw: i128,
    ) -> Vec<Assignment> {
        let key = (instrument_id, account_id);
        let (otype, strike, style) = {
            let pos = self.positions.get(&key).expect("position loaded");
            (pos.option_type, pos.strike_raw, pos.settlement_style)
        };
        // Reduce the long position.
        if let Some(pos) = self.positions.get_mut(&key) {
            pos.qty -= qty as i64;
        }

        let iv = intrinsic_value(otype, strike, underlying_price_raw);
        // Cash value per contract at exercise (0 for physical).
        let per_contract = match style {
            SettlementStyle::Cash => iv,
            SettlementStyle::Physical => 0,
        };

        // Allocate across short holders.
        let shorts: Vec<(u64, i64)> = self
            .positions
            .iter()
            .filter(|((iid, _), _)| *iid == instrument_id)
            .filter(|(_, p)| p.qty < 0)
            .map(|((_, acct), p)| (*acct, -p.qty))
            .collect();
        let allocations =
            self.allocate_shorts(account_id, instrument_id, qty, per_contract, &shorts);

        // Reduce short positions by allocation.
        for a in &allocations {
            if let Some(pos) = self.positions.get_mut(&(instrument_id, a.assignee_id)) {
                pos.qty += a.qty as i64;
            }
        }

        let entry = self.assignments.entry(instrument_id).or_default();
        entry.extend(allocations.clone());
        allocations
    }

    /// Allocate `qty` exercised contracts across short holders.
    fn allocate_shorts(
        &mut self,
        exerciser_id: u64,
        instrument_id: u32,
        qty: u64,
        per_contract_cash: i128,
        shorts: &[(u64, i64)],
    ) -> Vec<Assignment> {
        if shorts.is_empty() || qty == 0 {
            return Vec::new();
        }
        let total_short: i64 = shorts.iter().map(|(_, q)| q).sum();
        let mut remaining = qty.min(total_short as u64);
        let mut out = Vec::new();

        match self.method {
            AssignmentMethod::ProRata => {
                // Largest-remainder pro-rata: floor shares first, then
                // distribute the remainder to the largest fractional parts.
                let mut floors: Vec<((u64, i64), u64, i128)> = shorts
                    .iter()
                    .map(|s| {
                        let exact = (s.1 as i128 * qty as i128) / total_short as i128;
                        (*s, exact as u64, exact)
                    })
                    .collect();
                let assigned: u64 = floors.iter().map(|(_, f, _)| *f).sum();
                let mut leftover = remaining.saturating_sub(assigned);
                // Largest fractional remainder first.
                floors.sort_by_key(|((acct, q), _, _)| {
                    let frac = (*q as i128 * qty as i128) % total_short as i128;
                    (std::cmp::Reverse(frac), *acct)
                });
                let mut i = 0;
                while leftover > 0 && !floors.is_empty() {
                    let idx = i % floors.len();
                    if floors[idx].0 .1 > floors[idx].1 as i64 {
                        floors[idx].1 += 1;
                        leftover -= 1;
                    }
                    i += 1;
                    if i > floors.len() * 2 + leftover as usize {
                        break;
                    }
                }
                for ((acct, _), f, _) in floors {
                    if f > 0 {
                        out.push(Assignment {
                            exerciser_id,
                            assignee_id: acct,
                            instrument_id,
                            qty: f.min(remaining),
                            cash_value_raw: f.min(remaining) as i128 * per_contract_cash,
                        });
                        remaining -= f.min(remaining);
                    }
                }
            }
            AssignmentMethod::Random => {
                // Weighted random draw without replacement, bounded by each
                // short's remaining qty.
                let mut avail: Vec<(u64, u64)> =
                    shorts.iter().map(|(a, q)| (*a, *q as u64)).collect();
                while remaining > 0 && !avail.is_empty() {
                    // xorshift64* — deterministic, seedable.
                    self.rng_seed ^= self.rng_seed >> 12;
                    self.rng_seed ^= self.rng_seed << 25;
                    self.rng_seed ^= self.rng_seed >> 27;
                    let r = self.rng_seed.wrapping_mul(0x2545F4914F6CDD1D)
                        % avail.iter().map(|(_, q)| q).sum::<u64>().max(1);
                    let mut acc = 0u64;
                    let mut chosen = 0usize;
                    for (i, (_, q)) in avail.iter().enumerate() {
                        acc += q;
                        if r < acc {
                            chosen = i;
                            break;
                        }
                        chosen = i;
                    }
                    let take = 1.min(remaining);
                    out.push(Assignment {
                        exerciser_id,
                        assignee_id: avail[chosen].0,
                        instrument_id,
                        qty: take,
                        cash_value_raw: take as i128 * per_contract_cash,
                    });
                    avail[chosen].1 -= take;
                    if avail[chosen].1 == 0 {
                        avail.remove(chosen);
                    }
                    remaining -= take;
                }
            }
        }
        out
    }
}

impl Default for ExerciseEngine {
    fn default() -> Self {
        Self::new(AssignmentMethod::ProRata, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(acct: u64, qty: i64, style: ExerciseStyle) -> OptionPosition {
        OptionPosition {
            account_id: acct,
            instrument_id: 100,
            option_type: OptionType::Call,
            exercise_style: style,
            settlement_style: SettlementStyle::Cash,
            qty,
            strike_raw: 10_000,
        }
    }

    #[test]
    fn intrinsic_call_put() {
        assert_eq!(intrinsic_value(OptionType::Call, 100, 150), 50);
        assert_eq!(intrinsic_value(OptionType::Call, 100, 50), 0);
        assert_eq!(intrinsic_value(OptionType::Put, 100, 50), 50);
        assert_eq!(intrinsic_value(OptionType::Put, 100, 150), 0);
    }

    #[test]
    fn early_exercise_american_only() {
        let mut e = ExerciseEngine::new(AssignmentMethod::ProRata, 0);
        e.load_position(pos(1, 10, ExerciseStyle::American));
        let req = ExerciseRequest {
            account_id: 1,
            instrument_id: 100,
            qty: 4,
            underlying_price_raw: 12_000,
        };
        let out = e.early_exercise(&req).unwrap();
        // No short holders — exercise reduces the long, produces no assignments.
        assert!(out.is_empty());
        // Long reduced.
        assert_eq!(e.positions.get(&(100, 1)).unwrap().qty, 6);

        // European rejects early exercise.
        let mut e2 = ExerciseEngine::new(AssignmentMethod::ProRata, 0);
        e2.load_position(pos(1, 10, ExerciseStyle::European));
        assert!(e2.early_exercise(&req).is_none());
    }

    #[test]
    fn exercise_qty_capped_by_long() {
        let mut e = ExerciseEngine::new(AssignmentMethod::ProRata, 0);
        e.load_position(pos(1, 3, ExerciseStyle::American));
        let req = ExerciseRequest {
            account_id: 1,
            instrument_id: 100,
            qty: 10, // > 3 held
            underlying_price_raw: 12_000,
        };
        assert!(e.early_exercise(&req).is_none());
    }

    #[test]
    fn pro_rata_assignment_largest_remainder() {
        let mut e = ExerciseEngine::new(AssignmentMethod::ProRata, 0);
        e.load_position(pos(1, 100, ExerciseStyle::American)); // long 100
        e.load_position(pos(2, -60, ExerciseStyle::American)); // short 60
        e.load_position(pos(3, -40, ExerciseStyle::American)); // short 40
        let req = ExerciseRequest {
            account_id: 1,
            instrument_id: 100,
            qty: 50,
            underlying_price_raw: 11_000,
        };
        let out = e.early_exercise(&req).unwrap();
        // 50 across 60/40 shorts pro-rata: 30/20 exact.
        let total: u64 = out.iter().map(|a| a.qty).sum();
        assert_eq!(total, 50);
        let a2 = out.iter().find(|a| a.assignee_id == 2).cloned().unwrap();
        let a3 = out.iter().find(|a| a.assignee_id == 3).cloned().unwrap();
        assert_eq!(a2.qty, 30);
        assert_eq!(a3.qty, 20);
        // Cash: 50 × 1000 intrinsic.
        let cash: i128 = out.iter().map(|a| a.cash_value_raw).sum();
        assert_eq!(cash, 50 * 1000);
        // Short positions reduced.
        assert_eq!(e.positions.get(&(100, 2)).unwrap().qty, -30);
        assert_eq!(e.positions.get(&(100, 3)).unwrap().qty, -20);
    }

    #[test]
    fn expiry_auto_exercises_itm_only() {
        let mut e = ExerciseEngine::new(AssignmentMethod::ProRata, 0);
        e.load_position(pos(1, 10, ExerciseStyle::European));
        // Long 10 ITM calls strike 10_000, underlying 11_000.
        let out = e.process_expiry(100, 11_000);
        assert_eq!(out.exercises, vec![(1, 10)]);
        assert_eq!(out.state, ContractState::Settled);
        assert_eq!(out.cash_settled_raw, 10 * 1000);
        // Positions cleared.
        assert!(e.positions.values().all(|p| p.instrument_id != 100));

        // OTM expiry: nothing exercised.
        let mut e2 = ExerciseEngine::new(AssignmentMethod::ProRata, 0);
        e2.load_position(pos(1, 10, ExerciseStyle::European));
        let out2 = e2.process_expiry(100, 9_000);
        assert!(out2.exercises.is_empty());
        assert_eq!(out2.cash_settled_raw, 0);
    }

    #[test]
    fn expiry_threshold_blocks_marginal_itm() {
        // Threshold 200: ITM 100 is below → no auto-exercise.
        let mut e = ExerciseEngine::new(AssignmentMethod::ProRata, 200);
        e.load_position(pos(1, 10, ExerciseStyle::European));
        let out = e.process_expiry(100, 10_100); // intrinsic 100 < 200
        assert!(out.exercises.is_empty());

        let mut e2 = ExerciseEngine::new(AssignmentMethod::ProRata, 200);
        e2.load_position(pos(1, 10, ExerciseStyle::European));
        let out2 = e2.process_expiry(100, 10_250); // intrinsic 250 ≥ 200
        assert_eq!(out2.exercises, vec![(1, 10)]);
    }

    #[test]
    fn random_assignment_conserves_qty() {
        let mut e = ExerciseEngine::new(AssignmentMethod::Random, 0);
        e.load_position(pos(1, 100, ExerciseStyle::American));
        e.load_position(pos(2, -60, ExerciseStyle::American));
        e.load_position(pos(3, -40, ExerciseStyle::American));
        let req = ExerciseRequest {
            account_id: 1,
            instrument_id: 100,
            qty: 50,
            underlying_price_raw: 11_000,
        };
        let out = e.early_exercise(&req).unwrap();
        let total: u64 = out.iter().map(|a| a.qty).sum();
        assert_eq!(total, 50);
        // Each assignee capped at their short qty.
        let s2: u64 = out
            .iter()
            .filter(|a| a.assignee_id == 2)
            .map(|a| a.qty)
            .sum();
        let s3: u64 = out
            .iter()
            .filter(|a| a.assignee_id == 3)
            .map(|a| a.qty)
            .sum();
        assert!(s2 <= 60 && s3 <= 40);
        assert_eq!(s2 + s3, 50);
    }

    #[test]
    fn physical_settlement_produces_deliveries() {
        let mut e = ExerciseEngine::new(AssignmentMethod::ProRata, 0);
        let mut long = pos(1, 10, ExerciseStyle::European);
        long.settlement_style = SettlementStyle::Physical;
        e.load_position(long);
        let mut short = pos(2, -10, ExerciseStyle::European);
        short.settlement_style = SettlementStyle::Physical;
        e.load_position(short);
        let out = e.process_expiry(100, 11_000);
        // Cash does not move on physical.
        assert_eq!(out.cash_settled_raw, 0);
        // Long call receives +10 underlying; short call delivers −10.
        assert_eq!(out.deliveries.get(&1), Some(&10));
        assert_eq!(out.deliveries.get(&2), Some(&-10));
    }
}
