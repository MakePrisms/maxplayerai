//! Durable per-participant content context for the existing buyer/seller controllers.
//! Evidence may arrive in either order; unknown plaintext stays staged until its exact
//! signed carrier chain is available. There is no extra lifecycle phase or service ACK.
use super::{
    ContentType, Dispatch, Error, PreparedContent, Result,
    lifecycle::ResolvedOffer,
    runtime::Policy,
    store::{ContentStore, SignedContext},
};
use crate::home::MaxplayerHome;
use nostr_sdk::prelude::Event;

pub struct ContentContext {
    pub policy: Policy,
    pub store: ContentStore,
    recipient: String,
}
impl ContentContext {
    pub fn open(home: &MaxplayerHome, recipient: &str) -> Result<Self> {
        super::require_hex(recipient, 32)?;
        let policy = Policy::from_home(home)?;
        let store = ContentStore::open(&home.root.join("private-content.sqlite"))?;
        Ok(Self {
            policy,
            store,
            recipient: recipient.into(),
        })
    }
    pub fn recipient(&self) -> &str {
        &self.recipient
    }
    pub fn stage(&mut self, content: &PreparedContent, now: u64) -> Result<()> {
        self.store.stage(content, &self.recipient, now)?;
        Ok(())
    }
    pub fn resolve_offer(&mut self, offer: &Event, now: u64) -> Result<ResolvedOffer> {
        self.store.remember_event(
            offer,
            offer,
            &self.recipient,
            &self.policy.service,
            &self.policy.host,
        )?;
        let tags = super::wire::validate_private(offer, &self.policy.host)?;
        let content = if tags.get("discovery") == Some("targeted") {
            Some(self.accept_content(offer, offer, None, None, None, now)?)
        } else {
            None
        };
        super::lifecycle::resolve_offer(
            offer,
            content.as_ref(),
            &self.recipient,
            &self.policy.service,
            &self.policy.host,
        )
    }
    fn resolve_content(&self, context: &SignedContext<'_>, now: u64) -> Result<PreparedContent> {
        let tags = super::wire::validate_private(context.carrier, &self.policy.host)?;
        let job = tags.required("job")?;
        let id = tags.required("content-id")?;
        let content = self
            .store
            .get(job, id)?
            .or(self
                .store
                .staged(&context.carrier.pubkey.to_hex(), job, id, now)?)
            .or(if context.carrier.pubkey.to_hex() == self.recipient {
                self.store.authored(job, id, &self.recipient)?
            } else {
                None
            })
            .ok_or(Error("private content is not yet available"))?;
        context.validate(&content)?;
        if !content.body().recipients.contains(&self.recipient) {
            return Err(Error("not a content recipient"));
        }
        Ok(content)
    }
    pub fn accept_content(
        &mut self,
        carrier: &Event,
        offer: &Event,
        claim: Option<&Event>,
        award: Option<&Event>,
        result: Option<&Event>,
        now: u64,
    ) -> Result<PreparedContent> {
        let context = SignedContext {
            carrier,
            offer,
            claim,
            award,
            result,
            service: &self.policy.service,
            host: &self.policy.host,
        };
        let content = self.resolve_content(&context, now)?;
        self.store
            .receive_signed(&content, &context, &self.recipient, now)?;
        self.store.remember_event(
            carrier,
            offer,
            &self.recipient,
            &self.policy.service,
            &self.policy.host,
        )?;
        Ok(content)
    }
    pub fn select(&mut self, offer: &Event, claim: &Event, award: &Event) -> Result<()> {
        self.store.remember_selection(
            offer,
            claim,
            award,
            &self.recipient,
            &self.policy.service,
            &self.policy.host,
        )
    }
    /// Called before signing the inline receipt digest and again on retry. Returning
    /// the original envelope makes the digest independent of restart or wrapper randomness.
    pub fn prepare(
        &mut self,
        intent: &str,
        offer: &Event,
        claim: Option<&Event>,
        award: Option<&Event>,
        kind: ContentType,
        text: String,
        dispatch: Option<Dispatch>,
    ) -> Result<PreparedContent> {
        let proposed = super::carriers::prepare_content(
            offer,
            claim,
            award,
            &self.recipient,
            &self.policy.service,
            kind,
            text,
            dispatch,
            &self.policy.host,
        )?;
        self.store.prepare_once(intent, proposed.body().clone())
    }
    pub fn enqueue(
        &mut self,
        carrier: &Event,
        offer: &Event,
        claim: Option<&Event>,
        award: Option<&Event>,
        result: Option<&Event>,
        content: &PreparedContent,
    ) -> Result<()> {
        let context = SignedContext {
            carrier,
            offer,
            claim,
            award,
            result,
            service: &self.policy.service,
            host: &self.policy.host,
        };
        self.store.enqueue_signed(content, &context)?;
        self.store.remember_event(
            carrier,
            offer,
            &self.recipient,
            &self.policy.service,
            &self.policy.host,
        )?;
        Ok(())
    }
}
