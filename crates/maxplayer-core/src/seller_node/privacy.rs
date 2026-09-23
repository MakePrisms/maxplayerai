//! Private adaptation of the existing seller lifecycle; no new execution phase.
use crate::{gateway::EventDraft, home::MaxplayerHome, private_content as pc};
use nostr_sdk::Event;

pub fn is_private(draft: &EventDraft) -> bool {
    draft
        .tags
        .iter()
        .any(|t| t.0.as_slice() == ["visibility", "private"])
        || (draft.tags.iter().any(|t| t.first() == Some("job"))
            && !draft
                .tags
                .iter()
                .any(|t| t.0.as_slice() == ["visibility", "public"]))
}
pub fn known(
    home: &MaxplayerHome,
    seller: &str,
    id: &str,
) -> pc::Result<Option<(pc::channel::ContentContext, Event)>> {
    let path = home.root.join("private-content.sqlite");
    let offer = if path.exists() {
        pc::store::ContentStore::open(&path)?.event(id)?
    } else {
        None
    };
    let Some(offer) = offer else {
        // The lifecycle DB's marker survives independently of the content cache.
        let lifecycle = home.root.join(super::STATE_DB_FILE);
        if lifecycle.exists() {
            use rusqlite::OptionalExtension;
            let db = rusqlite::Connection::open_with_flags(
                lifecycle,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .map_err(|_| pc::Error("private classification unavailable"))?;
            let marked: Option<String> = db
                .query_row(
                    "SELECT value FROM seller_meta WHERE key=?1",
                    [format!("private-offer:{id}")],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|_| pc::Error("private classification unavailable"))?;
            if marked.is_some() {
                return Err(pc::Error("private offer content context missing"));
            }
        }
        return Ok(None);
    };
    let context = pc::channel::ContentContext::open(home, seller)?;
    Ok(Some((context, offer)))
}
/// Prepare text before a seller co-signature or outbox write. The exact nonce is
/// retained across retry; the outbox itself then contains only closed public tags.
pub fn project(
    home: &MaxplayerHome,
    seller: &str,
    id: &str,
    draft: EventDraft,
    dispatch: Option<pc::Dispatch>,
) -> pc::Result<EventDraft> {
    let Some((mut ctx, offer)) = known(home, seller, id)? else {
        if is_private(&draft) {
            return Err(pc::Error("private offer context unavailable"));
        }
        return pc::public_v2::project_local(home, id, draft);
    };
    let selected = ctx.store.selection(id)?;
    let (claim, award) = selected
        .as_ref()
        .map(|(c, a)| (Some(c), Some(a)))
        .unwrap_or_default();
    let mut text = draft.content.clone();
    for tag in &draft.tags {
        if tag.first() == Some("status") && tag.0.len() > 2 {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&tag.0[2]);
        }
        if tag.first() == Some("reason_detail") {
            if let Some(detail) = tag.0.get(1) {
                text.push('\n');
                text.push_str(detail);
            }
        }
    }
    let kind = match draft.kind {
        3402 => pc::ContentType::ClaimDetails,
        3403 => pc::ContentType::Answer,
        3404 => pc::ContentType::Feedback,
        _ => return Err(pc::Error("unsupported private seller publication")),
    };
    let content = if draft.kind == 3402 || !text.is_empty() {
        let intent = if draft.kind == 3403 {
            format!("answer:{id}")
        } else if draft.kind == 3402 {
            format!("claim:{id}")
        } else {
            format!(
                "feedback:{id}:{}",
                crate::receipt::result_content_hash_hex(&text)
            )
        };
        Some(ctx.prepare(&intent, &offer, claim, award, kind, text, dispatch)?)
    } else {
        None
    };
    pc::carriers::project(&offer, &draft, award, content.as_ref(), &ctx.policy.host)
}

pub fn bind_receipt(
    home: &MaxplayerHome,
    seller: &str,
    id: &str,
    preimage: &mut crate::receipt::ReceiptPreimage,
    answer: Option<&str>,
) -> pc::Result<()> {
    let Some((mut ctx, offer)) = known(home, seller, id)? else {
        return pc::public_v2::bind_receipt(home, seller, id, preimage, answer);
    };
    let (claim, award) = ctx
        .store
        .selection(id)?
        .ok_or(pc::Error("private selection missing"))?;
    pc::lifecycle::validate_selection(&offer, &claim, &award, &ctx.policy.host)?;
    if claim.pubkey.to_hex() != seller {
        return Err(pc::Error("not selected private seller"));
    }
    preimage.protocol = crate::receipt::ReceiptProtocol::V2;
    preimage.job_hash = pc::job_hash(id)?;
    if let Some(answer) = answer {
        let content = ctx.prepare(
            &format!("answer:{id}"),
            &offer,
            Some(&claim),
            Some(&award),
            pc::ContentType::Answer,
            answer.into(),
            None,
        )?;
        preimage.delivery_integrity_hash = content.commitment().into();
    }
    pc::settlement::canonical_json(preimage)?;
    Ok(())
}

pub fn record_publication(home: &MaxplayerHome, seller: &str, event: &Event) -> pc::Result<()> {
    let policy = pc::runtime::Policy::from_home(home)?;
    let tags = pc::wire::validate_private(event, &policy.host)?;
    let id = tags.required("root")?;
    let (mut ctx, offer) =
        known(home, seller, id)?.ok_or(pc::Error("private publication context missing"))?;
    let selected = ctx.store.selection(id)?;
    let (claim, award) = selected
        .as_ref()
        .map(|(c, a)| (Some(c), Some(a)))
        .unwrap_or_default();
    if event.kind.as_u16() == 3403 {
        pc::lifecycle::validate_result(
            &offer,
            claim.ok_or(pc::Error("missing claim"))?,
            award.ok_or(pc::Error("missing award"))?,
            event,
            &ctx.policy.host,
        )?;
    }
    if let Some(content_id) = tags.get("content-id") {
        let content = ctx
            .store
            .authored(tags.required("job")?, content_id, seller)?
            .ok_or(pc::Error("immutable private content missing"))?;
        ctx.enqueue(
            event,
            &offer,
            if event.kind.as_u16() == 3402 {
                Some(event)
            } else {
                claim
            },
            award,
            None,
            &content,
        )?;
    } else {
        ctx.store
            .remember_event(event, &offer, seller, &ctx.policy.service, &ctx.policy.host)?;
    }
    Ok(())
}

pub fn contribution(pin: &pc::Contribution) -> pc::Result<crate::contribution::ContributionOffer> {
    Ok(crate::contribution::ContributionOffer {
        target: crate::contribution::TargetRepoPin::new(
            pin.target_owner_pubkey.clone(),
            pin.target_clone_url.clone(),
        )
        .map_err(|_| pc::Error("invalid private contribution target"))?,
        base: crate::contribution::ContributionBase::new(
            pin.base_branch.clone(),
            pin.base_oid.clone(),
        )
        .map_err(|_| pc::Error("invalid private contribution base"))?,
        accepts: pin.accepts.clone(),
    })
}

pub fn input_cache(home: &MaxplayerHome, offer: &str) -> std::path::PathBuf {
    home.root.join("private-inputs").join(offer)
}
/// Prove the exact inputs are readable before CLAIM. Cache only verified Git
/// objects; execution later uses this pinned baseline instead of fetching a new tip.
pub async fn preflight(
    home: &MaxplayerHome,
    signer: &super::signer::SignerHandle,
    event: &Event,
    resolved: &pc::lifecycle::ResolvedOffer,
) -> pc::Result<()> {
    if resolved.attachments.is_empty() && resolved.contribution.is_none() {
        return Ok(());
    }
    let mut ctx = pc::channel::ContentContext::open(home, signer.public_key_hex())?;
    let imported_file = resolved
        .contribution
        .as_ref()
        .and_then(|c| c.input.as_ref())
        .is_some();
    let task = if resolved.attachments.is_empty() && !imported_file {
        None
    } else {
        Some(ctx.accept_content(
            event,
            event,
            None,
            None,
            None,
            nostr_sdk::Timestamp::now().as_secs(),
        )?)
    };
    let tags = pc::wire::validate_private(event, &ctx.policy.host)?;
    let remote = ctx
        .policy
        .host
        .job_repo(&event.pubkey.to_hex(), tags.required("job")?)?;
    let targeted_base =
        resolved.contribution.is_some() && tags.get("discovery") == Some("targeted");
    let auth = if task.is_some() || targeted_base {
        Some(
            signer
                .http_auth_header(remote.clone(), None, None)
                .await
                .map_err(|_| pc::Error("input signer unavailable"))?
                .map_err(|_| pc::Error("input authorization unavailable"))?,
        )
    } else {
        None
    };
    let mut pin = resolved.contribution.clone();
    if targeted_base {
        if let Some(pin) = &mut pin {
            pin.target_clone_url = remote.clone();
        }
    }
    let base_auth = if let Some(pin) = &pin {
        if targeted_base
            || pin
                .target_clone_url
                .starts_with(&ctx.policy.host.git_prefix)
        {
            Some(
                signer
                    .http_auth_header(pin.target_clone_url.clone(), None, None)
                    .await
                    .map_err(|_| pc::Error("input signer unavailable"))?
                    .map_err(|_| pc::Error("input authorization unavailable"))?,
            )
        } else {
            None
        }
    } else {
        None
    };
    let cache = input_cache(home, &event.id.to_hex());
    let buyer = event.pubkey.to_hex();
    tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(cache.parent().ok_or(pc::Error("input cache unavailable"))?)
            .map_err(|_| pc::Error("input cache unavailable"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                cache.parent().unwrap(),
                std::fs::Permissions::from_mode(0o700),
            )
            .map_err(|_| pc::Error("input cache permissions unavailable"))?;
        }
        let repo = if cache.exists() {
            git2::Repository::open_bare(&cache)
        } else {
            git2::Repository::init_bare(&cache)
        }
        .map_err(|_| pc::Error("input cache unavailable"))?;
        let scratch = tempfile::tempdir_in(cache.parent().unwrap())
            .map_err(|_| pc::Error("input quarantine unavailable"))?;
        let mut index = git2::Index::new().map_err(|_| pc::Error("input index unavailable"))?;
        let base = if let Some(pin) = &pin {
            // Same destination fence as every other delivery read, with bounded
            // pack transfer before the cumulative uncompressed-object check.
            crate::git_transport::fetch_bounded_objects(
                &repo,
                &pin.target_clone_url,
                &[&pin.base_oid],
                base_auth.as_deref(),
            )
            .map_err(|_| pc::Error("pinned contribution input unavailable"))?;
            pc::repositories::check_objects(&repo)?;
            let commit = repo
                .find_commit(
                    git2::Oid::from_str(&pin.base_oid)
                        .map_err(|_| pc::Error("invalid input pin"))?,
                )
                .map_err(|_| pc::Error("pinned input commit missing"))?;
            index
                .read_tree(
                    &commit
                        .tree()
                        .map_err(|_| pc::Error("input tree unavailable"))?,
                )
                .map_err(|_| pc::Error("input index unavailable"))?;
            Some(commit)
        } else {
            None
        };
        if let Some(task) = &task {
            pc::repositories::fetch_inputs(
                &repo,
                task,
                &buyer,
                &ctx.policy.host,
                auth.as_deref()
                    .ok_or(pc::Error("input authorization missing"))?,
                &scratch.path().join("files"),
            )?;
            for entry in &pc::repositories::input_manifest(task)? {
                // Inputs supplement a contribution; silently replacing its pinned
                // source/checks files would change the offered base.
                if index.iter().any(|e| {
                    let p = String::from_utf8_lossy(&e.path);
                    p == entry.path
                        || p.starts_with(&(entry.path.clone() + "/"))
                        || entry.path.starts_with(&(p.to_string() + "/"))
                }) {
                    return Err(pc::Error("input path conflicts with pinned base"));
                }
                let bytes = std::fs::read(scratch.path().join("files").join(&entry.path))
                    .map_err(|_| pc::Error("verified input unavailable"))?;
                let blob = repo
                    .blob(&bytes)
                    .map_err(|_| pc::Error("input blob unavailable"))?;
                index
                    .add(&git2::IndexEntry {
                        ctime: git2::IndexTime::new(0, 0),
                        mtime: git2::IndexTime::new(0, 0),
                        dev: 0,
                        ino: 0,
                        mode: 0o100644,
                        uid: 0,
                        gid: 0,
                        file_size: bytes.len() as u32,
                        id: blob,
                        flags: 0,
                        flags_extended: 0,
                        path: entry.path.as_bytes().to_vec(),
                    })
                    .map_err(|_| pc::Error("input index unavailable"))?;
            }
        }
        let oid = if task.is_none() {
            base.as_ref().ok_or(pc::Error("missing input base"))?.id()
        } else {
            let tree_id = index
                .write_tree_to(&repo)
                .map_err(|_| pc::Error("input baseline unavailable"))?;
            let tree = repo
                .find_tree(tree_id)
                .map_err(|_| pc::Error("input baseline unavailable"))?;
            let sig =
                git2::Signature::new("Maxplayer", "job@maxplayer.invalid", &git2::Time::new(0, 0))
                    .map_err(|_| pc::Error("input baseline unavailable"))?;
            let parents: Vec<_> = base.iter().collect();
            repo.commit(None, &sig, &sig, "Private job input", &tree, &parents)
                .map_err(|_| pc::Error("input baseline unavailable"))?
        };
        pc::repositories::check_objects(&repo)?;
        if let Ok(old) = repo.find_reference("refs/heads/input-baseline") {
            if old.target() != Some(oid) {
                return Err(pc::Error("private input baseline changed"));
            }
        } else {
            repo.reference(
                "refs/heads/input-baseline",
                oid,
                false,
                "validated private inputs",
            )
            .map_err(|_| pc::Error("input baseline persistence failed"))?;
        }
        Ok(())
    })
    .await
    .map_err(|_| pc::Error("input preflight worker unavailable"))?
}

pub struct Execution {
    pub remote: String,
    pub branch: String,
    pub job_hash: String,
    pub baseline: Option<(std::path::PathBuf, String)>,
    pub contribution: bool,
}
pub fn execution(home: &MaxplayerHome, seller: &str, id: &str) -> pc::Result<Option<Execution>> {
    let Some((mut ctx, offer)) = known(home, seller, id)? else {
        return Ok(None);
    };
    let (claim, award) = ctx
        .store
        .selection(id)?
        .ok_or(pc::Error("private execution selection missing"))?;
    pc::lifecycle::validate_selection(&offer, &claim, &award, &ctx.policy.host)?;
    if claim.pubkey.to_hex() != seller {
        return Err(pc::Error("private job awarded elsewhere"));
    }
    let resolved = ctx.resolve_offer(&offer, nostr_sdk::Timestamp::now().as_secs())?;
    let tags = pc::wire::validate_private(&offer, &ctx.policy.host)?;
    let baseline = if !resolved.attachments.is_empty() || resolved.contribution.is_some() {
        let path = input_cache(home, id);
        let repo = git2::Repository::open_bare(&path)
            .map_err(|_| pc::Error("pre-claim input cache missing"))?;
        pc::repositories::check_objects(&repo)?;
        let oid = repo
            .refname_to_id("refs/heads/input-baseline")
            .map_err(|_| pc::Error("pre-claim input baseline missing"))?;
        Some((path, oid.to_string()))
    } else {
        None
    };
    Ok(Some(Execution {
        remote: ctx
            .policy
            .host
            .job_repo(&offer.pubkey.to_hex(), tags.required("job")?)?,
        branch: format!("refs/heads/delivery/{id}"),
        job_hash: pc::job_hash(id)?,
        baseline,
        contribution: resolved.contribution.is_some(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_private_context_never_reclassifies_a_resumed_job_as_public() {
        let dir = tempfile::tempdir().unwrap();
        let home = crate::home::bootstrap(dir.path()).unwrap();
        let store =
            super::super::store::SellerStore::open(home.root.join(super::super::STATE_DB_FILE))
                .unwrap();
        let id = "11".repeat(32);
        let seller = crate::home::public_key_hex(&home).unwrap();
        assert!(known(&home, &seller, &id).unwrap().is_none());
        store.mark_private_offer(&id).unwrap();
        assert!(known(&home, &seller, &id).is_err());
    }
}
