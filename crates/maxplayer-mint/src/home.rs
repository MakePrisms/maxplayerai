//! `<home>/mint/`: everything the mint is. Losing it makes every credit it issued worthless;
//! restoring an old copy can let spent credits be spent again (spec decision 9).

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use bip39::Mnemonic;
use maxplayer_core::mint_wire::{FALLBACK_RELAYS, MAX_MINT_NAME_CHARS, valid_mint_name};
use nostr_sdk::prelude::{Keys, ToBech32};
use serde::{Deserialize, Serialize};

/// The relay used when `mint.toml` lists none (plus [`FALLBACK_RELAYS`]).
pub const DEFAULT_RELAY: &str = "wss://relay.maxplayer.ai";

/// `mint.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MintConfig {
    /// Relays to listen and answer on. Empty ⇒ [`DEFAULT_RELAY`] + [`FALLBACK_RELAYS`].
    pub relays: Vec<String>,
    /// New requests per second, across all clients.
    pub rate_limit: u32,
    /// Name advertised in NUT-06 info and shown by wallets next to the mint URL. Self-chosen,
    /// like a seller's kind-0 name: the `nostr://` npub stays the mint's identity.
    /// Unset ⇒ [`default_name`] (`maxplayer-mint-<8 hex>`, like a seller's `maxplayer-seller-<8 hex>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Where `run` writes its automatic backups. Unset ⇒ `<home>/mint/backups`. Point it at
    /// storage on another machine or disk so a lost disk doesn't take the backups with it.
    /// A relative path is taken from `<home>/mint/`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_dir: Option<PathBuf>,
}

impl Default for MintConfig {
    fn default() -> Self {
        Self {
            relays: Vec::new(),
            rate_limit: 20,
            name: None,
            backup_dir: None,
        }
    }
}

/// A name wallets will display ([`valid_mint_name`]).
pub fn validate_name(name: &str) -> Result<()> {
    if !valid_mint_name(name) {
        bail!(
            "mint name {name:?} must be 1-{MAX_MINT_NAME_CHARS} characters, with no leading or trailing spaces, control characters, '=', '\"' or '\\'"
        );
    }
    Ok(())
}

/// The name of a mint whose operator chose none: `maxplayer-mint-` + the first 8 hex characters of
/// its key, the same shape as an unnamed seller's `maxplayer-seller-<8 hex>`. Unique per mint, so
/// wallets show it (unlike the old shared "Maxplayer credits").
pub fn default_name(keys: &Keys) -> String {
    format!("maxplayer-mint-{}", &keys.public_key().to_hex()[..8])
}

impl MintConfig {
    /// The name `run` advertises: `name`, else [`default_name`].
    pub fn effective_name(&self, keys: &Keys) -> String {
        self.name.clone().unwrap_or_else(|| default_name(keys))
    }

    /// The backup directory of the mint in `mint_dir`.
    pub fn backup_dir_in(&self, mint_dir: &Path) -> PathBuf {
        match &self.backup_dir {
            Some(dir) => mint_dir.join(dir),
            None => mint_dir.join("backups"),
        }
    }

    pub fn effective_relays(&self) -> Vec<String> {
        if !self.relays.is_empty() {
            return self.relays.clone();
        }
        std::iter::once(DEFAULT_RELAY)
            .chain(FALLBACK_RELAYS.iter().copied())
            .map(str::to_owned)
            .collect()
    }
}

/// What `run` needs from `<home>/mint/`.
pub struct Secrets {
    pub keys: Keys,
    pub seed: [u8; 64],
    pub config: MintConfig,
}

/// `<home>/mint/`.
#[derive(Debug, Clone)]
pub struct MintHome {
    dir: PathBuf,
}

impl MintHome {
    pub fn at(home: &Path) -> Self {
        Self {
            dir: home.join("mint"),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn db_path(&self) -> PathBuf {
        self.dir.join("mint.sqlite")
    }

    fn key_path(&self) -> PathBuf {
        self.dir.join("nostr.key")
    }

    fn seed_path(&self) -> PathBuf {
        self.dir.join("seed")
    }

    fn config_path(&self) -> PathBuf {
        self.dir.join("mint.toml")
    }

    /// Take the single-listener lock: an exclusive `flock` on the `<home>/mint/` directory itself,
    /// held until the returned handle is dropped (the OS releases it if the process dies). The
    /// request log is check-then-act, so two `run` processes on one `mint.sqlite` could both
    /// execute a duplicate and record the loser's definitive error over the winner's success.
    /// Locking the directory rather than a lock file means deleting a file can't sidestep it.
    /// `issue` doesn't take it: it never runs saga recovery and its journal is safe next to a
    /// live `run`.
    pub fn lock_run(&self) -> Result<fs::File> {
        let dir =
            fs::File::open(&self.dir).with_context(|| format!("open {}", self.dir.display()))?;
        match dir.try_lock() {
            Ok(()) => Ok(dir),
            Err(fs::TryLockError::WouldBlock) => bail!(
                "another `maxplayer-mint run` is already serving {}",
                self.dir.display()
            ),
            Err(fs::TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("lock {}", self.dir.display()))
            }
        }
    }

    /// Create a new mint. Refuses if `<home>/mint/` exists in any form: overwriting a mint, or
    /// mixing a new key with an old database, is how credits get lost or double-spent.
    pub fn init(&self) -> Result<Keys> {
        self.init_named(None)
    }

    /// [`Self::init`], writing `name` into the new `mint.toml`.
    pub fn init_named(&self, name: Option<String>) -> Result<Keys> {
        if let Some(name) = &name {
            validate_name(name)?;
        }
        if fs::symlink_metadata(&self.dir).is_ok() {
            bail!(
                "{} already exists; refusing to create a mint over it",
                self.dir.display()
            );
        }
        if let Some(parent) = self.dir.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        // `create_dir` (not `_all`) fails if another process created it in between.
        fs::create_dir(&self.dir).with_context(|| format!("create {}", self.dir.display()))?;
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))?;

        let keys = Keys::generate();
        write_private(&self.key_path(), &keys.secret_key().to_secret_hex())?;
        let mut entropy = [0u8; 16];
        getrandom::fill(&mut entropy).map_err(|error| anyhow!("entropy: {error}"))?;
        let mnemonic = Mnemonic::from_entropy(&entropy)?;
        write_private(&self.seed_path(), &mnemonic.to_string())?;
        write_private(
            &self.config_path(),
            &toml::to_string(&MintConfig {
                name,
                ..MintConfig::default()
            })?,
        )?;
        Ok(keys)
    }

    /// Set `name` in an existing mint's `mint.toml` (other settings kept). A running `run`
    /// advertises it after a restart.
    pub fn set_name(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        let mut config = self.load()?.config;
        config.name = Some(name.to_owned());
        let tmp = self.dir.join("mint.toml.tmp");
        let _ = fs::remove_file(&tmp);
        write_private(&tmp, &toml::to_string(&config)?)?;
        fs::rename(&tmp, self.config_path()).context("replace mint.toml")?;
        Ok(())
    }

    /// `mint.toml` alone (defaults when it is missing).
    pub fn config(&self) -> Result<MintConfig> {
        let config: MintConfig = match fs::read_to_string(self.config_path()) {
            Ok(text) => toml::from_str(&text).context("parse mint.toml")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => MintConfig::default(),
            Err(error) => return Err(error).context("read mint.toml"),
        };
        if let Some(name) = &config.name {
            validate_name(name).context("mint.toml")?;
        }
        Ok(config)
    }

    pub fn load(&self) -> Result<Secrets> {
        let key = read(&self.key_path())?;
        let keys = Keys::parse(key.trim()).context("parse nostr.key")?;
        let seed = parse_seed(&read(&self.seed_path())?)?;
        let config = self.config()?;
        Ok(Secrets { keys, seed, config })
    }
}

/// The 64-byte seed from the text of a `seed` file (a BIP-39 mnemonic).
pub fn parse_seed(text: &str) -> Result<[u8; 64]> {
    let mnemonic = Mnemonic::parse_normalized(text.trim()).context("parse seed")?;
    Ok(mnemonic.to_seed_normalized(""))
}

/// `nostr://<npub>` for the mint's key.
pub fn mint_url(keys: &Keys) -> Result<String> {
    Ok(format!(
        "{}{}",
        maxplayer_core::mint_wire::NOSTR_MINT_SCHEME,
        keys.public_key().to_bech32()?
    ))
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn read(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}
