//! Private SQLite journal, using the pinned CDK KV store. No record pruning.
use anyhow::Result;
use cdk::cdk_database::WalletDatabase;
use serde::{Serialize, de::DeserializeOwned};
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path, sync::Arc};
pub struct Journal(pub Arc<cdk_sqlite::WalletSqliteDatabase>);
/// The journal format this build writes and understands, stored once as `meta/journal`. A journal
/// stamped with a higher version was written by a newer `maxplayer`: refused, never guessed at.
pub const JOURNAL_VERSION: u64 = 1;
/// Key of a deliberately malformed row planted in every money namespace ([`TOMBSTONE_NAMESPACES`]).
/// The old standalone `maxplayer-trade` binary lists a namespace into its record struct before it
/// acts on it (startup recovery replays `inbox_retry`, then reads `swap`; `fund` reads
/// `funding`/`receive`; `withdraw` reads `withdrawal`; `receive` reads `receive`/`funding`; `send`
/// reads `send`), so this row stops it with its own text in the error ("invalid type: string
/// \"maxplayer-trade is obsolete ...\""). Not covered: `fund --id <id>` of an already journaled
/// funding and `send --reclaim <id>`, which read one record directly. Every reader in this crate
/// skips the row ([`Journal::all`], `observe::journal_rows`, `wallet::read_status`,
/// `send::check_redeemed`).
pub const OBSOLETE_TOMBSTONE_KEY: &str = "obsolete-maxplayer-trade-tombstone";
/// The namespaces that carry the tombstone: every one an old binary lists before moving money.
pub const TOMBSTONE_NAMESPACES: [&str; 6] = [
    "inbox_retry",
    "swap",
    "funding",
    "withdrawal",
    "receive",
    "send",
];
/// The tombstone's value: a JSON string, never a valid record.
pub const OBSOLETE_TOMBSTONE: &str = "maxplayer-trade is obsolete: trading moved into `maxplayer \
    trade`. Delete this binary (`command -v maxplayer-trade` shows where it is) and run `maxplayer \
    trade ...`; nothing was changed";
impl Journal {
    pub async fn open(home: &Path) -> Result<Self> {
        let p = crate::wallet::ensure_state_dir(home)?.join("trade.sqlite");
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&p)?;
        let j = Self(Arc::new(cdk_sqlite::WalletSqliteDatabase::new(p).await?));
        j.stamp().await?;
        Ok(j)
    }
    /// Check the journal version and plant the old-binary tombstone. Each row is written only when
    /// absent, so opening a stamped journal writes nothing.
    async fn stamp(&self) -> Result<()> {
        match self.0.kv_read("trade-v1", "meta", "journal").await? {
            Some(v) => {
                let found = serde_json::from_slice::<serde_json::Value>(&v)
                    .ok()
                    .and_then(|v| v["version"].as_u64())
                    .filter(|&n| n >= 1)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "this trade journal has a corrupt version row (meta/journal); nothing \
                             was changed"
                        )
                    })?;
                anyhow::ensure!(
                    found <= JOURNAL_VERSION,
                    "this trade journal is version {found}, written by a newer maxplayer (this one \
                     reads up to {JOURNAL_VERSION}); upgrade maxplayer, nothing was changed"
                );
            }
            None => {
                self.0
                    .kv_write(
                        "trade-v1",
                        "meta",
                        "journal",
                        &serde_json::to_vec(&serde_json::json!({ "version": JOURNAL_VERSION }))?,
                    )
                    .await?
            }
        }
        for ns in TOMBSTONE_NAMESPACES {
            if self
                .0
                .kv_read("trade-v1", ns, OBSOLETE_TOMBSTONE_KEY)
                .await?
                .is_none()
            {
                self.0
                    .kv_write(
                        "trade-v1",
                        ns,
                        OBSOLETE_TOMBSTONE_KEY,
                        &serde_json::to_vec(OBSOLETE_TOMBSTONE)?,
                    )
                    .await?;
            }
        }
        Ok(())
    }
    pub async fn get<T: DeserializeOwned>(&self, ns: &str, id: &str) -> Result<Option<T>> {
        self.0
            .kv_read("trade-v1", ns, id)
            .await?
            .map(|v| Ok(serde_json::from_slice(&v)?))
            .transpose()
    }
    pub async fn put<T: Serialize>(&self, ns: &str, id: &str, v: &T) -> Result<()> {
        self.0
            .kv_write("trade-v1", ns, id, &serde_json::to_vec(v)?)
            .await?;
        Ok(())
    }
    pub async fn remove(&self, ns: &str, id: &str) -> Result<()> {
        self.0.kv_remove("trade-v1", ns, id).await?;
        Ok(())
    }
    pub async fn all<T: DeserializeOwned>(&self, ns: &str) -> Result<Vec<T>> {
        let mut out = Vec::new();
        for k in self.0.kv_list("trade-v1", ns).await? {
            if k == OBSOLETE_TOMBSTONE_KEY {
                continue;
            }
            if let Some(v) = self.get(ns, &k).await? {
                out.push(v)
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the old standalone binary does on startup: read every swap row as a `Swap`. It must
    /// stop on the tombstone with the delete-it text, while this crate's readers never see it.
    #[tokio::test]
    async fn old_binary_stops_on_the_tombstone_and_current_readers_skip_it() {
        let home = tempfile::tempdir().unwrap();
        let j = Journal::open(home.path()).await.unwrap();
        let mut old_view = Vec::new();
        for k in j.0.kv_list("trade-v1", "swap").await.unwrap() {
            let v = j.0.kv_read("trade-v1", "swap", &k).await.unwrap().unwrap();
            old_view.push(serde_json::from_slice::<crate::coordinator::Swap>(&v));
        }
        let err = old_view
            .into_iter()
            .find_map(Result::err)
            .expect("an old binary must fail to read the swap journal");
        assert!(
            err.to_string().contains("maxplayer-trade is obsolete"),
            "{err}"
        );
        // fund, withdraw, receive and send list their own namespace before moving money.
        async fn old_list<T: DeserializeOwned>(j: &Journal, ns: &str) -> String {
            for k in j.0.kv_list("trade-v1", ns).await.unwrap() {
                let v = j.0.kv_read("trade-v1", ns, &k).await.unwrap().unwrap();
                if let Err(e) = serde_json::from_slice::<T>(&v) {
                    return e.to_string();
                }
            }
            panic!("an old binary must fail to read the {ns} journal")
        }
        for err in [
            old_list::<crate::coordinator::Retained>(&j, "inbox_retry").await,
            old_list::<crate::money::Funding>(&j, "funding").await,
            old_list::<crate::money::Withdrawal>(&j, "withdrawal").await,
            old_list::<crate::receive::Receipt>(&j, "receive").await,
            old_list::<crate::send::SendAttempt>(&j, "send").await,
        ] {
            assert!(err.contains("maxplayer-trade is obsolete"), "{err}");
        }
        for ns in TOMBSTONE_NAMESPACES {
            assert!(
                crate::observe::journal_rows(home.path(), ns)
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(
            j.all::<crate::money::Funding>("funding")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            j.all::<crate::money::Withdrawal>("withdrawal")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            j.all::<crate::receive::Receipt>("receive")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            j.all::<crate::send::SendAttempt>("send")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            j.all::<crate::coordinator::Swap>("swap")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            crate::observe::journal_rows(home.path(), "swap")
                .unwrap()
                .is_empty()
        );
        let status = crate::wallet::read_status(home.path()).unwrap();
        assert_eq!(status["records"], serde_json::json!([]), "{status}");
    }

    /// Re-opening a stamped journal writes nothing; a journal from a newer format is refused.
    #[tokio::test]
    async fn reopen_writes_nothing_and_a_newer_journal_is_refused() {
        let home = tempfile::tempdir().unwrap();
        drop(Journal::open(home.path()).await.unwrap());
        let db = crate::wallet::state_dir(home.path()).join("trade.sqlite");
        let dump = || {
            let c = rusqlite::Connection::open(&db).unwrap();
            let mut s = c
                .prepare("SELECT secondary_namespace, key, value, updated_time FROM kv_store ORDER BY 1, 2")
                .unwrap();
            s.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>()
        };
        let before = dump();
        assert_eq!(
            before.len(),
            1 + TOMBSTONE_NAMESPACES.len(),
            "meta/journal + one tombstone per money namespace: {before:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let j = Journal::open(home.path()).await.unwrap();
        assert_eq!(
            before,
            dump(),
            "a second open must not rewrite the stamp rows"
        );
        j.0.kv_write("trade-v1", "meta", "journal", br#"{"version":2}"#)
            .await
            .unwrap();
        drop(j);
        let err = Journal::open(home.path())
            .await
            .err()
            .expect("newer journal refused");
        assert!(
            err.to_string().contains("written by a newer maxplayer"),
            "{err}"
        );
    }
}
