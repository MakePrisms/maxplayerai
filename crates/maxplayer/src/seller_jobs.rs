//! `maxplayer seller jobs` — every job this seat won, newest first, with whether it was paid.
//!
//! `maxplayer seller fees` lists only jobs with a collected payment, so a job that was delivered but
//! never paid did not show anywhere a seller could read it. This prints each job's state, its
//! payment (free, paid, awaiting payment within the buyer's pay window, or unpaid after it), the
//! platform fee's remittance for a paid job, and the latest remittance attempt when it did not pay.
//! Read-only: it opens `seller.sqlite`, reads, prints and exits; it moves no sats.

use std::io::Write;
use std::path::PathBuf;

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
#[cfg(feature = "wallet")]
const RUNTIME_ERROR: i32 = 2;
const DEFAULT_LIMIT: usize = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Options {
    home: Option<PathBuf>,
    limit: usize,
    json: bool,
}

/// Entry from `sell::run` for `maxplayer seller jobs ...`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if crate::cli::is_help_request(args) {
        usage(out);
        return SUCCESS;
    }
    let options = match parse(args) {
        Ok(options) => options,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            usage(err);
            return USAGE_ERROR;
        }
    };

    #[cfg(not(feature = "wallet"))]
    {
        let _ = (options, out);
        let _ = writeln!(
            err,
            "maxplayer seller jobs requires the wallet feature (rebuild with default features)"
        );
        USAGE_ERROR
    }

    #[cfg(feature = "wallet")]
    {
        match print_jobs(&options, out) {
            Ok(()) => SUCCESS,
            Err(message) => {
                let _ = writeln!(err, "{message}");
                RUNTIME_ERROR
            }
        }
    }
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        home: None,
        limit: DEFAULT_LIMIT,
        json: false,
    };
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--home" => {
                idx += 1;
                options.home = Some(PathBuf::from(
                    args.get(idx).ok_or("--home requires a value")?,
                ));
            }
            "--limit" => {
                idx += 1;
                let raw = args.get(idx).ok_or("--limit requires a value")?;
                options.limit = raw
                    .parse::<usize>()
                    .ok()
                    .filter(|limit| *limit > 0)
                    .ok_or_else(|| format!("--limit must be a positive number, got {raw}"))?;
            }
            "--json" => options.json = true,
            other => return Err(format!("unknown seller jobs option: {other}")),
        }
        idx += 1;
    }
    Ok(options)
}

fn usage(w: &mut dyn Write) {
    let _ = writeln!(
        w,
        "Usage:\n  maxplayer seller jobs [--home <dir>] [--limit <n>] [--json]\n\nLists the jobs this seat won, newest first (default {DEFAULT_LIMIT}), each with its state and\nwhether it was paid: free, paid (with the platform fee's remittance), delivered and awaiting\npayment (the buyer can pay for 7 days after delivery), or unpaid once that window closed. When the\nlatest platform-fee remittance attempt did not pay, it prints that attempt's error too; see\n`maxplayer seller fees remit` to retry it. Moves no sats (it only opens, reads and prints the\nseller store). --json prints a flat array, one object per job: job_id, buyer,\nstatus (in_progress | failed | free | paid | awaiting_payment | settled_elsewhere | unpaid),\namount_sats, mint (null until paid), created_at_unix, delivered_at_unix, paid_at_unix,\npay_by_unix (awaiting_payment and unpaid only), fee_sats, fee_status (null unless paid; none |\nin_flight | spending | remitted | unremitted), fee_sent (true once nothing is owed) and fee_error.\nA session job is listed by its first turn only. The store is opened read-only; one an older\nbinary left is refused until `seller serve` from this binary migrates it. A seat that never ran\n`seller serve` has no store: that is an error (exit 2), not an empty list. Exit 0 = printed;\n1 = usage; 2 = error."
    );
}

#[cfg(feature = "wallet")]
fn print_jobs(options: &Options, out: &mut dyn Write) -> Result<(), String> {
    use maxplayer_core::seller_node::store::SellerStore;

    let root = match &options.home {
        Some(path) => path.clone(),
        None => maxplayer_core::home::default_home_dir()
            .map_err(|error| format!("resolve home: {error}"))?,
    };
    let db = root.join(maxplayer_core::seller_node::STATE_DB_FILE);
    if !db.exists() {
        return Err(format!(
            "no seller store at {} — this seat has not run `maxplayer seller serve` yet",
            db.display()
        ));
    }
    // Read-only: no WAL switch, no migration of a store a running `seller serve` owns (#1451 r1 M6).
    let store = SellerStore::open_read_only(&db)
        .map_err(|error| format!("open {}: {error}", db.display()))?;
    let snapshot = store
        .job_payments_snapshot(options.limit)
        .map_err(|error| format!("read jobs: {error}"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    if options.json {
        let _ = writeln!(out, "{}", json_rows(&snapshot, now));
    } else {
        let _ = write!(out, "{}", render(&snapshot, now, &db.display().to_string()));
    }
    Ok(())
}

/// The latest remittance attempt, when it did not pay and still matters: some platform fee on the
/// seat is unsent, and the remittance it names (if any) has not since settled — a later
/// reconciliation that settles it journals no new attempt, so the failure would otherwise linger.
#[cfg(feature = "wallet")]
fn active_failed_attempt(
    snapshot: &maxplayer_core::seller_node::store::JobPaymentsSnapshot,
) -> Option<&maxplayer_core::seller_node::store::RemitAttempt> {
    use maxplayer_core::seller_node::store::RemitAttemptOutcome;
    let attempt = snapshot.last_attempt.as_ref()?;
    if attempt.outcome == RemitAttemptOutcome::Paid || snapshot.unsent_fee_sats == 0 {
        return None;
    }
    if attempt.remittance_id.is_some()
        && snapshot.last_attempt_remittance_state.as_deref() == Some("settled")
    {
        return None;
    }
    Some(attempt)
}

/// Whether the failed attempt applies to this paid job's fee. An attempt that names a remittance
/// applies to the receipts pinned to it (a melt that failed or is held leaves them pinned, the
/// remittance `planned`/`spending`). One that names none — it failed before planning, or its plan
/// was released — applies to the fees still unremitted when it started (same-second ties count as
/// covered: the store keeps whole seconds).
#[cfg(feature = "wallet")]
fn attempt_covers(
    attempt: &maxplayer_core::seller_node::store::RemitAttempt,
    attempt_remittance_state: Option<&str>,
    receipt: &maxplayer_core::seller_node::store::JobReceipt,
) -> bool {
    let fee = fee_status(receipt);
    if matches!(fee, FeeStatus::NoFee | FeeStatus::Remitted) {
        return false;
    }
    let by_time = fee == FeeStatus::Unremitted && attempt.started_at_unix >= receipt.received_at_unix;
    match attempt.remittance_id.as_deref() {
        Some(id) if receipt.remittance_id.as_deref() == Some(id) => true,
        Some(_) => attempt_remittance_state == Some("failed") && by_time,
        None => by_time,
    }
}

/// `--json`: a flat array, one object per job, NEWEST first, with these keys (stable):
/// - `job_id`, `buyer` (`null` when the offer row is gone);
/// - `status`: one of [`PayStatus::key`];
/// - `amount_sats`: what was paid, else the offer amount;
/// - `mint`: the paying mint, `null` until a payment landed;
/// - `created_at_unix`, `delivered_at_unix`, `paid_at_unix`;
/// - `pay_by_unix`: the end of the buyer's pay window, only on `awaiting_payment` and `unpaid`;
/// - `fee_sats`, `fee_status` (`null` unless paid; else one of [`FeeStatus::key`]);
/// - `fee_sent`: `true` once nothing is owed on a paid job (remitted, or no fee); `false` otherwise;
/// - `fee_error`: the seat's latest remittance failure, only on a paid job whose fee it applies to
///   ([`attempt_covers`]) and only while it is still unresolved ([`active_failed_attempt`]).
///
/// Turn-1 rows only: a session job's later turns are not listed yet.
#[cfg(feature = "wallet")]
fn json_rows(snapshot: &maxplayer_core::seller_node::store::JobPaymentsSnapshot, now: i64) -> serde_json::Value {
    let attempt = active_failed_attempt(snapshot);
    serde_json::Value::Array(
        snapshot
            .rows
            .iter()
            .map(|row| {
                let status = pay_status(row, now);
                let fee = row.receipt.as_ref().map(fee_status);
                let fee_error = match (row.receipt.as_ref(), attempt) {
                    (Some(receipt), Some(attempt))
                        if attempt_covers(
                            attempt,
                            snapshot.last_attempt_remittance_state.as_deref(),
                            receipt,
                        ) =>
                    {
                        Some(format!("{}: {}", attempt.outcome.as_str(), attempt.detail))
                    }
                    _ => None,
                };
                let pay_by_unix = match status {
                    PayStatus::AwaitingPayment { .. } | PayStatus::Unpaid => row.pay_by_unix,
                    _ => None,
                };
                serde_json::json!({
                    "job_id": row.job_id,
                    "buyer": row.buyer_pubkey,
                    "status": status.key(),
                    "amount_sats": paid_or_offered_sats(row),
                    "mint": row.mint,
                    "created_at_unix": row.created_at_unix,
                    "delivered_at_unix": row.delivered_at_unix,
                    "paid_at_unix": row.receipt.as_ref().map(|r| r.received_at_unix),
                    "pay_by_unix": pay_by_unix,
                    "fee_sats": row.receipt.as_ref().map(|r| r.fee_sats),
                    "fee_status": fee.map(FeeStatus::key),
                    "fee_sent": matches!(fee, Some(FeeStatus::Remitted | FeeStatus::NoFee)),
                    "fee_error": fee_error,
                })
            })
            .collect(),
    )
}

#[cfg(feature = "wallet")]
fn paid_or_offered_sats(row: &maxplayer_core::seller_node::store::JobPaymentRow) -> Option<u64> {
    row.receipt.as_ref().map(|r| r.amount_sats).or(row.offer_sats)
}

/// Whether one job was paid, as `seller jobs` reports it.
#[cfg(feature = "wallet")]
#[derive(Debug, Clone, PartialEq, Eq)]
enum PayStatus {
    InProgress,
    Failed,
    Free,
    Paid,
    AwaitingPayment { pay_by_unix: i64 },
    SettledElsewhere,
    Unpaid,
}

#[cfg(feature = "wallet")]
impl PayStatus {
    fn key(&self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Failed => "failed",
            Self::Free => "free",
            Self::Paid => "paid",
            Self::AwaitingPayment { .. } => "awaiting_payment",
            Self::SettledElsewhere => "settled_elsewhere",
            Self::Unpaid => "unpaid",
        }
    }
}

/// A receipt wins; then a failed job; then the relay-derived settled marker, which is written on a
/// resumed job with NO local delivery (so it must be read before "not delivered yet"); then the
/// delivery and its pay window, inclusive of its last second like the buyer's
/// (`job_lifecycle::derive_claim_liveness`).
#[cfg(feature = "wallet")]
fn pay_status(row: &maxplayer_core::seller_node::store::JobPaymentRow, now: i64) -> PayStatus {
    if row.receipt.is_some() {
        return PayStatus::Paid;
    }
    if row.state == "failed" {
        return PayStatus::Failed;
    }
    if row.settled_elsewhere_at_unix.is_some() {
        return PayStatus::SettledElsewhere;
    }
    match row.delivered_at_unix {
        None => PayStatus::InProgress,
        Some(_) if row.payment == "none" => PayStatus::Free,
        Some(_) => match row.pay_by_unix {
            Some(pay_by_unix) if now <= pay_by_unix => PayStatus::AwaitingPayment { pay_by_unix },
            _ => PayStatus::Unpaid,
        },
    }
}

/// Where a paid job's platform fee stands.
#[cfg(feature = "wallet")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeeStatus {
    /// No fee on this payment (under 10 sats at 10%, or collected before fees were recorded).
    NoFee,
    Remitted,
    /// In a planned remittance whose payment has not been sent yet.
    InFlight,
    /// In a remittance whose payment was sent and awaits the mint's answer (a failed or held melt
    /// sits here); the node reconciles it automatically.
    Spending,
    Unremitted,
}

#[cfg(feature = "wallet")]
impl FeeStatus {
    fn key(self) -> &'static str {
        match self {
            Self::NoFee => "none",
            Self::Remitted => "remitted",
            Self::InFlight => "in_flight",
            Self::Spending => "spending",
            Self::Unremitted => "unremitted",
        }
    }
}

#[cfg(feature = "wallet")]
fn fee_status(receipt: &maxplayer_core::seller_node::store::JobReceipt) -> FeeStatus {
    if receipt.fee_sats == 0 {
        return FeeStatus::NoFee;
    }
    match (receipt.remittance_id.as_ref(), receipt.remittance_state.as_deref()) {
        (Some(_), Some("settled")) => FeeStatus::Remitted,
        (Some(_), Some("planned")) => FeeStatus::InFlight,
        (Some(_), Some("spending")) => FeeStatus::Spending,
        _ => FeeStatus::Unremitted,
    }
}

/// `YYYY-MM-DD HH:MM UTC` for a unix time (proleptic Gregorian; no time-zone database).
fn utc(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        secs / 3_600,
        (secs % 3_600) / 60
    )
}

/// The job list as text. Pure, so a test can assert on the exact words a seller reads.
#[cfg(feature = "wallet")]
fn render(
    snapshot: &maxplayer_core::seller_node::store::JobPaymentsSnapshot,
    now: i64,
    db: &str,
) -> String {
    let mut text = format!("Seller jobs — {db}\n");
    if snapshot.rows.is_empty() {
        text.push_str("No jobs won yet.\n");
        return text;
    }
    let attempt = active_failed_attempt(snapshot);
    let mut awaiting = 0usize;
    let mut unpaid = 0usize;
    for row in &snapshot.rows {
        let amount = match paid_or_offered_sats(row) {
            Some(sats) if row.payment != "none" => format!("{sats} sats"),
            Some(_) => "free".to_owned(),
            None => "amount unknown".to_owned(),
        };
        let status = match pay_status(row, now) {
            PayStatus::InProgress => format!("{} — not delivered yet", row.state),
            PayStatus::Failed => "failed — nothing delivered, nothing owed".to_owned(),
            PayStatus::Free => "delivered — free job, no payment due".to_owned(),
            PayStatus::Paid => {
                let receipt = row.receipt.as_ref().expect("paid has a receipt");
                let fee = match fee_status(receipt) {
                    FeeStatus::NoFee => "no platform fee".to_owned(),
                    FeeStatus::Remitted => format!("platform fee {} sats remitted", receipt.fee_sats),
                    FeeStatus::InFlight => {
                        format!("platform fee {} sats being remitted", receipt.fee_sats)
                    }
                    FeeStatus::Spending => format!(
                        "platform fee {} sats held: its payment awaits the mint's answer (reconciled automatically)",
                        receipt.fee_sats
                    ),
                    FeeStatus::Unremitted => format!(
                        "platform fee {} sats unremitted (the node remits it automatically)",
                        receipt.fee_sats
                    ),
                };
                let failed = match attempt {
                    Some(attempt)
                        if attempt_covers(
                            attempt,
                            snapshot.last_attempt_remittance_state.as_deref(),
                            receipt,
                        ) =>
                    {
                        " (the last remittance attempt for it did not pay; see below)"
                    }
                    _ => "",
                };
                format!(
                    "PAID {} sats at {} — {fee}{failed}",
                    receipt.amount_sats,
                    utc(receipt.received_at_unix)
                )
            }
            PayStatus::AwaitingPayment { pay_by_unix } => {
                awaiting += 1;
                format!(
                    "delivered — AWAITING PAYMENT (the buyer can pay until {})",
                    utc(pay_by_unix)
                )
            }
            PayStatus::SettledElsewhere => {
                "settled per the relay; no payment collected by this seat".to_owned()
            }
            PayStatus::Unpaid => {
                unpaid += 1;
                "delivered — UNPAID (the buyer's 7-day pay window has closed)".to_owned()
            }
        };
        text.push_str(&format!(
            "  job {}  {}  {amount}\n    {status}\n",
            row.job_id,
            utc(row.created_at_unix)
        ));
    }
    if awaiting > 0 || unpaid > 0 {
        text.push_str(&format!(
            "{awaiting} delivered job(s) awaiting payment, {unpaid} unpaid after the pay window.\n"
        ));
    }
    if let Some(attempt) = attempt {
        text.push_str(&format!(
            "Seat-wide: the latest platform-fee remittance attempt {} at {}: {}\n  {} sats of platform fee on this seat are not sent yet. Run `maxplayer seller fees remit` to see the plan and retry.\n",
            attempt.outcome.as_str(),
            utc(attempt.started_at_unix),
            attempt.detail,
            snapshot.unsent_fee_sats
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_options_and_refuses_bad_ones() {
        assert_eq!(
            parse(&args(&[])).unwrap(),
            Options { home: None, limit: DEFAULT_LIMIT, json: false }
        );
        assert_eq!(
            parse(&args(&["--home", "/h", "--limit", "5", "--json"])).unwrap(),
            Options { home: Some(PathBuf::from("/h")), limit: 5, json: true }
        );
        assert!(parse(&args(&["--limit", "0"])).is_err());
        assert!(parse(&args(&["--limit"])).is_err());
        assert!(parse(&args(&["--bogus"])).unwrap_err().contains("unknown seller jobs option"));
    }

    #[test]
    fn utc_formats_known_instants() {
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00 UTC");
        assert_eq!(utc(1_795_807_271), "2026-11-27 19:21 UTC");
    }

    #[cfg(feature = "wallet")]
    mod wallet {
        use super::super::*;
        use maxplayer_core::seller_node::store::{
            JobPaymentRow, JobPaymentsSnapshot, JobReceipt, RemitAttempt, RemitAttemptOutcome,
            RemitAttemptTrigger,
        };

        const WEEK: i64 = 604_800;

        fn row(id: &str, state: &str, payment: &str, delivered: Option<i64>) -> JobPaymentRow {
            JobPaymentRow {
                job_id: id.to_owned(),
                buyer_pubkey: Some("b".repeat(64)),
                mint: None,
                state: state.to_owned(),
                created_at_unix: 0,
                offer_sats: Some(100),
                payment: payment.to_owned(),
                delivered_at_unix: delivered,
                pay_by_unix: delivered.map(|at| at + WEEK),
                settled_elsewhere_at_unix: None,
                receipt: None,
            }
        }

        /// A paid row as the store returns it: delivered, so `pay_by_unix` is set too.
        fn paid_row(id: &str, received_at: i64, fee: u64, remittance: Option<(&str, &str)>) -> JobPaymentRow {
            let mut paid = row(id, "paid", "sat", Some(received_at - 1));
            paid.mint = Some("https://mint.example".into());
            paid.receipt = Some(JobReceipt {
                received_at_unix: received_at,
                amount_sats: 90,
                fee_sats: fee,
                remittance_id: remittance.map(|(id, _)| id.to_owned()),
                remittance_state: remittance.map(|(_, state)| state.to_owned()),
            });
            paid
        }

        fn attempt(at: i64, outcome: RemitAttemptOutcome, remittance: Option<&str>, detail: &str) -> RemitAttempt {
            RemitAttempt {
                attempt_id: 1,
                started_at_unix: at,
                trigger: RemitAttemptTrigger::Retry,
                unremitted_sats: 9,
                outcome,
                detail: detail.to_owned(),
                remittance_id: remittance.map(str::to_owned),
            }
        }

        fn snapshot(
            rows: Vec<JobPaymentRow>,
            last_attempt: Option<RemitAttempt>,
            remittance_state: Option<&str>,
            unsent_fee_sats: u64,
        ) -> JobPaymentsSnapshot {
            JobPaymentsSnapshot {
                rows,
                last_attempt,
                last_attempt_remittance_state: remittance_state.map(str::to_owned),
                unsent_fee_sats,
            }
        }

        fn by<'a>(jobs: &'a [serde_json::Value], id: &str) -> &'a serde_json::Value {
            jobs.iter().find(|job| job["job_id"] == id).unwrap_or_else(|| panic!("{id}"))
        }

        #[test]
        fn pay_status_covers_every_case() {
            let now = 1_000;
            assert_eq!(pay_status(&row("a", "executing", "sat", None), now), PayStatus::InProgress);
            assert_eq!(pay_status(&row("b", "failed", "sat", None), now), PayStatus::Failed);
            assert_eq!(pay_status(&row("c", "delivered", "none", Some(900)), now), PayStatus::Free);
            assert_eq!(
                pay_status(&row("d", "delivered", "sat", Some(900)), now),
                PayStatus::AwaitingPayment { pay_by_unix: 900 + WEEK }
            );
            assert_eq!(
                pay_status(&row("e", "delivered", "sat", Some(900)), 900 + WEEK),
                PayStatus::AwaitingPayment { pay_by_unix: 900 + WEEK },
                "the window's last second is still payable, as for the buyer"
            );
            assert_eq!(pay_status(&row("e", "delivered", "sat", Some(900)), 900 + WEEK + 1), PayStatus::Unpaid);
            // #1451 r1 H1: the marker is written on a resumed job with NO local delivery.
            let mut elsewhere = row("f", "executing", "sat", None);
            elsewhere.settled_elsewhere_at_unix = Some(950);
            assert_eq!(pay_status(&elsewhere, now), PayStatus::SettledElsewhere);
            assert_eq!(pay_status(&paid_row("g", 950, 10, None), now), PayStatus::Paid);
        }

        #[test]
        fn fee_status_reads_the_remittance() {
            let receipt = |fee, id: Option<&str>, state: Option<&str>| JobReceipt {
                received_at_unix: 1,
                amount_sats: 100,
                fee_sats: fee,
                remittance_id: id.map(str::to_owned),
                remittance_state: state.map(str::to_owned),
            };
            assert_eq!(fee_status(&receipt(0, None, None)), FeeStatus::NoFee);
            assert_eq!(fee_status(&receipt(0, Some("r"), Some("settled"))), FeeStatus::NoFee);
            assert_eq!(fee_status(&receipt(10, None, None)), FeeStatus::Unremitted);
            assert_eq!(fee_status(&receipt(10, Some("r"), Some("planned"))), FeeStatus::InFlight);
            assert_eq!(fee_status(&receipt(10, Some("r"), Some("spending"))), FeeStatus::Spending);
            assert_eq!(fee_status(&receipt(10, Some("r"), Some("settled"))), FeeStatus::Remitted);
            assert_eq!(fee_status(&receipt(10, Some("r"), Some("failed"))), FeeStatus::Unremitted);
        }

        #[test]
        fn json_keys_amounts_pay_window_and_zero_fee() {
            let rows = vec![
                paid_row("bulk", 10, 9, Some(("b2786f75", "settled"))),
                paid_row("tiny", 10, 0, None),
                row("waiting", "delivered", "sat", Some(10)),
                row("free", "delivered", "none", Some(10)),
            ];
            let value = json_rows(&snapshot(rows, None, None, 0), 100);
            let jobs = value.as_array().expect("a flat array");
            let mut keys: Vec<&str> = jobs[0].as_object().unwrap().keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                [
                    "amount_sats", "buyer", "created_at_unix", "delivered_at_unix", "fee_error",
                    "fee_sats", "fee_sent", "fee_status", "job_id", "mint", "paid_at_unix",
                    "pay_by_unix", "status"
                ]
            );
            let bulk = by(jobs, "bulk");
            assert_eq!((bulk["status"].as_str(), bulk["fee_status"].as_str()), (Some("paid"), Some("remitted")));
            assert_eq!(bulk["fee_sent"], true);
            assert_eq!((bulk["amount_sats"].as_u64(), bulk["paid_at_unix"].as_i64()), (Some(90), Some(10)));
            assert_eq!(bulk["mint"], "https://mint.example");
            // r1 M2: no pay window on a paid or free job, even though the store computes one.
            assert_eq!(bulk["pay_by_unix"], serde_json::Value::Null);
            assert_eq!(by(jobs, "free")["pay_by_unix"], serde_json::Value::Null);
            assert_eq!(by(jobs, "waiting")["pay_by_unix"], 10 + WEEK);
            assert_eq!(by(jobs, "waiting")["fee_status"], serde_json::Value::Null);
            assert_eq!(by(jobs, "waiting")["fee_sent"], false);
            // r1 M1: a paid job with no fee owes nothing.
            let tiny = by(jobs, "tiny");
            assert_eq!((tiny["fee_status"].as_str(), tiny["fee_sent"].as_bool()), (Some("none"), Some(true)));
            assert_eq!(json_rows(&snapshot(vec![], None, None, 0), 0), serde_json::json!([]));
        }

        /// r1 H2: a melt that failed (or is held) leaves its receipts pinned to a `spending` remittance;
        /// the error belongs on THOSE jobs, not on a newer unremitted one the attempt never covered.
        #[test]
        fn a_failed_melt_error_lands_on_the_jobs_its_remittance_holds() {
            let rows = vec![
                paid_row("pinned", 10, 9, Some(("m1", "spending"))),
                paid_row("newer", 30, 9, None),
            ];
            let failed = attempt(20, RemitAttemptOutcome::Failed, Some("m1"), "melt failed");
            let value = json_rows(&snapshot(rows.clone(), Some(failed.clone()), Some("spending"), 18), 100);
            let jobs = value.as_array().unwrap();
            assert_eq!(by(jobs, "pinned")["fee_error"], "failed: melt failed");
            assert_eq!(by(jobs, "pinned")["fee_status"], "spending");
            assert_eq!(by(jobs, "newer")["fee_error"], serde_json::Value::Null);
            // A held refusal journaled after the newer payment still names m1: same answer.
            let held = attempt(40, RemitAttemptOutcome::Refused, Some("m1"), "HELD: remittance m1");
            let value = json_rows(&snapshot(rows.clone(), Some(held), Some("spending"), 18), 100);
            let jobs = value.as_array().unwrap();
            assert_eq!(by(jobs, "pinned")["fee_error"], "refused: HELD: remittance m1");
            assert_eq!(by(jobs, "newer")["fee_error"], serde_json::Value::Null);
            let text = render(&snapshot(rows, Some(failed), Some("spending"), 18), 100, "db");
            assert!(text.contains("platform fee 9 sats held: its payment awaits the mint's answer"), "{text}");
            assert!(text.contains("(the last remittance attempt for it did not pay; see below)"), "{text}");
            assert!(text.contains("18 sats of platform fee on this seat are not sent yet"), "{text}");
        }

        /// r1 H3: reconciliation settles the remittance a failed attempt named and journals nothing
        /// new; that failure is resolved — no `fee_error`, no footer.
        #[test]
        fn a_failure_whose_remittance_later_settled_is_not_reported() {
            let rows = vec![
                paid_row("pinned", 10, 9, Some(("m1", "settled"))),
                paid_row("below-minimum", 30, 2, None),
            ];
            let failed = attempt(20, RemitAttemptOutcome::Refused, Some("m1"), "HELD: remittance m1");
            let resolved = snapshot(rows.clone(), Some(failed.clone()), Some("settled"), 2);
            let jobs = json_rows(&resolved, 100);
            for job in jobs.as_array().unwrap() {
                assert_eq!(job["fee_error"], serde_json::Value::Null, "{job}");
            }
            assert!(!render(&resolved, 100, "db").contains("Seat-wide"));
            // Nothing unsent anywhere: an old pre-plan failure is moot too.
            let moot = snapshot(rows, Some(attempt(40, RemitAttemptOutcome::Failed, None, "quote")), None, 0);
            assert!(!render(&moot, 100, "db").contains("Seat-wide"));
        }

        /// A failure before any plan (no remittance) applies to the fees unremitted when it ran.
        #[test]
        fn a_pre_plan_failure_covers_the_fees_unremitted_when_it_ran() {
            let rows = vec![
                paid_row("owed", 10, 9, None),
                paid_row("same-second", 20, 9, None),
                paid_row("after", 30, 9, None),
                paid_row("tiny", 10, 0, None),
            ];
            let failed = attempt(20, RemitAttemptOutcome::Failed, None, "mint quote failed");
            let value = json_rows(&snapshot(rows, Some(failed), None, 27), 100);
            let jobs = value.as_array().unwrap();
            assert_eq!(by(jobs, "owed")["fee_error"], "failed: mint quote failed");
            assert_eq!(by(jobs, "same-second")["fee_error"], "failed: mint quote failed");
            assert_eq!(by(jobs, "after")["fee_error"], serde_json::Value::Null);
            assert_eq!(by(jobs, "tiny")["fee_error"], serde_json::Value::Null);
        }

        #[test]
        fn render_names_unpaid_jobs_amounts_and_an_empty_list() {
            let mut gone_offer = paid_row("gone-offer", 5, 9, None);
            gone_offer.offer_sats = None;
            let rows = vec![
                row("owed", "delivered", "sat", Some(0)),
                row("late", "delivered", "sat", Some(-700_000)),
                gone_offer,
            ];
            let failed = attempt(0, RemitAttemptOutcome::Failed, None, "mint quote failed");
            let text = render(&snapshot(rows, Some(failed.clone()), None, 9), 10, "/h/seller.sqlite");
            assert!(text.contains("Seller jobs — /h/seller.sqlite"), "{text}");
            assert!(text.contains("AWAITING PAYMENT (the buyer can pay until 1970-01-08 00:00 UTC)"), "{text}");
            assert!(text.contains("UNPAID (the buyer's 7-day pay window has closed)"), "{text}");
            assert!(text.contains("1 delivered job(s) awaiting payment, 1 unpaid after the pay window."), "{text}");
            // r1 L2: the paid amount, not "amount unknown", when the offer row is gone.
            assert!(text.contains("job gone-offer  1970-01-01 00:00 UTC  90 sats"), "{text}");
            assert!(text.contains("Seat-wide: the latest platform-fee remittance attempt failed at 1970-01-01 00:00 UTC: mint quote failed"), "{text}");
            // r1 H3: an empty list prints no seat-wide footer.
            assert_eq!(
                render(&snapshot(vec![], Some(failed), None, 9), 0, "db"),
                "Seller jobs — db\nNo jobs won yet.\n"
            );
        }

        fn temp_home(label: &str) -> std::path::PathBuf {
            let root = std::env::temp_dir().join(format!("mx-seller-jobs-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            root
        }

        fn call(root: &std::path::Path, extra: &[&str]) -> (i32, String, String) {
            let mut args = vec!["--home".to_owned(), root.display().to_string()];
            args.extend(extra.iter().map(|s| (*s).to_owned()));
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = run(&args, &mut out, &mut err);
            (code, String::from_utf8_lossy(&out).into_owned(), String::from_utf8_lossy(&err).into_owned())
        }

        #[test]
        fn a_missing_store_is_an_error_and_an_empty_one_an_empty_list() {
            let root = temp_home("empty");
            let (code, out, err) = call(&root, &["--json"]);
            assert_eq!((code, out.as_str()), (RUNTIME_ERROR, ""), "{err}");
            assert!(err.contains("no seller store"), "{err}");
            maxplayer_core::seller_node::store::SellerStore::open(
                &root.join(maxplayer_core::seller_node::STATE_DB_FILE),
            )
            .expect("create an empty store");
            let (code, out, _) = call(&root, &["--json"]);
            assert_eq!((code, out.trim()), (SUCCESS, "[]"));
            let (code, out, _) = call(&root, &[]);
            assert_eq!(code, SUCCESS);
            assert!(out.contains("No jobs won yet."), "{out}");
            let _ = std::fs::remove_dir_all(&root);
        }

        /// End to end on a real store: the relay-settled marker on an undelivered job (r1 H1), a
        /// failed receive with no payment (r1 M3), and a failed melt pinned to a spending remittance
        /// (r1 H2), read through `run --json`.
        #[test]
        fn run_reads_a_real_store() {
            use maxplayer_core::seller_node::store::SellerStore;
            let root = temp_home("real");
            let db = root.join(maxplayer_core::seller_node::STATE_DB_FILE);
            let store = SellerStore::open(&db).expect("create");
            {
                let conn = rusqlite::Connection::open(&db).expect("seed connection");
                conn.execute_batch(
                    "INSERT INTO offers (offer_id, buyer_pubkey, amount_sats, unit, task, deadline_unix, targeted, created_at_unix, payment)
                         VALUES ('gone', 'b', 100, 'sat', 't', 9, 1, 1, 'sat'),
                                ('tried', 'b', 100, 'sat', 't', 9, 1, 1, 'sat'),
                                ('held', 'b', 100, 'sat', 't', 9, 1, 1, 'sat');
                     INSERT INTO jobs (job_id, offer_id, state, created_at_unix, updated_at_unix)
                         VALUES ('gone', 'gone', 'executing', 30, 30),
                                ('tried', 'tried', 'delivered', 20, 20),
                                ('held', 'held', 'paid', 10, 10);
                     INSERT INTO deliveries (job_id, result_ref, delivered_at_unix, payment)
                         VALUES ('tried', 'r', 21, 'sat'), ('held', 'r', 11, 'sat');
                     INSERT INTO pending_receive (job_id, token_hash, buyer_pubkey, mint, amount_sats, created_at_unix)
                         VALUES ('tried', 't1', 'b', 'https://refused.example', 100, 22),
                                ('held', 't2', 'b', 'https://mint.example', 100, 12);
                     INSERT INTO fee_remittances (remittance_id, gross_sats, net_sats, destination, payment_hash, bolt11, state, created_at_unix, spending_since_unix)
                         VALUES ('m1', 10, 9, 'd', 'h', 'bolt', 'planned', 13, 14);
                     INSERT INTO receipts (receipt_id, job_id, amount_sats, received_at_unix, fee_bps, fee_sats, remittance_id)
                         VALUES ('rc', 'held', 100, 12, 1000, 10, 'm1');
                     INSERT INTO fee_remit_attempts (started_at_unix, trigger, unremitted_sats, outcome, detail, remittance_id)
                         VALUES (14, 'collect', 10, 'failed', 'melt failed', 'm1');",
                )
                .expect("seed");
            }
            store.mark_settled_elsewhere("gone", 40).expect("mark");
            drop(store);
            let (code, out, err) = call(&root, &["--json"]);
            assert_eq!(code, SUCCESS, "{err}");
            let value: serde_json::Value = serde_json::from_str(&out).expect("json");
            let jobs = value.as_array().unwrap();
            assert_eq!(by(jobs, "gone")["status"], "settled_elsewhere");
            let tried = by(jobs, "tried");
            assert_eq!(tried["status"], "unpaid");
            assert_eq!(tried["mint"], serde_json::Value::Null, "a refused receive is no payment");
            let held = by(jobs, "held");
            assert_eq!((held["fee_status"].as_str(), held["fee_sent"].as_bool()), (Some("spending"), Some(false)));
            assert_eq!(held["fee_error"], "failed: melt failed");
            assert_eq!(held["mint"], "https://mint.example");
            let _ = std::fs::remove_dir_all(&root);
        }

        /// r1 M6: the command opens the store read-only — a store an older binary left behind is
        /// refused with a hint and NOT migrated.
        #[test]
        fn an_older_store_is_refused_and_left_unmigrated() {
            let root = temp_home("older");
            let db = root.join(maxplayer_core::seller_node::STATE_DB_FILE);
            drop(maxplayer_core::seller_node::store::SellerStore::open(&db).expect("create"));
            let conn = rusqlite::Connection::open(&db).expect("open");
            conn.execute("UPDATE seller_meta SET value = '1' WHERE key = 'schema_version'", [])
                .expect("age the store");
            let (code, out, err) = call(&root, &["--json"]);
            assert_eq!((code, out.as_str()), (RUNTIME_ERROR, ""));
            assert!(err.contains("older than this binary"), "{err}");
            let version: String = conn
                .query_row("SELECT value FROM seller_meta WHERE key = 'schema_version'", [], |row| row.get(0))
                .expect("read version");
            assert_eq!(version, "1", "a read-only open migrates nothing");
            let _ = std::fs::remove_dir_all(&root);
        }
    }
}
