# BUSINESS.md: Ex8:Settlement — Business Requirements & DVP settlement, custody, FX, crypto, options

> Audience: product owners, business analysts, and anyone evaluating what
> Ex8:Settlement **does for the business**. For architecture read `README.md`;
> for wire-level detail read `DESIGN.md`.

## 1. Business Purpose

- Put every executed trade into a **legally binding settlement posture**:
  the central counterparty (CCP) steps in as buyer to every seller and seller
  to every buyer, through **novation**, in real time per trade.
- **Deliver versus payment (DVP)** discipline: no security moves without the
  matching cash (or crypto) movement, and no batch partially settles — a
  settlement either completes atomically or is rejected.
- Connect the platform to the **outside world of settlement**: CCP end-of-day
  file interfaces (CDS, SIS, Fortis), a crypto **custody** integration for
  on-chain assets, and fiat rails executed as Bank8:Ledger postings.
- Let brokers **share clearing workload**: an executing broker can hand a
  trade to the clearing broker that will actually carry it (give-up/give-in).
- Bring **negotiated, off-book trades** (blocks, EFP, EFR) into the
  settlement universe with venue-style validation rules.
- **Manage collateral**: value what participants pledged, apply haircuts and
  concentration caps, and flag utilization for the margin process.
- **Convert currencies**: FX deals produce precisely two settlement legs
  (spot, T+0/T+1/T+2), forwards settle at value date, NDFs net to a single
  quote-currency cash amount — under per-currency and per-value-date
  exposure limits.
- **Exercise options correctly**: in-the-money longs auto-exercise at expiry,
  short holders are assigned transparently (pro-rata or random), and the
  contract settles in cash or physically.
- Provide the **reconciliation backstop** to CCPs: deterministic netting CSV
  and fixed-width EOD exports that let the CCP's books and the venue's books
  be compared line by line.

## 2. Business Scope (What the Service Provides)

| # | Capability | Description |
|---|---|---|
| 1 | Novation | Real-time CCP substitution for both trade sides, idempotent per trade, with a pending queue when the CCP is unavailable |
| 2 | Collateral valuation | Post-haircut asset values, per-bucket concentration caps, rehypothecation flag |
| 3 | Give-up / give-in | Allocation of trades from executing to clearing brokers under an approved-pair and minimum-size policy |
| 4 | Block / EFP / EFR reporting | Validation of negotiated trade reports (size, price band, reporting deadline) and persistence of the verdict |
| 5 | CCP EOD file exports | Fixed-width CDS (3-record layout), SIS and Fortis file rendering with named part files |
| 6 | Settlement netting | Per-participant, per-security net quantity and net amount CSV as the CCP reconciliation backstop |
| 7 | Crypto custody | Asset master, custody accounts, on-chain transaction ledger, deposits/withdrawals with idempotency and step-up authorization |
| 8 | Atomic settlement batches | All-or-nothing multi-leg crypto settlement with intra-batch netting |
| 9 | Fiat margin rails | Ledger-entry request shapes for margin-collateral movements via Bank8:Ledger |
| 10 | FX conversion core | Currency-exposure ledger with value-date buckets and limits; spot legs; FX-swap rolls; forward and NDF settlement |
| 11 | Options exercise / assignment / expiry | Early exercise (American), auto-exercise at expiry, pro-rata or random short assignment, cash or physical settlement |
| 12 | Admin surfaces | Four RFW admin pages (novation, custody, collateral, blocks) registered with the platform admin shell |
| 13 | Audit & events | Every state change journaled and relayed to Sentin8:Bus (outbox producer) and the platform audit ingest |

**What the service does NOT own (business boundary)**

| Concern | Owner |
|---|---|
| The event-sourced trade/position ledger and internal novation statements | Ex8:Clearing |
| Order matching | Ex8:ME |
| Margin calculation and margin calls | Ex8:Margin |
| Regulatory transaction reporting | Ex8:Regulatory |
| Instrument/pair/contract masters and tenor catalog | Ex8:RefData |
| Fiat money movement itself (ledger postings) | Bank8:Ledger |
| Cron triggers for expiry and EOD job schedules | Sentin8:Cron |
| Identity, tokens, M2M client registration | Sentin8:Auth |
| Message delivery between services | Sentin8:Bus |

## 3. Business Capabilities

### Novation

| Capability | Business rule |
|---|---|
| CCP substitution | Both original counterparties are replaced by the CCP; the original trade is never altered |
| Idempotency | The same trade is never novated twice — a duplicate delivery returns the original result |
| Sequencing | Novations receive a dense sequence number per session, enabling gap detection on reconciliation |
| CCP outage tolerance | Trades recorded while the CCP is unavailable queue and novate automatically on recovery, oldest first |
| Per-facility routing | Each security clears at the CCP facility configured for it, with per-facility export credentials held as secret references |

### Custody & collateral

| Capability | Business rule |
|---|---|
| Custody accounts | One custody account per (env, asset, address); private keys are never stored — only secret references |
| Movement lifecycle | Every on-chain transaction moves through pending → signed → broadcast → confirmed (or failed); stuck transactions are surfaced for operations |
| Withdrawal approval | Withdrawals always require fresh step-up authentication, respect a per-asset minimum and a 24-hour notional cap |
| Movement idempotency | A retried deposit/withdrawal (same transaction reference) can never move funds twice |
| Collateral value | Assets are valued post-haircut; the largest asset bucket is capped at a configurable concentration share |
| Rehypothecation | Whether pledged collateral may be re-used is a per-row explicit flag, defaulting to no |
| Crypto margin | Moving collateral in/out of the margin sub-account is a custody movement (or a ledger entry for fiat) |

### CCP files & netting

| Capability | Business rule |
|---|---|
| CDS export | One 3-record group per trade inside a transmission/batch envelope; the file is the fixed-width format the CCP's reader expects, verified byte-for-byte |
| SIS / Fortis exports | Same common record core, per-variant reserved blocks and named files (`VER1.ISIN.P.<date>.0001.part`, `<date>.emcf`) |
| Netting | Buys and sells per participant per security net to one quantity and one amount; a fully flat position still appears (with zeros) |
| Reconciliation posture | The EOD files are the reconciliation backstop — real-time novation remains the primary settlement mechanism |

### FX (conversion core)

| Capability | Business rule |
|---|---|
| Exposure tracking | Every FX deal shifts signed base/quote currency positions, bucketed by value date |
| Exposure limits | Per-currency limits apply both per value-date bucket and to the currency total; a deal that breaches either is refused pre-trade (limits are strict — exactly at-limit is allowed) |
| Spot settlement | A dealt spot trade produces exactly two legs (receive base / deliver quote or the reverse) at the pair's T+0/T+1/T+2 convention, each against the venue — never bilateral |
| FX swap roll | Rolling produces a far-leg forward at near rate + forward points with the opposite side |
| Forward settlement | Deliverable forwards settle both currency legs fully at the contract rate on value date |
| NDF settlement | Non-deliverable forwards settle one quote-currency net: (fix − contract rate) × quantity; the client receives when the fix exceeds the contract rate |

### Crypto (settlement extension)

| Capability | Business rule |
|---|---|
| Precision | Crypto prices and quantities carry 8-decimal scale; mixed-scale multiplication is refused rather than approximated |
| Continuous market | The 24/7 market supports operational halt/resume only — no calendar phases |
| Atomic settlement | A multi-leg settlement batch executes entirely or not at all; intra-batch offsetting needs no external funds |
| Custody abstraction | Any custodian integration implements the same movement/balance contract with credentials by secret reference |

### Options

| Capability | Business rule |
|---|---|
| Exercise styles | European options exercise only at expiry; American options may exercise early, capped by their long position |
| ITM determination | Intrinsic value per contract (call: underlying − strike; put: strike − underlying; floor zero) |
| Auto-exercise threshold | At expiry only longs with intrinsic value at or above the configurable threshold exercise; OTM legs expire worthless |
| Assignment | Exercised contracts are allocated across short holders pro-rata (largest-remainder) or by weighted random draw — never exceeding any holder's short |
| Cash settlement | Cash-settled contracts pay intrinsic value × quantity on exercise |
| Physical settlement | Physically settled contracts convert net positions into underlying delivery obligations (long call receives, short call delivers; puts reversed); OTM legs deliver nothing |
| Contract expiry | After settlement the contract is terminal: positions are cleared and the outcome is audit-logged |

## 4. Business Events (Outbound)

| Event | When it fires |
|---|---|
| `exchange8:clearing:trade:novated` | The CCP has become counterparty to both sides of a trade (design ownership of novation outcome) |
| `exchange8:clearing:exercise:requested` | A holder exercises (early exercise or the expiry trigger initiates one) |
| `exchange8:clearing:assignment:allocated` | Short holders have been assigned exercised contracts |
| `exchange8:clearing:expiry:settled` | A contract finished expiry processing and its cash/deliveries are known |
| Outboxed state changes (custody confirmations, give-up allocations, block verdicts) | Every domain write is committed together with an outbox row relayed to Sentin8:Bus |

**Inbound (consumed)**

| Event / input | Source |
|---|---|
| `exchange8:clearing:trade:*` recorded trades | Ex8:Clearing ledger |
| Liquidation events (collateral demand) | Ex8:Margin |
| Instrument, FX pair and corporate-action definitions | Ex8:RefData |
| Expiry and EOD file schedule triggers | Sentin8:Cron |
| Fiat settlement legs and margin-collateral entries | Bank8:Ledger (executed postings) |

## 5. Business Rules & Invariants

1. Every trade novates exactly once — duplicate deliveries return the
   original novation and never consume a new sequence number.
2. A settlement state change and its durable event record are committed
   together: the platform can always reconstruct what happened and what
   must be told to the bus.
3. Custody funds never move on a repeated transaction reference; an
   insufficient-funds attempt does not "burn" the reference.
4. Withdrawals require fresh step-up authentication — a normal session is
   never sufficient — and respect the per-asset minimum and rolling 24-hour
   notional cap.
5. A multi-leg settlement batch is atomic: any single under-covered
   delivery leg rejects the whole batch.
6. Crypto arithmetic is exact: prices and quantities use pinned scales,
   and mismatched scales are refused, never silently converted.
7. FX exposure limits are checked before a deal commits, against both the
   value-date bucket and the currency total; a deal is refused on either
   breach.
8. NDFs are never delivered — exactly one quote-currency cash amount
   settles them.
9. Options auto-exercise only in-the-money longs beyond the configured
   threshold; assignment never exceeds a short holder's open quantity and
   always conserves the exercised quantity.
10. Block, EFP, EFR reports are validated against minimum size, reference
    price band and reporting deadline before acceptance; invalid reports are
    recorded with their rejection reason, never dropped silently.
11. No plaintext credentials anywhere: custodian keys, CCP file
    credentials and API secrets are referenced, not stored.
12. Every data table is tenant-isolated end to end (row-level security +
    explicit tenant predicates); no service role can bypass it.
13. EOD exports and the netting CSV are deterministic: the same day's data
    renders byte-identically with the same field widths.

## 6. Target-User Roles & Use Cases

| Persona | Role | What they do with the service |
|---|---|---|
| Clearing broker | Operational user | Reviews give-up allocations, accepts give-ins, monitors novation status per facility |
| Crypto desk operator | Custody user | Deposits/withdraws assets, watches transaction lifecycle, requests margin collateral moves |
| Compliance / risk manager | Supervisor | Reviews collateral utilization, exposure buckets and per-currency limit decisions |
| Exchange options desk | Trader-facing operations | Exercised longs view upcoming assignments, expiry outcomes and delivery obligations |
| CCP liaison / back office | Settlement operations | Consumes the EOD files and netting CSV; reconciles with the CCP's own books |
| Platform administrator | Admin | Manages the Shield policy sets, service health and admin pages for settlement |
| Treasury (FX) | Product owner | Defines pair conventions, monitors NDF fix results and forward rolls |

## 7. KPIs / Success Criteria

- **Novation completeness**: 100% of executed trades novated with zero
  double-novations (idempotency proven); novation keeps pace with trade
  volume at exchange peak rates.
- **Settlement integrity**: zero partially applied settlement batches; zero
  double-applied custody movements (deduplicated by transaction reference).
- **Custody correctness**: every on-chain transaction reaches a terminal
  state (`confirmed` or `failed`) and stuck items are surfaced within
  operations' alert window.
- **Reconciliation cleanliness**: CCP EOD files and the netting CSV accepted
  by the facility without field-offset errors; net positions match the
  ledger exactly, including flats.
- **FX correctness**: every dealt trade yields exactly two legs at the right
  value date; no limit breach goes unflagged pre-trade.
- **Options expiry accuracy**: ITM determination matches the threshold
  policy; assigned totals always equal exercised totals.
- **Security posture**: no secret material outside the secret-reference
  store at any point; all authorization decisions pass the enforced policy
  set.
- **Operational resilience**: relay and audit journals drain to zero pending
  after an outage; recovery replay never duplicates downstream effects.

## 8. Dependencies & Reliance on Other Services

| Dependency | What the business relies on it for |
|---|---|
| Ex8:Clearing | The authoritative trade record this service novates and reports on |
| Ex8:RefData | Instruments, FX pair/tenor definitions, corporate actions driving settlement |
| Ex8:Margin | Collateral shortfall responses and margin requirement authority |
| Ex8:Regulatory | The regulatory reporting outlet; settlement only supplies validated trades |
| Bank8:Ledger | Execution of every fiat leg and margin-collateral entry |
| Sentin8:Cron | Expiry triggers and EOD file schedules |
| Sentin8:Bus | Durable, audited event delivery (outbox producer pattern) |
| Sentin8:Auth | Identity, M2M client credentials (certificate-bound), step-up token verification |
| Sentin8:Sys | Audit intake (`POST /v1/audit`) and instance resolution |
| Bank8/other verticals | None directly — fiat movement is always mediated through Bank8:Ledger |

## 9. Acceptance Criteria

1. A trade novated twice (same id, same environment) produces one novation
   record, one sequence number, and identical results both times.
2. With the CCP unreachable, recorded trades enter the pending queue and
   novate automatically on recovery, oldest first, without duplicates.
3. A collateral account's effective value equals post-haircut value minus
   any bucket-cap excess, and margin sees the capped number only.
4. A give-up for an unapproved broker pair or below the minimum is refused
   with no allocation record.
5. A block trade report outside its reference band, below minimum size or
   past its deadline is stored with the matching rejection state and
   surfaces error `exchange8_block_trade_rejected`.
6. The CDS export renders the 3-record layout byte-exactly (pinned record
   lengths); SIS/Fortis files carry their named part filenames and correct
   transaction counts in the trailer.
7. The netting CSV nets buys against sells per participant per security and
   reports flats with zeros.
8. A custody deposit replayed with the same transaction reference returns a
   duplicate verdict and moves no funds; overdrawn withdrawals are refused;
   withdrawals without fresh step-up authentication are refused.
9. A settlement batch containing one uncovered leg settles nothing; the
   same batch whose legs net inside the batch settles.
10. An FX deal breaching a currency bucket or total limit is refused
    pre-trade with the currency named; NDF settlement yields a single
    quote-currency amount with the correct receiving side.
11. At expiry, only ITM longs beyond the threshold exercise; assignment
    totals match exercised quantities; cash pays intrinsic and physical
    produces exactly the pre-exercise net deliveries.
12. Every state change is auditable end-to-end: journal row → pushed audit
    event, with retries after a platform outage and zero loss.

## 10. Open Business Questions

- Which CCP facility transport (file drop vs API submission) each configured
  facility expects, and how acknowledgements feed back into novation status.
- Whether the settlement legs of CLOB-matched FX should additionally be
  streamed to Bank8:Ledger in real time or only batched at value date.
- The auto-exercise threshold defaults per product class (typical listed
  convention is one tick in the money) and who owns per-product overrides.
- Assignment method defaults: whether pro-rata or weighted-random should be
  the venue-wide default or configurable per contract class.
- Whether rehypothecation-flagged collateral should be reportable to
  participants as a segregation statement.
- How corporate-action settlement (cash/stock delivery) coordinates with
  this service's custody adapters versus the CCP facility files.

---

**Design attribution:** exchange8:settlement and its underlying concepts were designed by Hamid Jahanzad <Hamid@Jahanzad.com>.
