//! Private SQLite journal, using the pinned CDK KV store. No record pruning.
use anyhow::Result;
use cdk::cdk_database::WalletDatabase;
use serde::{Serialize, de::DeserializeOwned};
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path, sync::Arc};
pub struct Journal(pub Arc<cdk_sqlite::WalletSqliteDatabase>);
impl Journal {
    pub async fn open(home: &Path) -> Result<Self> {
        let p = home.join("trade.sqlite");
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&p)?;
        Ok(Self(Arc::new(
            cdk_sqlite::WalletSqliteDatabase::new(p).await?,
        )))
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
            if let Some(v) = self.get(ns, &k).await? {
                out.push(v)
            }
        }
        Ok(out)
    }
}
