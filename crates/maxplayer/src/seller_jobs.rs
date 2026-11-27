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
        "Usage:\n  maxplayer seller jobs [--home <dir>] [--limit <n>] [--json]\n\nLists the jobs this seat won, newest first (default {DEFAULT_LIMIT}), each with its state and\nwhether it was paid: free, paid (with the platform fee's remittance), delivered and awaiting\npayment (the buyer can pay for 7 days after delivery), or unpaid once that window closed. When the\nlatest platform-fee remittance attempt did not pay, it prints that attempt's error too; see\n`maxplayer seller fees remit` to retry it. Moves no sats (it only opens, reads and prints the\nseller store). --json prints a flat array, one object per job: job_id, buyer, amount_sats, mint,\nstatus (in_progress | failed | free | paid | awaiting_payment | settled_elsewhere | unpaid),\npay_by_unix, fee_sats, fee_sent, fee_error. A seat that never ran `seller serve` has no store:\nthat is an error (exit 2), not an empty list. Exit 0 = printed; 1 = usage; 2 = error."
    );
}

#[cfg(feature = "wallet")]
fn print_jobs(options: &Options, out: &mut dyn Write) -> Result<(), String> {
    use maxplayer_core::seller_node::store::RemitAttemptOutcome;

    let (store, _root, db) = crate::seller_fees::open_store(&options.home)?;
    let rows = store
        .recent_job_payments(options.limit)
        .map_err(|error| format!("read jobs: {error}"))?;
    let last_attempt = store
        .recent_remit_attempts(1)
        .map_err(|error| format!("read remittance attempts: {error}"))?
        .into_iter()
        .next()
        .filter(|attempt| attempt.outcome != RemitAttemptOutcome::Paid);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    if options.json {
        let _ = writeln!(out, "{}", json_rows(&rows, last_attempt.as_ref(), now));
    } else {
        let _ = write!(
            out,
            "{}",
            render(&rows, last_attempt.as_ref(), now, &db.display().to_string())
        );
    }
    Ok(())
}

/// `--json`: a flat array, one object per job, with these keys (stable):
/// `job_id`, `buyer`, `amount_sats` (what was paid, else the offer amount), `mint` (the paying
/// mint, `null` before a payment), `status` (one of [`PayStatus::key`]), `pay_by_unix`, `fee_sats`,
/// `fee_sent` (the platform fee reached the platform) and `fee_error`. `fee_error` is the seat's
/// latest failed remittance attempt, set only on a paid job whose fee is still unremitted and that
/// the attempt covered (it started after the payment): remittance is seat-wide, never per job.
#[cfg(feature = "wallet")]
fn json_rows(
    rows: &[maxplayer_core::seller_node::store::JobPaymentRow],
    last_failed_attempt: Option<&maxplayer_core::seller_node::store::RemitAttempt>,
    now: i64,
) -> serde_json::Value {
    serde_json::Value::Array(
        rows.iter()
            .map(|row| {
                let fee = row.receipt.as_ref().map(fee_status);
                let fee_error = match (row.receipt.as_ref(), fee, last_failed_attempt) {
                    (Some(receipt), Some(FeeStatus::Unremitted), Some(attempt))
                        if attempt.started_at_unix >= receipt.received_at_unix =>
                    {
                        Some(format!("{}: {}", attempt.outcome.as_str(), attempt.detail))
                    }
                    _ => None,
                };
                serde_json::json!({
                    "job_id": row.job_id,
                    "buyer": row.buyer_pubkey,
                    "amount_sats": row.receipt.as_ref().map(|r| r.amount_sats).or(row.offer_sats),
                    "mint": row.mint,
                    "status": pay_status(row, now).key(),
                    "pay_by_unix": row.pay_by_unix,
                    "fee_sats": row.receipt.as_ref().map(|r| r.fee_sats),
                    "fee_sent": fee == Some(FeeStatus::Remitted),
                    "fee_error": fee_error,
                })
            })
            .collect(),
    )
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

#[cfg(feature = "wallet")]
fn pay_status(row: &maxplayer_core::seller_node::store::JobPaymentRow, now: i64) -> PayStatus {
    if row.receipt.is_some() {
        return PayStatus::Paid;
    }
    match (row.state.as_str(), row.delivered_at_unix) {
        ("failed", _) => PayStatus::Failed,
        (_, None) => PayStatus::InProgress,
        (_, Some(_)) if row.payment == "none" => PayStatus::Free,
        (_, Some(_)) if row.settled_elsewhere_at_unix.is_some() => PayStatus::SettledElsewhere,
        (_, Some(_)) => match row.pay_by_unix {
            Some(pay_by_unix) if now < pay_by_unix => PayStatus::AwaitingPayment { pay_by_unix },
            _ => PayStatus::Unpaid,
        },
    }
}

/// Where a paid job's platform fee stands.
#[cfg(feature = "wallet")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeeStatus {
    None,
    Remitted,
    InFlight,
    Unremitted,
}

#[cfg(feature = "wallet")]
impl FeeStatus {
    fn key(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Remitted => "remitted",
            Self::InFlight => "in_flight",
            Self::Unremitted => "unremitted",
        }
    }
}

#[cfg(feature = "wallet")]
fn fee_status(receipt: &maxplayer_core::seller_node::store::JobReceipt) -> FeeStatus {
    if receipt.fee_sats == 0 {
        return FeeStatus::None;
    }
    match (receipt.remittance_id.as_ref(), receipt.remittance_state.as_deref()) {
        (Some(_), Some("settled")) => FeeStatus::Remitted,
        (Some(_), Some("planned")) => FeeStatus::InFlight,
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
    rows: &[maxplayer_core::seller_node::store::JobPaymentRow],
    last_failed_attempt: Option<&maxplayer_core::seller_node::store::RemitAttempt>,
    now: i64,
    db: &str,
) -> String {
    let mut text = format!("Seller jobs — {db}\n");
    if rows.is_empty() {
        text.push_str("No jobs won yet.\n");
    }
    let mut awaiting = 0usize;
    let mut unpaid = 0usize;
    for row in rows {
        let amount = match row.offer_sats {
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
                    FeeStatus::None => "no platform fee".to_owned(),
                    FeeStatus::Remitted => format!("platform fee {} sats remitted", receipt.fee_sats),
                    FeeStatus::InFlight => {
                        format!("platform fee {} sats being remitted", receipt.fee_sats)
                    }
                    FeeStatus::Unremitted => format!(
                        "platform fee {} sats unremitted (the node remits it automatically)",
                        receipt.fee_sats
                    ),
                };
                format!(
                    "PAID {} sats at {} — {fee}",
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
                "delivered — settled per the relay; no payment collected by this seat".to_owned()
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
    if let Some(attempt) = last_failed_attempt {
        text.push_str(&format!(
            "Seat-wide: the latest platform-fee remittance attempt {} at {}: {}\n  It covers every unremitted fee above. Run `maxplayer seller fees remit` to see the plan and retry.\n",
            attempt.outcome.as_str(),
            utc(attempt.started_at_unix),
            attempt.detail
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
            JobPaymentRow, JobReceipt, RemitAttempt, RemitAttemptOutcome, RemitAttemptTrigger,
        };

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
                pay_by_unix: delivered.map(|at| at + 604_800),
                settled_elsewhere_at_unix: None,
                receipt: None,
            }
        }

        #[test]
        fn pay_status_covers_every_case() {
            let now = 1_000;
            assert_eq!(pay_status(&row("a", "executing", "sat", None), now), PayStatus::InProgress);
            assert_eq!(pay_status(&row("b", "failed", "sat", None), now), PayStatus::Failed);
            assert_eq!(pay_status(&row("c", "delivered", "none", Some(900)), now), PayStatus::Free);
            assert_eq!(
                pay_status(&row("d", "delivered", "sat", Some(900)), now),
                PayStatus::AwaitingPayment { pay_by_unix: 900 + 604_800 }
            );
            assert_eq!(
                pay_status(&row("e", "delivered", "sat", Some(900)), 900 + 604_800),
                PayStatus::Unpaid,
                "at the window's end the job is unpaid"
            );
            let mut elsewhere = row("f", "delivered", "sat", Some(900));
            elsewhere.settled_elsewhere_at_unix = Some(950);
            assert_eq!(pay_status(&elsewhere, now), PayStatus::SettledElsewhere);
            let mut paid = row("g", "paid", "sat", Some(900));
            paid.receipt = Some(JobReceipt {
                received_at_unix: 950,
                amount_sats: 100,
                fee_sats: 10,
                remittance_id: None,
                remittance_state: None,
            });
            assert_eq!(pay_status(&paid, now), PayStatus::Paid);
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
            assert_eq!(fee_status(&receipt(0, None, None)), FeeStatus::None);
            assert_eq!(fee_status(&receipt(10, None, None)), FeeStatus::Unremitted);
            assert_eq!(fee_status(&receipt(10, Some("r"), Some("planned"))), FeeStatus::InFlight);
            assert_eq!(fee_status(&receipt(10, Some("r"), Some("settled"))), FeeStatus::Remitted);
            assert_eq!(fee_status(&receipt(10, Some("r"), Some("failed"))), FeeStatus::Unremitted);
        }

        #[test]
        fn render_names_unpaid_jobs_and_a_failed_remittance() {
            let rows = vec![
                row("owed", "delivered", "sat", Some(0)),
                row("late", "delivered", "sat", Some(-700_000)),
            ];
            let attempt = RemitAttempt {
                attempt_id: 1,
                started_at_unix: 0,
                trigger: RemitAttemptTrigger::Retry,
                unremitted_sats: 10,
                outcome: RemitAttemptOutcome::Failed,
                detail: "mint quote failed".to_owned(),
                remittance_id: None,
            };
            let text = render(&rows, Some(&attempt), 10, "/h/seller.sqlite");
            assert!(text.contains("Seller jobs — /h/seller.sqlite"), "{text}");
            assert!(text.contains("job owed"), "{text}");
            assert!(text.contains("AWAITING PAYMENT (the buyer can pay until 1970-01-08 00:00 UTC)"), "{text}");
            assert!(text.contains("UNPAID (the buyer's 7-day pay window has closed)"), "{text}");
            assert!(text.contains("1 delivered job(s) awaiting payment, 1 unpaid after the pay window."), "{text}");
            assert!(text.contains("Seat-wide: the latest platform-fee remittance attempt failed at 1970-01-01 00:00 UTC: mint quote failed"), "{text}");
            assert!(text.contains("maxplayer seller fees remit"), "{text}");
            assert_eq!(render(&[], None, 0, "db"), "Seller jobs — db\nNo jobs won yet.\n");
        }

        fn paid_row(id: &str, received_at: i64, remittance: Option<(&str, &str)>) -> JobPaymentRow {
            let mut paid = row(id, "paid", "sat", Some(received_at));
            paid.mint = Some("https://mint.example".into());
            paid.receipt = Some(JobReceipt {
                received_at_unix: received_at,
                amount_sats: 90,
                fee_sats: 9,
                remittance_id: remittance.map(|(id, _)| id.to_owned()),
                remittance_state: remittance.map(|(_, state)| state.to_owned()),
            });
            paid
        }

        #[test]
        fn json_is_a_flat_array_and_pins_the_seat_wide_error_only_where_it_applies() {
            let rows = vec![
                paid_row("bulk", 10, Some(("b2786f75", "settled"))),
                paid_row("owed-fee", 10, None),
                paid_row("after-attempt", 30, None),
                row("waiting", "delivered", "sat", Some(10)),
            ];
            let attempt = RemitAttempt {
                attempt_id: 7,
                started_at_unix: 20,
                trigger: RemitAttemptTrigger::Retry,
                unremitted_sats: 18,
                outcome: RemitAttemptOutcome::Failed,
                detail: "melt failed".into(),
                remittance_id: None,
            };
            let value = json_rows(&rows, Some(&attempt), 100);
            let jobs = value.as_array().expect("a flat array");
            assert_eq!(jobs.len(), 4);
            let mut keys: Vec<&str> = jobs[0].as_object().unwrap().keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                ["amount_sats", "buyer", "fee_error", "fee_sats", "fee_sent", "job_id", "mint", "pay_by_unix", "status"]
            );
            // A bulk remittance that discharged this receipt: sent, no error.
            assert_eq!(jobs[0]["status"], "paid");
            assert_eq!(jobs[0]["fee_sent"], true);
            assert_eq!(jobs[0]["fee_error"], serde_json::Value::Null);
            assert_eq!(jobs[0]["amount_sats"], 90);
            assert_eq!(jobs[0]["mint"], "https://mint.example");
            // Unremitted, and the failed attempt came after the payment: it carries the error.
            assert_eq!(jobs[1]["fee_sent"], false);
            assert_eq!(jobs[1]["fee_error"], "failed: melt failed");
            // Paid after the failed attempt: that attempt never covered it.
            assert_eq!(jobs[2]["fee_error"], serde_json::Value::Null);
            // Not paid yet: no fee, no error.
            assert_eq!(jobs[3]["status"], "awaiting_payment");
            assert_eq!(jobs[3]["fee_sent"], false);
            assert_eq!(jobs[3]["fee_error"], serde_json::Value::Null);
            assert_eq!(jobs[3]["amount_sats"], 100);
            assert_eq!(json_rows(&[], None, 0), serde_json::json!([]));
        }

        #[test]
        fn a_missing_store_is_an_error_and_an_empty_one_an_empty_list() {
            let root = std::env::temp_dir().join(format!("mx-seller-jobs-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let call = |extra: &[&str]| {
                let mut args = vec!["--home".to_owned(), root.display().to_string()];
                args.extend(extra.iter().map(|s| (*s).to_owned()));
                let (mut out, mut err) = (Vec::new(), Vec::new());
                let code = run(&args, &mut out, &mut err);
                (code, String::from_utf8_lossy(&out).into_owned(), String::from_utf8_lossy(&err).into_owned())
            };
            let (code, out, err) = call(&["--json"]);
            assert_eq!((code, out.as_str()), (RUNTIME_ERROR, ""), "{err}");
            assert!(err.contains("no seller store"), "{err}");
            maxplayer_core::seller_node::store::SellerStore::open(
                &root.join(maxplayer_core::seller_node::STATE_DB_FILE),
            )
            .expect("create an empty store");
            let (code, out, _) = call(&["--json"]);
            assert_eq!((code, out.trim()), (SUCCESS, "[]"));
            let (code, out, _) = call(&[]);
            assert_eq!(code, SUCCESS);
            assert!(out.contains("No jobs won yet."), "{out}");
            let _ = std::fs::remove_dir_all(&root);
        }
    }
}
