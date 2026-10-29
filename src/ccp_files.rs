// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Exchange8:Settlement
// SPDX-License-Identifier: Apache-2.0

//! Exchange8:Settlement CCP EOD file builders - CDS 3-record layout (
//! `Daemon/Feed/CDSFile.cpp` parity) plus the SIS/Fortis record shapes.
//!
//! The CDS export is a fixed-width text file: a transmission header, a batch
//! header, then one **3-record group per trade** (record 1 = trade
//! identification, record 2 = security/quantity/price, record 3 = account
//! tags), followed by batch and transmission trailers. Field widths are
//! pinned to the struct definitions (the wire spec).
//!
//! Design: `services/settlement/DESIGN.md` §2 (CCP file request family), §9.

/// Fixed-width fillers (the platform spec parity).
fn fill_alpha(buf: &mut String, value: &str, width: usize) {
    let v = if value.len() > width {
        &value[..width]
    } else {
        value
    };
    buf.push_str(v);
    buf.push_str(&" ".repeat(width - v.len()));
}

fn fill_numeric(buf: &mut String, value: u64, width: usize) {
    let s = value.to_string();
    if s.len() >= width {
        buf.push_str(&s[s.len() - width..]);
    } else {
        buf.push_str(&"0".repeat(width - s.len()));
        buf.push_str(&s);
    }
}

/// Decimal field with implicit scale: value × 10^(D−E) zero-padded to
/// `sz1  D` .
fn fill_decimal(buf: &mut String, value: u64, int_width: usize, exp: i32, decimals: usize) {
    let scaled = if decimals >= exp as usize {
        value * 10u64.pow((decimals - exp as usize) as u32)
    } else {
        value / 10u64.pow((exp as usize - decimals) as u32)
    };
    fill_numeric(buf, scaled, int_width + decimals);
}

/// HHMMSS from a microsecond timestamp .
fn fill_time(buf: &mut String, ts_us: u64) {
    let sec = ts_us / 1_000_000;
    let hours = sec / 3600;
    let minutes = (sec % 3600) / 60;
    let seconds = sec % 60;
    fill_numeric(buf, hours, 2);
    fill_numeric(buf, minutes, 2);
    fill_numeric(buf, seconds, 2);
}

/// One trade to export.
#[derive(Clone, Debug)]
pub struct CdsTrade {
    /// Trade reference (e.g. `YYYYMMDD-<trade_id>`).
    pub trade_reference: String,
    /// ISO trade date `YYYYMMDD`.
    pub trade_date: String,
    /// Value (settlement) date `YYYYMMDD`.
    pub value_date: String,
    /// Execution timestamp (µs since midnight UTC; converted to local by shell).
    pub ts_us: u64,
    /// Side of the reporting participant: true = buy.
    pub reporting_side_is_buy: bool,
    /// Buyer CUID (5) and seller CUID (5).
    pub buyer_cuid: String,
    pub seller_cuid: String,
    /// Security identifiers.
    pub isin: String,
    pub symbol: String,
    pub currency: String,
    /// Quantity (raw integer units).
    pub volume: u64,
    /// Price (raw ticks; scale per instrument).
    pub price: u64,
    /// Buyer/seller account + sub-participant tags.
    pub buyer_account: String,
    pub seller_account: String,
    pub buyer_sub: String,
    pub seller_sub: String,
    /// Tag numbers share the trade id.
    pub trade_id: String,
    /// Market id (5).
    pub market_id: String,
}

/// CDS file builder: emits the full transmission (headers + trades +
/// trailers) as one `String` (the shell writes it to the EOD path).
pub struct CdsFile {
    user_id: String,
    password: String,
    transmission_id: String,
    market_id: String,
    /// `YYYYMMDD` creation date.
    date_created: String,
    trades: Vec<CdsTrade>,
}

impl CdsFile {
    pub fn new(
        user_id: &str,
        password: &str,
        transmission_id: &str,
        market_id: &str,
        date_created: &str,
    ) -> Self {
        CdsFile {
            user_id: user_id.to_string(),
            password: password.to_string(),
            transmission_id: transmission_id.to_string(),
            market_id: market_id.to_string(),
            date_created: date_created.to_string(),
            trades: Vec::new(),
        }
    }

    /// Add a trade to the export.
    pub fn add_trade(&mut self, t: CdsTrade) {
        self.trades.push(t);
    }

    /// Record 1 (79 B + LF): trade identification.
    fn record1(t: &CdsTrade, out: &mut String) {
        // Widths from CDSFile.h CDSRecord1.
        fill_numeric(out, 1, 2); // recordSequence
        fill_numeric(out, 3, 2); // recordCount
        fill_numeric(out, 5010, 4); // transactionID
        fill_alpha(out, &t.seller_cuid, 5);
        fill_alpha(out, &t.buyer_cuid, 5);
        fill_alpha(out, &t.trade_reference, 26);
        fill_alpha(out, "DP", 3); // tradeType: delivery vs payment
        fill_alpha(out, &t.trade_date, 8);
        fill_alpha(out, &t.value_date, 8);
        fill_alpha(out, &t.market_id, 5);
        fill_time(out, t.ts_us);
        fill_alpha(out, "", 6); // filler
        out.push('\n');
    }

    /// Record 2 (79 B + LF): security, quantity, price, net amount.
    fn record2(t: &CdsTrade, out: &mut String) {
        fill_numeric(out, 2, 2); // recordSequence
        fill_alpha(out, "CNS", 3); // modeOfSettlement
        fill_alpha(out, &t.isin, 12);
        fill_alpha(out, &t.symbol, 8);
        fill_alpha(out, &t.currency, 3);
        // quantity: 11 int width + 2 implied decimals (volume × 100).
        fill_decimal(out, t.volume, 11, 0, 2);
        // price: 8 int width + 9 implied decimals.
        fill_decimal(out, t.price, 8, 4, 9);
        fill_alpha(out, "", 13); // netAmount (spaces)
        fill_alpha(out, "", 9); // filler
        out.push('\n');
    }

    /// Record 3 (79 B + LF): account tags and settlement indicators.
    fn record3(t: &CdsTrade, out: &mut String) {
        fill_numeric(out, 3, 2); // recordSequence
        out.push('0'); // settlementType
        fill_alpha(out, &t.buyer_account, 10);
        fill_alpha(out, &t.seller_account, 10);
        fill_alpha(out, &t.buyer_sub, 2);
        fill_alpha(out, &t.seller_sub, 2);
        // Tag numbers: 3-char prefix + 10-char trade id.
        fill_alpha(out, "11A", 3);
        fill_alpha(out, &t.trade_id, 10);
        fill_alpha(out, "12A", 3);
        fill_alpha(out, &t.trade_id, 10);
        out.push('N'); // cancelIndicator
        out.push('N'); // nonResidentIndicator
        fill_alpha(out, "", 25); // filler
        out.push('\n');
    }

    /// The short date `YYMMDD` .
    fn short_date(&self) -> String {
        self.date_created
            .get(2..8)
            .unwrap_or(&self.date_created)
            .to_string()
    }

    /// Transmission header .
    fn transmission_header(&self, out: &mut String) {
        fill_alpha(out, "01", 2);
        fill_alpha(out, "01", 2);
        fill_alpha(out, "0001", 4);
        fill_alpha(out, &self.user_id, 8);
        fill_alpha(out, &self.password, 8);
        fill_alpha(out, &self.transmission_id, 4);
        fill_alpha(out, "001", 3);
        fill_alpha(out, &self.short_date(), 6);
        fill_alpha(out, "", 37);
        out.push('\n');
    }

    /// Batch header .
    fn batch_header(&self, out: &mut String) {
        fill_alpha(out, "01", 2);
        fill_alpha(out, "01", 2);
        fill_alpha(out, "0002", 4);
        fill_alpha(out, &self.user_id, 8);
        fill_alpha(out, &self.password, 8);
        fill_alpha(out, &self.short_date(), 6);
        fill_alpha(out, "", 6); // targetDate
        fill_alpha(out, "", 4); // targetRun
        fill_alpha(out, "", 6); // marketID
        fill_alpha(out, &self.market_id, 5); // transceiverCUID
        fill_alpha(out, "", 23);
        out.push('\n');
    }

    /// Batch trailer .
    fn batch_trailer(&self, out: &mut String) {
        fill_alpha(out, "01", 2);
        fill_alpha(out, "01", 2);
        fill_alpha(out, "0003", 4);
        fill_alpha(out, &self.user_id, 8);
        fill_alpha(out, &self.password, 8);
        fill_alpha(out, &self.short_date(), 6);
        fill_alpha(out, "", 6);
        fill_alpha(out, "", 4);
        fill_alpha(out, "", 29);
        out.push('\n');
    }

    /// Transmission trailer .
    fn transmission_trailer(&self, out: &mut String) {
        fill_alpha(out, "01", 2);
        fill_alpha(out, "01", 2);
        fill_alpha(out, "0004", 4);
        fill_alpha(out, &self.user_id, 8);
        fill_alpha(out, &self.password, 8);
        fill_alpha(out, &self.transmission_id, 4);
        fill_alpha(out, "001", 3);
        fill_alpha(out, &self.short_date(), 6);
        fill_alpha(out, "", 37);
        out.push('\n');
    }

    /// Render the complete file content.
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.transmission_header(&mut out);
        self.batch_header(&mut out);
        for t in &self.trades {
            Self::record1(t, &mut out);
            Self::record2(t, &mut out);
            Self::record3(t, &mut out);
        }
        self.batch_trailer(&mut out);
        self.transmission_trailer(&mut out);
        out
    }

    pub fn trade_count(&self) -> usize {
        self.trades.len()
    }
}

// ------------------------------------------------------------------------
// SIS / Fortis EOD files
// ------------------------------------------------------------------------

/// One transaction in the SIS/Fortis EOD export (shared record layout).
#[derive(Clone, Debug)]
pub struct EodTransaction {
    pub account: String,
    pub currency: String,
    pub exec_id: String,
    pub exec_ref_id: String,
    /// ExecTransType char (`0` new, `1` cancel, `2` correct).
    pub exec_trans_type: u8,
    /// IDSource int (2-char zero-padded field).
    pub id_source: u64,
    /// LastCapacity char (`1` agency, `P` principal).
    pub last_capacity: u8,
    /// Price as f64 (fills `%0*.*f` with 7 decimals).
    pub last_px: f64,
    pub last_shares: i64,
    pub order_id: String,
    pub security_id: String,
    /// Side char (`1` buy, `2` sell).
    pub side: u8,
    pub symbol: String,
    /// TransactTime string (17 chars, caller-formatted).
    pub transact_time: String,
    pub client_id: String,
    pub security_exchange: String,
    pub contra_broker: String,
    pub clearing_firm: String,
}

/// Fill helpers matching the host fill helpers / `SISFillIntField` /
/// `SISFillPriceField` (`%0*.*f`).
fn eod_fill_string(buf: &mut String, value: &str, width: usize) {
    fill_alpha(buf, value, width);
}

fn eod_fill_int(buf: &mut String, value: i64, width: usize) {
    // sprintf "%0*i" - sign  zero-pad.
    let s = value.abs().to_string();
    let sign = if value < 0 { "-" } else { "" };
    if s.len() + sign.len() >= width {
        buf.push_str(sign);
        buf.push_str(&s[s.len() + sign.len() - width..]);
    } else {
        buf.push_str(sign);
        buf.push_str(&"0".repeat(width - s.len() - sign.len()));
        buf.push_str(&s);
    }
}

fn eod_fill_price(buf: &mut String, value: f64, int_width: usize, decimals: usize) {
    // : sprintf("%0*.*f", sz1+sz2+1, sz2, value) - total width is
    // int_width + decimals + 1 (the +1 covers the decimal point); ALL
    // decimals are printed (no trailing-zero trimming).
    let total = int_width + decimals + 1;
    let s = format!("{:.*}", decimals, value);
    if s.len() >= total {
        buf.push_str(&s[s.len() - total..]);
    } else {
        buf.push_str(&"0".repeat(total - s.len()));
        buf.push_str(&s);
    }
}

/// EOD record widths shared by SIS and Fortis (`SISEndOfDayRecord.h`) -
/// documentation table (0 = single-char field).
#[allow(dead_code)]
const EOD_W: [(usize, &str); 18] = [
    (30, "account"),
    (3, "currency"),
    (20, "execID"),
    (20, "execRefID"),
    (0, "execTransType"), // single char
    (2, "IDSource"),
    (0, "lastCapacity"), // single char
    (18, "lastPX"),
    (10, "lastShares"),
    (10, "orderID"),
    (40, "securityID"), // fixed-width 12 in the host format, padded to 40 by memset
    (0, "side"),        // single char
    (20, "symbol"),
    (17, "transactTime"),
    (20, "clientID"),
    (3, "securityExchange"),
    (20, "contraBroker"),
    (20, "clearingFirm"),
];

/// Render one EOD transaction record (SIS/Fortis common fields). Returns the
/// record bytes WITHOUT the trailing reserved block (caller-specific) -
/// actually the reserved block is spaces, so include it here per variant.
fn eod_record_common(t: &EodTransaction, out: &mut String) {
    eod_fill_string(out, &t.account, 30);
    eod_fill_string(out, &t.currency, 3);
    eod_fill_string(out, &t.exec_id, 20);
    eod_fill_string(out, &t.exec_ref_id, 20);
    out.push(t.exec_trans_type as char);
    eod_fill_int(out, t.id_source as i64, 2);
    out.push(t.last_capacity as char);
    eod_fill_price(out, t.last_px, 10, 7);
    eod_fill_int(out, t.last_shares, 10);
    eod_fill_string(out, &t.order_id, 10);
    // fills securityID to 12 chars (rest of the 40 stays spaces).
    eod_fill_string(out, &t.security_id, 12);
    out.push(t.side as char);
    eod_fill_string(out, &t.symbol, 20);
    eod_fill_string(out, &t.transact_time, 17);
    eod_fill_string(out, &t.client_id, 20);
    eod_fill_string(out, &t.security_exchange, 3);
    eod_fill_string(out, &t.contra_broker, 20);
    eod_fill_string(out, &t.clearing_firm, 20);
}

/// SIS EOD file (the platform spec).
///
/// Layout: transaction records, then a trailer. Record: common fields +
/// `settlmntTyp='0'` + reserved(29) + `#` + LF. Trailer: sender(8) +
/// businessDate(8) + numberOfTransactions(10) + filler(268) + `#` + LF.
/// Filename: `VER1.ISIN.P.<YYYYMMDD>.0001.part`.
pub struct SisFile {
    /// `YYYYMMDD` business date.
    pub business_date: String,
    transactions: Vec<EodTransaction>,
}

impl SisFile {
    pub fn new(business_date: &str) -> Self {
        SisFile {
            business_date: business_date.to_string(),
            transactions: Vec::new(),
        }
    }

    /// filename: `VER1.ISIN.P.<date>.0001.part`.
    pub fn filename(&self) -> String {
        format!("VER1.ISIN.P.{}.0001.part", self.business_date)
    }

    pub fn add_transaction(&mut self, t: EodTransaction) {
        self.transactions.push(t);
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for t in &self.transactions {
            eod_record_common(t, &mut out);
            out.push('0'); // settlmntTyp
            out.push_str(&" ".repeat(29)); // reserved
            out.push('#');
            out.push('\n');
        }
        // Trailer.
        eod_fill_string(&mut out, "VER1", 8);
        eod_fill_string(&mut out, &self.business_date, 8);
        eod_fill_int(&mut out, self.transactions.len() as i64, 10);
        out.push_str(&" ".repeat(268));
        out.push('#');
        out.push('\n');
        out
    }

    pub fn transaction_count(&self) -> usize {
        self.transactions.len()
    }
}

/// Fortis EOD file (the platform spec).
///
/// Same record layout as SIS but WITHOUT `settlmntTyp`, 38 reserved bytes.
/// Filename: `<YYYYMMDD>.emcf`.
pub struct FortisFile {
    pub business_date: String,
    transactions: Vec<EodTransaction>,
}

impl FortisFile {
    pub fn new(business_date: &str) -> Self {
        FortisFile {
            business_date: business_date.to_string(),
            transactions: Vec::new(),
        }
    }

    /// filename: `<date>.emcf`.
    pub fn filename(&self) -> String {
        format!("{}.emcf", self.business_date)
    }

    pub fn add_transaction(&mut self, t: EodTransaction) {
        self.transactions.push(t);
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for t in &self.transactions {
            eod_record_common(t, &mut out);
            out.push_str(&" ".repeat(38)); // reserved
            out.push('#');
            out.push('\n');
        }
        // Trailer (same shape as SIS).
        eod_fill_string(&mut out, "VER1", 8);
        eod_fill_string(&mut out, &self.business_date, 8);
        eod_fill_int(&mut out, self.transactions.len() as i64, 10);
        out.push_str(&" ".repeat(268));
        out.push('#');
        out.push('\n');
        out
    }

    pub fn transaction_count(&self) -> usize {
        self.transactions.len()
    }
}

// Layout table kept for documentation parity (see EOD_W).
#[allow(dead_code)]
fn _eod_layout_doc() -> [(usize, &'static str); 18] {
    EOD_W
}

// ------------------------------------------------------------------------
// Settlement netting CSV (design: clearing doc §4.4 - "settlement netting
// feed", owned by Exchange8:Settlement)
// ------------------------------------------------------------------------

/// One trade side to net.
#[derive(Clone, Debug)]
pub struct NettingTrade {
    /// Clearing participant CUID.
    pub cuid: String,
    pub security_id: String,
    /// +qty for buys, −qty for sells (per participant's side).
    pub signed_qty: i64,
    /// Gross settled amount (signed; the same sign as signed_qty).
    pub signed_amount: i64,
}

/// Netting row: aggregated position per (cuid, security).
#[derive(Debug, PartialEq)]
pub struct NettingRow {
    pub cuid: String,
    pub security_id: String,
    pub net_qty: i64,
    pub net_amount: i64,
}

/// Aggregate trades into net settlement positions per (cuid, security).
///
/// Signed quantities net buys against sells; a flat net is still reported
/// (zero position, zero amount) so the CSV matches the participant universe.
pub fn net_trades(trades: &[NettingTrade]) -> Vec<NettingRow> {
    let mut rows: Vec<NettingRow> = Vec::new();
    for t in trades {
        if let Some(row) = rows
            .iter_mut()
            .find(|r| r.cuid == t.cuid && r.security_id == t.security_id)
        {
            row.net_qty += t.signed_qty;
            row.net_amount += t.signed_amount;
        } else {
            rows.push(NettingRow {
                cuid: t.cuid.clone(),
                security_id: t.security_id.clone(),
                net_qty: t.signed_qty,
                net_amount: t.signed_amount,
            });
        }
    }
    // Deterministic order: participant, then security.
    rows.sort_by(|a, b| (&a.cuid, &a.security_id).cmp(&(&b.cuid, &b.security_id)));
    rows
}

/// Render the netting CSV: `cuid,security_id,net_qty,net_amount` with a
/// header line (the CCP reconciliation backstop per design §4.4).
pub fn render_netting_csv(rows: &[NettingRow]) -> String {
    let mut out = String::from("cuid,security_id,net_qty,net_amount\n");
    for r in rows {
        out.push_str(&format!(
            "{},{},{},{}\n",
            r.cuid, r.security_id, r.net_qty, r.net_amount
        ));
    }
    out
}

#[cfg(test)]
mod netting_tests {
    use super::*;

    fn t(cuid: &str, sec: &str, qty: i64, amount: i64) -> NettingTrade {
        NettingTrade {
            cuid: cuid.into(),
            security_id: sec.into(),
            signed_qty: qty,
            signed_amount: amount,
        }
    }

    #[test]
    fn netting_aggregates_and_nets() {
        // Participant B: buys 100 + 50, sells 150 → net 0.
        let rows = net_trades(&[
            t("B123", "ABC", 100, 1_005_000),
            t("B123", "ABC", 50, 502_500),
            t("B123", "ABC", -150, -1_507_500),
        ]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].net_qty, 0);
        assert_eq!(rows[0].net_amount, 0);
    }

    #[test]
    fn netting_separates_participants_and_securities() {
        let rows = net_trades(&[
            t("A", "ABC", 100, 1_000_000),
            t("B", "ABC", -100, -1_000_000),
            t("A", "XYZ", -50, -250_000),
        ]);
        assert_eq!(rows.len(), 3);
        // Sorted by (cuid, security): A/ABC, A/XYZ, B/ABC.
        assert_eq!(rows[0].cuid, "A");
        assert_eq!(rows[0].security_id, "ABC");
        assert_eq!(rows[1].security_id, "XYZ");
        assert_eq!(rows[2].cuid, "B");
    }

    #[test]
    fn netting_csv_layout() {
        let rows = net_trades(&[
            t("C01", "ABC", 300, 3_000_000),
            t("C01", "ABC", -100, -1_000_000),
        ]);
        let csv = render_netting_csv(&rows);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "cuid,security_id,net_qty,net_amount");
        assert_eq!(lines[1], "C01,ABC,200,2000000");
        assert!(csv.ends_with('\n'));
    }

    #[test]
    fn netting_empty_trades() {
        let rows = net_trades(&[]);
        assert!(rows.is_empty());
        let csv = render_netting_csv(&rows);
        assert_eq!(csv, "cuid,security_id,net_qty,net_amount\n");
    }

    #[test]
    fn netting_partial_position_keeps_sign() {
        // Net long and net short keep their signs in amount and qty.
        let long = net_trades(&[t("L", "S", 250, 2_500_000), t("L", "S", -100, -1_000_000)]);
        assert_eq!(long[0].net_qty, 150);
        assert_eq!(long[0].net_amount, 1_500_000);
        let short = net_trades(&[t("S", "S", -250, -2_500_000), t("S", "S", 100, 1_000_000)]);
        assert_eq!(short[0].net_qty, -150);
        assert_eq!(short[0].net_amount, -1_500_000);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_trade() -> CdsTrade {
        CdsTrade {
            trade_reference: "20260829-9001".into(),
            trade_date: "20260829".into(),
            value_date: "20260901".into(),
            ts_us: 10 * 3_600_000_000 + 15 * 60_000_000 + 30_000_000, // 10:15:30
            reporting_side_is_buy: true,
            buyer_cuid: "B1234".into(),
            seller_cuid: "S5678".into(),
            isin: "CA1234567890".into(),
            symbol: "ABC".into(),
            currency: "CAD".into(),
            volume: 100,
            price: 100_500, // 10.05 at scale 4
            buyer_account: "ACCT000001".into(),
            seller_account: "ACCT000002".into(),
            buyer_sub: "01".into(),
            seller_sub: "02".into(),
            trade_id: "9001".into(),
            market_id: "XTSE".into(),
        }
    }

    #[test]
    fn fill_alpha_pads_and_truncates() {
        let mut s = String::new();
        fill_alpha(&mut s, "ABC", 5);
        assert_eq!(s, "ABC  ");
        let mut s2 = String::new();
        fill_alpha(&mut s2, "TOOLONGVALUE", 5);
        assert_eq!(s2, "TOOLO");
        let mut s3 = String::new();
        fill_alpha(&mut s3, "", 3);
        assert_eq!(s3, "   ");
    }

    #[test]
    fn fill_numeric_zero_pads() {
        let mut s = String::new();
        fill_numeric(&mut s, 7, 4);
        assert_eq!(s, "0007");
        let mut s2 = String::new();
        fill_numeric(&mut s2, 123456, 4);
        assert_eq!(s2, "3456"); // fixed-width field truncates from the left per spec
    }

    #[test]
    fn fill_decimal_scales() {
        // Volume 100 at 2 implied decimals → 100×100 = 10_000, 13 chars.
        let mut s = String::new();
        fill_decimal(&mut s, 100, 11, 0, 2);
        assert_eq!(s, "0000000010000");
        // Price 100_500 (scale 4) at 9 implied decimals → ×10^5
        // = 10_050_000_000 (11 digits) padded to 8+9 = 17 chars.
        let mut s2 = String::new();
        fill_decimal(&mut s2, 100_500, 8, 4, 9);
        assert_eq!(s2, "00000010050000000");
    }

    #[test]
    fn fill_time_formats_hhmmss() {
        let mut s = String::new();
        fill_time(&mut s, (10 * 3600 + 15 * 60 + 30) * 1_000_000);
        assert_eq!(s, "101530");
    }

    #[test]
    fn record_lengths_match_struct_field_widths() {
        // Widths from CDSFile.h struct definitions (+ LF):
        // record1: seq2+count2+txn4+seller5+buyer5+ref26+type3+date8+value8
        //         +market5+time6+filler6 = 80 (+1 LF).
        let t = sample_trade();
        let mut r1 = String::new();
        CdsFile::record1(&t, &mut r1);
        assert_eq!(r1.len(), 2 + 2 + 4 + 5 + 5 + 26 + 3 + 8 + 8 + 5 + 6 + 6 + 1);
        // record2: seq2+mode3+isin12+symbol8+ccy3+qty13+price17+net13+filler9
        //         = 81 (+1 LF).
        let mut r2 = String::new();
        CdsFile::record2(&t, &mut r2);
        assert_eq!(r2.len(), 2 + 3 + 12 + 8 + 3 + 13 + 17 + 13 + 9 + 1);
        // record3: seq2+type1+buyer10+seller10+bsub2+ssub2+tag13+tag13+cancel1
        //         +nonres1+filler25 = 81 (+1 LF).
        let mut r3 = String::new();
        CdsFile::record3(&t, &mut r3);
        assert_eq!(r3.len(), 2 + 1 + 10 + 10 + 2 + 2 + 13 + 13 + 1 + 1 + 25 + 1);
        // All end with LF.
        assert!(r1.ends_with('\n') && r2.ends_with('\n') && r3.ends_with('\n'));
    }

    #[test]
    fn record1_field_positions() {
        let t = sample_trade();
        let mut r1 = String::new();
        CdsFile::record1(&t, &mut r1);
        let b = r1.as_bytes();
        assert_eq!(&b[0..2], b"01"); // sequence
        assert_eq!(&b[2..4], b"03"); // count
        assert_eq!(&b[4..8], b"5010"); // transaction
        assert_eq!(&b[8..13], b"S5678"); // seller CUID
        assert_eq!(&b[13..18], b"B1234"); // buyer CUID
        assert_eq!(&b[18..44], b"20260829-9001             "); // ref (26)
        assert_eq!(&b[44..47], b"DP "); // trade type
        assert_eq!(&b[47..55], b"20260829"); // trade date
        assert_eq!(&b[55..63], b"20260901"); // value date
        assert_eq!(&b[63..68], b"XTSE "); // market id
        assert_eq!(&b[68..74], b"101530"); // exec time
    }

    #[test]
    fn record2_field_positions() {
        let t = sample_trade();
        let mut r2 = String::new();
        CdsFile::record2(&t, &mut r2);
        let b = r2.as_bytes();
        assert_eq!(&b[0..2], b"02");
        assert_eq!(&b[2..5], b"CNS");
        assert_eq!(&b[5..17], b"CA1234567890");
        assert_eq!(&b[17..25], b"ABC     ");
        assert_eq!(&b[25..28], b"CAD");
        // quantity 100 → 100.00 → 13 digits.
        assert_eq!(&b[28..41], b"0000000010000");
        // price 10.05 → ×10^5 = 10_050_000_000 → 17 digits.
        assert_eq!(&b[41..58], b"00000010050000000");
    }

    #[test]
    fn record3_field_positions() {
        let t = sample_trade();
        let mut r3 = String::new();
        CdsFile::record3(&t, &mut r3);
        let b = r3.as_bytes();
        assert_eq!(&b[0..2], b"03");
        assert_eq!(b[2], b'0'); // settlementType
        assert_eq!(&b[3..13], b"ACCT000001");
        assert_eq!(&b[13..23], b"ACCT000002");
        assert_eq!(&b[23..25], b"01");
        assert_eq!(&b[25..27], b"02");
        // buyer tag: "11A" + 10-char trade id (13-wide field).
        assert_eq!(&b[27..40], b"11A9001      ");
        assert_eq!(&b[40..53], b"12A9001      ");
        assert_eq!(b[53], b'N'); // cancel
        assert_eq!(b[54], b'N'); // non-resident
    }

    #[test]
    fn full_file_structure() {
        let mut f = CdsFile::new("USER01", "SECRET01", "0001", "XTSE", "20260829");
        f.add_trade(sample_trade());
        f.add_trade(sample_trade());
        let content = f.render();
        let lines: Vec<&str> = content.lines().collect();
        // transmission hdr + batch hdr + (3 records × 2 trades) + batch trl
        // + transmission trl = 10 lines.
        assert_eq!(lines.len(), 2 + 6 + 2);
        assert_eq!(f.trade_count(), 2);
        // Headers carry transaction ids 0001/0002, trailers 0003/0004.
        assert_eq!(&lines[0].as_bytes()[4..8], b"0001");
        assert_eq!(&lines[1].as_bytes()[4..8], b"0002");
        assert_eq!(&lines[8].as_bytes()[4..8], b"0003");
        assert_eq!(&lines[9].as_bytes()[4..8], b"0004");
        // The file ends with a newline (writes LF-terminated records).
        assert!(content.ends_with('\n'));
    }

    #[test]
    fn header_carries_credentials_and_date() {
        let f = CdsFile::new("USER01", "SECRET01", "0001", "XTSE", "20260829");
        let content = f.render();
        let hdr = content.lines().next().unwrap();
        // Layout: seq(2) count(2) txn(4) user(8) pwd(8) trans(4) tseq(3) date(6).
        assert_eq!(&hdr[8..16], "USER01  ");
        assert_eq!(&hdr[16..24], "SECRET01");
        assert_eq!(&hdr[24..28], "0001");
        assert_eq!(&hdr[28..31], "001");
        assert_eq!(&hdr[31..37], "260829"); // YYMMDD
    }

    #[test]
    fn empty_file_is_just_headers_and_trailers() {
        let f = CdsFile::new("U", "P", "0001", "X", "20260829");
        let content = f.render();
        // 4 structural lines, no trade records.
        assert_eq!(content.lines().count(), 4);
        assert_eq!(f.trade_count(), 0);
    }

    // ---------------------------------------------------------------
    // SIS / Fortis EOD builders - pinned to SISEndOfDayFile.h /
    // FortisEndOfDayFile.h struct widths + the .cpp fill order.
    // ---------------------------------------------------------------

    fn sample_eod_transaction() -> EodTransaction {
        EodTransaction {
            account: "ACCT000001".into(),
            currency: "CAD".into(),
            exec_id: "E9001".into(),
            exec_ref_id: "".into(),
            exec_trans_type: b'0',
            id_source: 4,
            last_capacity: b'1',
            last_px: 10.05,
            last_shares: 100,
            order_id: "O12345".into(),
            security_id: "CA1234567890".into(),
            side: b'1',
            symbol: "ABC".into(),
            transact_time: "2026-08-29 10:15".into(),
            client_id: "CL01".into(),
            security_exchange: "TSX".into(),
            contra_broker: "B202".into(),
            clearing_firm: "F301".into(),
        }
    }

    #[test]
    fn sis_record_layout_and_lengths() {
        let mut f = SisFile::new("20260829");
        f.add_transaction(sample_eod_transaction());
        let content = f.render();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2); // 1 record + trailer
        let rec = lines[0].as_bytes();
        // account[30] currency[3] execID[20] execRefID[20] execTransType[1]
        // IDSource[2] lastCapacity[1] lastPX[18] lastShares[10] orderID[10]
        // securityID[12-filled] side[1] symbol[20] transactTime[17]
        // clientID[20] securityExchange[3] contraBroker[20] clearingFirm[20]
        // settlmntTyp[1] reserved[29] #[1] LF[1]
        assert_eq!(
            rec.len(),
            30 + 3
                + 20
                + 20
                + 1
                + 2
                + 1
                + 18
                + 10
                + 10
                + 12
                + 1
                + 20
                + 17
                + 20
                + 3
                + 20
                + 20
                + 1
                + 29
                + 1
        );
        assert_eq!(&rec[0..10], b"ACCT000001");
        assert_eq!(&rec[30..33], b"CAD");
        assert_eq!(&rec[33..38], b"E9001"); // execID at 33
        assert_eq!(rec[73], b'0'); // execTransType at 73
        assert_eq!(&rec[74..76], b"04"); // IDSource zero-padded
        assert_eq!(rec[76], b'1'); // lastCapacity
                                   // lastPX at 77..95: "10.0500000" zero-padded in 18 chars.
        assert_eq!(&rec[77..95], b"0000000010.0500000");
        assert_eq!(&rec[95..105], b"0000000100"); // lastShares
        assert_eq!(&rec[105..111], b"O12345"); // orderID at 105
        assert_eq!(&rec[115..127], b"CA1234567890"); // securityID 12-filled
        assert_eq!(rec[127], b'1'); // side at 127
        assert_eq!(&rec[128..131], b"ABC"); // symbol at 128 (20-wide, left)
                                            // settlmntTyp at 228, reserved 229..258, '#' at 258.
        assert_eq!(rec[228], b'0');
        assert_eq!(rec[258], b'#');
    }

    #[test]
    fn sis_trailer_layout() {
        let mut f = SisFile::new("20260829");
        f.add_transaction(sample_eod_transaction());
        f.add_transaction(sample_eod_transaction());
        let content = f.render();
        let trailer = content.lines().last().unwrap().as_bytes();
        // sender[8] businessDate[8] count[10] filler[268] '#'.
        assert_eq!(trailer.len(), 8 + 8 + 10 + 268 + 1);
        assert_eq!(&trailer[0..4], b"VER1");
        assert_eq!(&trailer[8..16], b"20260829");
        assert_eq!(&trailer[16..26], b"0000000002");
        assert_eq!(trailer[trailer.len() - 1], b'#');
        assert_eq!(f.transaction_count(), 2);
    }

    #[test]
    fn sis_filename() {
        let f = SisFile::new("20260829");
        assert_eq!(f.filename(), "VER1.ISIN.P.20260829.0001.part");
    }

    #[test]
    fn fortis_record_missing_settlmnt_and_wider_reserved() {
        let mut f = FortisFile::new("20260829");
        f.add_transaction(sample_eod_transaction());
        let content = f.render();
        let rec = content.lines().next().unwrap().as_bytes();
        // Fortis: common fields + reserved[38] + '#' - no settlmntTyp.
        assert_eq!(
            rec.len(),
            30 + 3
                + 20
                + 20
                + 1
                + 2
                + 1
                + 18
                + 10
                + 10
                + 12
                + 1
                + 20
                + 17
                + 20
                + 3
                + 20
                + 20
                + 38
                + 1
        );
        // The last char before '#' is a reserved space (SIS has '0' there).
        assert_eq!(rec[rec.len() - 2], b' ');
        assert_eq!(rec[rec.len() - 1], b'#');
    }

    #[test]
    fn fortis_trailer_and_filename() {
        let mut f = FortisFile::new("20260829");
        f.add_transaction(sample_eod_transaction());
        let content = f.render();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        let trailer = lines[1].as_bytes();
        assert_eq!(&trailer[0..4], b"VER1");
        assert_eq!(&trailer[16..26], b"0000000001");
        assert_eq!(f.filename(), "20260829.emcf");
    }

    #[test]
    fn empty_sis_is_trailer_only() {
        let f = SisFile::new("20260829");
        let content = f.render();
        assert_eq!(content.lines().count(), 1);
        assert_eq!(&content.as_bytes()[16..26], b"0000000000");
    }

    #[test]
    fn eod_price_field_semantics() {
        // %0*.*f with sz1=10, sz2=7 → 18 chars: 10 int + '.' + 7 decimals.
        let mut s = String::new();
        eod_fill_price(&mut s, 10.05, 10, 7);
        assert_eq!(s, "0000000010.0500000");
        assert_eq!(s.len(), 18);
        // Whole-number price keeps the decimal point.
        let mut s2 = String::new();
        eod_fill_price(&mut s2, 10.0, 10, 7);
        assert_eq!(s2, "0000000010.0000000");
        // Large price fills the int width without truncation damage.
        let mut s3 = String::new();
        eod_fill_price(&mut s3, 9999999999.0, 10, 7);
        assert_eq!(s3, "9999999999.0000000");
    }

    #[test]
    fn eod_int_field_zero_pads() {
        let mut s = String::new();
        eod_fill_int(&mut s, 100, 10);
        assert_eq!(s, "0000000100");
        let mut s2 = String::new();
        eod_fill_int(&mut s2, 0, 10);
        assert_eq!(s2, "0000000000");
    }
}
