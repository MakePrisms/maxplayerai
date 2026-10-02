//! Private posting keeps all existing validation/budget gates. Repository setup
//! and complete pinned inputs precede publication; no later buyer-input phase.
use super::{
    Error, Result, builders, channel::ContentContext, hosting, inputs, repositories, session, wire,
};
use crate::{
    gateway,
    home::MaxplayerHome,
    job_lifecycle::{PostJobOutcome, PostJobRequest},
};
use nostr_sdk::Keys;

pub async fn post(
    home: &MaxplayerHome,
    keys: &Keys,
    request: &PostJobRequest,
    validated: &gateway::EventDraft,
    contribution: Option<&crate::contribution::ContributionOffer>,
) -> Result<PostJobOutcome> {
    let parsed = gateway::parse_offer(validated).map_err(|_| Error("invalid posting request"))?;
    let offer = gateway::OfferDraft {
        task: parsed.task,
        output: parsed.output,
        amount_sats: parsed.amount,
        deadline_unix: parsed.deadline_unix,
        seller_pubkey: parsed.seller_pubkey,
        requested_agent: parsed.requested_agent,
        requested_harness_family: parsed.requested_harness_family,
        requested_model: parsed.requested_model,
        required_capabilities: parsed.required_capabilities,
        payment_mode: parsed.payment_mode,
        accepts_delivery: parsed.accepts_delivery,
    };
    let mut ctx = ContentContext::open(home, &keys.public_key().to_hex())?;
    let job = super::random_id()?;
    let staging = super::input_staging::Staging::new(&home.root)
        .map_err(|_| Error("input staging unavailable"))?;
    let repo = git2::Repository::init_bare(staging.path())
        .map_err(|_| Error("input staging unavailable"))?;
    let snapshot = if request.inputs.is_empty() {
        None
    } else {
        Some(inputs::prepare(&repo, &job, &request.inputs)?)
    };
    // Both discovery modes prepare the exact base before the offer becomes visible.
    // The base gets its OWN staging repository: input blobs written beside the
    // fetched pack would disqualify it from being forwarded unchanged (#1095).
    let imported_base =
        contribution.map(|c| (c.target.clone_url().to_owned(), c.base.oid().to_owned()));
    let base_staging = match imported_base {
        Some(_) => Some(
            super::input_staging::Staging::new(&home.root)
                .map_err(|_| Error("input staging unavailable"))?,
        ),
        None => None,
    };
    let imported_ref = if let (Some((source, oid)), Some(base_staging)) =
        (&imported_base, &base_staging)
    {
        let base_repo = git2::Repository::init_bare(base_staging.path())
            .map_err(|_| Error("input staging unavailable"))?;
        let mint = if crate::delivery_transport::is_relay_git_locator(source) {
            let intended = source.clone();
            let signing = keys.clone();
            Some(std::sync::Arc::new(move |destination: &str| {
                if !crate::git_transport::same_destination(&intended, destination) {
                    return Err("wrong base input source".into());
                }
                crate::git_transport::nip98_authorization_header_with_keys(
                    destination, &signing, None, None,
                ).map_err(|e| e.to_string())
            }) as crate::git_transport::AuthMinter)
        } else {
            None
        };
        let (source, oid) = (source.clone(), oid.clone());
        let inputs_path = snapshot.as_ref().map(|_| staging.path().to_owned());
        let base_repo = tokio::task::spawn_blocking(move || {
            crate::git_transport::fetch_private_input_base(&base_repo, &source, &oid, mint)
                .map_err(|_| Error("pinned contribution base unavailable"))?;
            let used = repositories::check_objects_after(&base_repo, Default::default())?;
            // Base and inputs land in one job repository: check their combined quota
            // before uploading either, not each staging repository alone.
            if let Some(path) = inputs_path {
                let inputs = git2::Repository::open_bare(path)
                    .map_err(|_| Error("input staging unavailable"))?;
                repositories::check_objects_after(&inputs, used)?;
            }
            Ok::<_, Error>(base_repo)
        })
        .await
        .map_err(|_| Error("base input worker unavailable"))??;
        let reference = format!("refs/heads/input/{}", super::random_id()?);
        base_repo.reference(
            &reference,
            git2::Oid::from_str(&imported_base.as_ref().unwrap().1)
                .map_err(|_| Error("invalid base pin"))?,
            false,
            "pinned input",
        )
        .map_err(|_| Error("base input pin unavailable"))?;
        Some((base_repo, reference))
    } else {
        None
    };
    let pin = contribution.map(|c| super::Contribution {
        target_owner_pubkey: c.target.owner_pubkey().into(),
        target_clone_url: c.target.clone_url().into(),
        base_branch: c.base.branch().into(),
        base_oid: c.base.oid().into(),
        accepts: c.accepts.clone(),
        input: None,
    });
    let prepared = builders::prepare_offer(
        keys,
        &offer,
        builders::OfferOptions {
            visibility: wire::Visibility::Private,
            category: request
                .output_category
                .ok_or(Error("explicit output category required"))?,
            service: &ctx.policy.service,
            job_id: &job,
            attachments: snapshot
                .as_ref()
                .map(|s| s.manifest.clone())
                .unwrap_or_default(),
            contribution: pin,
        },
        &ctx.policy.host,
    )?;
    // Setup is authenticated storage, not an application ACK or a lifecycle event.
    {
        let provision =
            hosting::ProvisionRequest::new(&prepared.event, None, None, &ctx.policy.host)?;
        let auth = hosting::auth_header(keys, provision.url(), provision.body())?;
        provision.send(&auth).await?;
        // The fetched base goes first: its pack can be forwarded unchanged only
        // into a still-empty repository. Inputs are a separate root commit.
        if let Some((base_repo, reference)) = imported_ref {
            let remote = provision.repo().to_owned();
            let intended = remote.clone();
            let scope = reference.clone();
            let signing = keys.clone();
            let oid = imported_base
                .as_ref()
                .ok_or(Error("base input missing"))?
                .1
                .clone();
            let mint: crate::git_transport::AuthMinter = std::sync::Arc::new(move |destination| {
                if !crate::git_transport::same_destination(&intended, destination) {
                    return Err("wrong input destination".into());
                }
                crate::git_transport::nip98_authorization_header_with_keys(
                    destination,
                    &signing,
                    Some(&scope),
                    None,
                )
                .map_err(|e| e.to_string())
            });
            let store = crate::collect::delivery_store_path(home);
            tokio::task::spawn_blocking(move || {
                crate::git_transport::push_private_input(
                    &base_repo, &remote, &reference, &oid, mint,
                )
                .map_err(|e| repositories::upload_error(&e, "base input upload unavailable"))?;
                // Keep the fetched base for the buyer's later verify fetch (#1096 review
                // B2). Advisory: a failure only makes that fetch as slow as before.
                if let Err(e) = crate::store_seed::write(&store, base_repo.path(), &oid) {
                    crate::opline!("buyer base seed not kept ({e}); collect fetches unseeded");
                }
                Ok::<_, Error>(())
            })
            .await
            .map_err(|_| Error("base input worker unavailable"))??;
        }
        if let (Some(snapshot), Some(task)) = (&snapshot, &prepared.task) {
            let intended = provision.repo().to_owned();
            let scope = snapshot.reference.clone();
            let signing = keys.clone();
            let mint: crate::git_transport::AuthMinter = std::sync::Arc::new(move |destination| {
                if !crate::git_transport::same_destination(&intended, destination) {
                    return Err("wrong private input destination".into());
                }
                crate::git_transport::nip98_authorization_header_with_keys(
                    destination,
                    &signing,
                    Some(&scope),
                    None,
                )
                .map_err(|e| e.to_string())
            });
            let (snapshot, task, path) = (snapshot.clone(), task.clone(), staging.path().to_owned());
            let host = wire::HostPolicy {
                git_prefix: ctx.policy.host.git_prefix.clone(),
                accepted_mints: ctx.policy.host.accepted_mints.clone(),
            };
            tokio::task::spawn_blocking(move || {
                let repo = git2::Repository::open_bare(path)
                    .map_err(|_| Error("input staging unavailable"))?;
                repositories::upload_inputs(&repo, &task, &snapshot, &host, mint)
            })
            .await
            .map_err(|_| Error("input upload worker unavailable"))??;
        }
    }
    // From the enqueue on, the outbox may publish this offer even after a crash.
    crate::job_lifecycle::note_offer_before_publication(&prepared.event.id.to_hex())
        .map_err(|_| Error("preparation state unavailable; offer not published"))?;
    if let Some(task) = &prepared.task {
        ctx.enqueue(&prepared.event, &prepared.event, None, None, None, task)?;
    } else {
        ctx.store
            .enqueue_open_offer(&prepared.event, &ctx.policy.service, &ctx.policy.host)?;
    }
    // A failed relay attempt leaves a durable job, not an invitation to repost it.
    // The buyer tracks the returned OFFER id and retries its outbox before judging absence.
    if let Ok(mut transport) =
        session::AuthenticatedContentRelay::connect(keys, &home.config.relay_url).await
    {
        let _ = session::flush(&mut ctx.store, keys, &mut transport, 64).await;
        transport.disconnect().await;
    }
    let id = prepared.event.id.to_hex();
    Ok(PostJobOutcome {
        job_id: id.clone(),
        job_hash: super::job_hash(&id)?,
        offer_kind: gateway::JOB_OFFER_KIND,
        targeted: offer.seller_pubkey.is_some(),
        seller_pubkey: offer.seller_pubkey,
        amount_sats: offer.amount_sats,
        relay_url: home.config.relay_url.clone(),
        task: offer.task,
        output: offer.output,
    })
}

/// Called under the buyer daemon home lock; each live staging directory also has its own lease.
pub(crate) fn clean_stale_staging(home: &std::path::Path) -> std::io::Result<()> {
    super::input_staging::clean(home)
}
