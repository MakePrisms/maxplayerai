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
    let staging =
        tempfile::tempdir_in(&home.root).map_err(|_| Error("input staging unavailable"))?;
    let repo = git2::Repository::init_bare(staging.path())
        .map_err(|_| Error("input staging unavailable"))?;
    let snapshot = if request.inputs.is_empty() {
        None
    } else {
        Some(inputs::prepare(&repo, &job, &request.inputs)?)
    };
    let imported_base = if offer.seller_pubkey.is_some() {
        contribution.map(|c| (c.target.clone_url().to_owned(), c.base.oid().to_owned()))
    } else {
        None
    };
    let imported_ref = if let Some((source, oid)) = &imported_base {
        let header = if crate::delivery_transport::is_relay_git_locator(source) {
            Some(
                crate::git_transport::nip98_authorization_header_with_keys(
                    source, keys, None, None,
                )
                .map_err(|_| Error("base input authorization unavailable"))?,
            )
        } else {
            None
        };
        let (source, oid) = (source.clone(), oid.clone());
        let repo = tokio::task::spawn_blocking(move || {
            crate::git_transport::fetch_bounded_objects(&repo, &source, &[&oid], header.as_deref())
                .map_err(|_| Error("pinned contribution base unavailable"))?;
            repositories::check_objects(&repo)?;
            Ok::<_, Error>(repo)
        })
        .await
        .map_err(|_| Error("base input worker unavailable"))??;
        let reference = format!("refs/heads/input/{}", super::random_id()?);
        repo.reference(
            &reference,
            git2::Oid::from_str(&imported_base.as_ref().unwrap().1)
                .map_err(|_| Error("invalid base pin"))?,
            false,
            "pinned input",
        )
        .map_err(|_| Error("base input pin unavailable"))?;
        Some((repo, reference))
    } else {
        None
    };
    let repo = if let Some((repo, _)) = &imported_ref {
        git2::Repository::open_bare(repo.path())
    } else {
        git2::Repository::open_bare(staging.path())
    }
    .map_err(|_| Error("input staging unavailable"))?;
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
    if offer.seller_pubkey.is_some() {
        let provision =
            hosting::ProvisionRequest::new(&prepared.event, None, None, &ctx.policy.host)?;
        let auth = hosting::auth_header(keys, provision.url(), provision.body())?;
        provision.send(&auth).await?;
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
            let (snapshot, task, path) = (snapshot.clone(), task.clone(), repo.path().to_owned());
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
            tokio::task::spawn_blocking(move || {
                crate::git_transport::push_private_input(
                    &base_repo, &remote, &reference, &oid, mint,
                )
                .map_err(|_| Error("base input upload unavailable"))
            })
            .await
            .map_err(|_| Error("base input worker unavailable"))??;
        }
    }
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
