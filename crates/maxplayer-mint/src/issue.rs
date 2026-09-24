//! `maxplayer-mint issue <amount>`: mint credits in-process into a local token file (spec §4).
//!
//! No quote, no payment processor, no network surface, and nothing advertised in NUT-04: the
//! operator's own command is the only way in. The mint signs through cdk's own signatory and the
//! signatures are stored through cdk's own database API, so restore and `total_issued` see them
//! exactly as they see swap outputs.
//!
//! Journal (the mint's sqlite, cdk KV namespace `maxplayer_mint/issues`):
//! 1. `pending`: keyset + the output secrets, written BEFORE anything is signed.
//! 2. `committed`: the signatures, written in ONE transaction with cdk's signature rows, so an
//!    issue is either fully recorded or not at all.
//! 3. `written`: the token file exists; the secrets are dropped from the journal.
//!
//! Every `issue` run first finishes earlier unfinished issues ([`reconcile`]): `pending` with none
//! of its outputs signed is signed now (the SAME outputs), `committed` rewrites its token file.
//! An interrupted issue is completed with its original outputs, never re-issued blind.
//!
//! The mint used here is never `start`ed: cdk's startup saga recovery belongs to `run` alone, and
//! must not roll back a swap a live listener has in flight on the same database.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow, bail};
use cdk::Amount;
use cdk::Mint;
use cdk::amount::SplitTarget;
use cdk::dhke::construct_proofs;
use cdk::mint_url::MintUrl;
use cdk::nuts::{
    BlindSignature, BlindedMessage, CurrencyUnit, Id, PreMintSecrets, SecretKey, Token,
};
use cdk::secret::Secret;
use serde::{Deserialize, Serialize};

use crate::backend;
use crate::replay::{self, Signed};

const NAMESPACE: &str = "maxplayer_mint";
const ISSUES: &str = "issues";

/// One output of an issue: what the holder needs to unblind its signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Output {
    pub amount: Amount,
    pub secret: Secret,
    pub r: SecretKey,
    pub blinded: BlindedMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Entry {
    Pending {
        keyset: Id,
        outputs: Vec<Output>,
    },
    Committed {
        keyset: Id,
        outputs: Vec<Output>,
        signatures: Vec<BlindSignature>,
    },
    Written {
        amount: u64,
        file: String,
    },
}

/// A finished issue.
#[derive(Debug, Clone, PartialEq)]
pub struct Issued {
    pub id: String,
    pub amount: u64,
    pub file: PathBuf,
}

/// Issue `amount` sat: [`begin`], then finish it.
pub async fn issue(mint: &Mint, dir: &Path, url: &str, amount: u64) -> Result<Issued> {
    let id = begin(mint, amount).await?;
    finish(mint, dir, url, &id).await
}

/// Journal a new issue as `pending` (nothing signed yet) and return its id.
pub async fn begin(mint: &Mint, amount: u64) -> Result<String> {
    if amount == 0 {
        bail!("amount must be at least 1 sat");
    }
    let keyset = backend::active_keyset(mint)?;
    let pre = PreMintSecrets::random(
        keyset,
        Amount::from(amount),
        &SplitTarget::default(),
        &backend::fee_and_amounts(),
    )
    .context("split amount")?;
    let outputs = pre
        .secrets
        .iter()
        .map(|p| Output {
            amount: p.amount,
            secret: p.secret.clone(),
            r: p.r.clone(),
            blinded: p.blinded_message.clone(),
        })
        .collect();
    let mut raw = [0u8; 16];
    getrandom::fill(&mut raw).map_err(|error| anyhow!("entropy: {error}"))?;
    let id: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    write(mint, &id, &Entry::Pending { keyset, outputs }).await?;
    Ok(id)
}

/// `pending` → `committed`: sign the journaled outputs (unless the mint already has) and store
/// the signatures and the journal entry in one transaction.
pub async fn sign(mint: &Mint, id: &str) -> Result<()> {
    let Some(Entry::Pending { keyset, outputs }) = read(mint, id).await? else {
        return Ok(());
    };
    let blinded: Vec<BlindedMessage> = outputs.iter().map(|o| o.blinded.clone()).collect();
    let signed = replay::signed_outputs(mint, &blinded)
        .await
        .map_err(|error| anyhow!(error))?;
    let fresh = match signed {
        Signed::Partial => bail!("ALARM: issue {id} is partially signed; not completed"),
        Signed::All(signatures) => (signatures, false),
        Signed::None => (mint.blind_sign(blinded.clone()).await?, true),
    };
    let (signatures, new) = fresh;
    let entry = Entry::Committed {
        keyset,
        outputs,
        signatures: signatures.clone(),
    };
    let value = serde_json::to_vec(&entry)?;
    let mut tx = mint.localstore().begin_transaction().await?;
    if new {
        let secrets: Vec<_> = blinded.iter().map(|b| b.blinded_secret).collect();
        tx.add_blind_signatures(&secrets, &signatures, None).await?;
    }
    tx.kv_write(NAMESPACE, ISSUES, id, &value).await?;
    tx.commit().await?;
    Ok(())
}

/// Finish every unfinished issue, oldest state first. Returns the ones finished now.
pub async fn reconcile(mint: &Mint, dir: &Path, url: &str) -> Result<Vec<Issued>> {
    let mut done = Vec::new();
    for id in mint.localstore().kv_list(NAMESPACE, ISSUES).await? {
        if !matches!(read(mint, &id).await?, Some(Entry::Written { .. })) {
            done.push(finish(mint, dir, url, &id).await?);
        }
    }
    Ok(done)
}

async fn finish(mint: &Mint, dir: &Path, url: &str, id: &str) -> Result<Issued> {
    sign(mint, id).await?;
    let entry = read(mint, id).await?.context("issue vanished")?;
    let Entry::Committed {
        keyset,
        outputs,
        signatures,
    } = entry
    else {
        let Entry::Written { amount, file } = entry else {
            bail!("issue {id} not committed");
        };
        return Ok(Issued {
            id: id.to_owned(),
            amount,
            file: file.into(),
        });
    };
    let keys = mint
        .keyset(&keyset)
        .with_context(|| format!("keyset {keyset} missing"))?
        .keys;
    let proofs = construct_proofs(
        signatures,
        outputs.iter().map(|o| o.r.clone()).collect(),
        outputs.iter().map(|o| o.secret.clone()).collect(),
        &keys,
    )?;
    let amount: u64 = outputs.iter().map(|o| o.amount.to_u64()).sum();
    let token = Token::new(MintUrl::from_str(url)?, proofs, None, CurrencyUnit::Sat).to_string();
    let file = write_token(dir, id, &token)?;
    write(
        mint,
        id,
        &Entry::Written {
            amount,
            file: file.display().to_string(),
        },
    )
    .await?;
    Ok(Issued {
        id: id.to_owned(),
        amount,
        file,
    })
}

/// `<dir>/issued/<id>.token`, 0600, written atomically (same content on every retry).
fn write_token(dir: &Path, id: &str, token: &str) -> Result<PathBuf> {
    let issued = dir.join("issued");
    fs::create_dir_all(&issued).with_context(|| format!("create {}", issued.display()))?;
    fs::set_permissions(&issued, fs::Permissions::from_mode(0o700))?;
    let file = issued.join(format!("{id}.token"));
    let tmp = issued.join(format!(".{id}.token.tmp"));
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("create {}", tmp.display()))?;
    out.write_all(token.as_bytes())?;
    out.write_all(b"\n")?;
    out.sync_all()?;
    fs::rename(&tmp, &file)?;
    Ok(file)
}

pub async fn read(mint: &Mint, id: &str) -> Result<Option<Entry>> {
    let bytes = mint.localstore().kv_read(NAMESPACE, ISSUES, id).await?;
    Ok(bytes.map(|b| serde_json::from_slice(&b)).transpose()?)
}

async fn write(mint: &Mint, id: &str, entry: &Entry) -> Result<()> {
    let value = serde_json::to_vec(entry)?;
    let mut tx = mint.localstore().begin_transaction().await?;
    tx.kv_write(NAMESPACE, ISSUES, id, &value).await?;
    tx.commit().await?;
    Ok(())
}
