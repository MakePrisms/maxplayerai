//! Trusted deployment policy and the privacy boundary around the existing lifecycle.
use super::{
    Error, Result,
    wire::{HostPolicy, Visibility},
};
use crate::home::MaxplayerHome;
use nostr_sdk::prelude::PublicKey;

pub struct Policy {
    pub service: String,
    pub host: HostPolicy,
}
impl Policy {
    #[cfg(feature = "wallet")]
    pub fn for_evidence(
        home: &MaxplayerHome,
        evidence: &super::evidence::PrivateEvidence,
    ) -> Result<Self> {
        if super::public_v2::is_public(&evidence.offer) {
            super::public_v2::validate_offer(&evidence.offer)?;
            // Public evidence does not use a recipient service or private host.
            Ok(Self {
                service: String::new(),
                host: HostPolicy {
                    git_prefix: String::new(),
                    accepted_mints: home.config.accepted_mints.clone(),
                },
            })
        } else {
            Self::from_home(home)
        }
    }

    pub fn from_home(home: &MaxplayerHome) -> Result<Self> {
        let config = &home.config.privacy;
        if !config.private_content_v2 || !config.private_job_repos || !config.private_jobs {
            return Err(Error("private jobs are not enabled on this installation"));
        }
        let service = config
            .service_pubkey
            .as_deref()
            .ok_or(Error("private service identity is not configured"))?;
        super::require_hex(service, 32)?;
        PublicKey::from_hex(service).map_err(|_| Error("invalid private service identity"))?;
        let git_base = config
            .git_base
            .as_deref()
            .ok_or(Error("private Git host is not configured"))?;
        let host = HostPolicy {
            git_prefix: git_base.to_owned(),
            accepted_mints: home.config.accepted_mints.clone(),
        };
        host.job_repo(service, &"00".repeat(32))?;
        Ok(Self {
            service: service.to_owned(),
            host,
        })
    }
}
/// None resolves through the persisted default, never through recipient availability.
/// A disabled/misconfigured private lane is an error, not a reason to change visibility.
pub fn requested_visibility(
    home: &MaxplayerHome,
    requested: Option<Visibility>,
) -> Result<Visibility> {
    let visibility = match requested {
        Some(visibility) => visibility,
        None => match home.config.privacy.default_visibility.as_str() {
            "private" => Visibility::Private,
            "public" => Visibility::Public,
            _ => return Err(Error("invalid configured job visibility")),
        },
    };
    if visibility == Visibility::Private {
        Policy::from_home(home)?;
    }
    Ok(visibility)
}
