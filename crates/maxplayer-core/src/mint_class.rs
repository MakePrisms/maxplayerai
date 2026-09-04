//! The mint CLASS a buyer's wallet reasons about before it moves anything: Lightning-backed, or an
//! ISSUER mint — a seat's own Cashu mint whose tokens are an IOU for that seat's future work
//! (`docs/protocol-v1.md` §4.2 "Issuer mint").
//!
//! An issuer mint has NO Lightning. Nothing enters it from outside and nothing leaves it: its tokens
//! are minted by the issuer on its own authority and are good for exactly one thing, hiring the
//! issuer. Two facts follow, and every rule in this module is one of them spelled out:
//!
//! - **The real-mint fence admits it.** The fence exists to stop REAL sats moving without an
//!   operator's opt-in. An issuer mint carries no sats, so it passes whatever `allow_real_mints`
//!   says, and whatever scheme its URL has (a sidecar on `http://127.0.0.1` is the normal case).
//! - **The Lightning hop refuses it, in both directions.** A hop melts at a source mint to pay an
//!   invoice a target mint raised. An issuer mint can neither pay nor be paid over Lightning, so a
//!   hop INTO one cannot land and a hop OUT of one cannot leave. The plain reason a buyer reads is
//!   [`ISSUER_HOP_REFUSAL`]: it holds none of this seller's currency, and Lightning cannot buy any.
//!
//! How a mint is KNOWN to be an issuer mint — it is DECLARED by an operator, and never inferred
//! from the mint itself. Each declaration is recorded with WHO said it ([`IssuerMarker`]), because
//! the two rules above do not trust the two sources equally:
//!
//! 1. **This seat's own config** ([`crate::home::MaxplayerConfig::issuer_mint`]) — the source its
//!    kind-30340 `issuer_mint` tag ([`crate::heartbeat::ISSUER_MINT_TAG`]) is published from — is
//!    [`IssuerMarker::Own`]. The operator stated it; it admits and it refuses.
//! 2. **A counterparty's advertisement.** A SELLER's declaration is read off the seller's
//!    announcement at accept and is [`IssuerMarker::Declared`]. It REFUSES the hop (the seller's
//!    word can only make the buyer more careful) but does NOT widen the fence: a seller's signed
//!    tag must not be able to open the buyer's real-mint fence to any mint the seller cares to
//!    name — a real mint the buyer holds sats at, declared "issuer" by a stranger, would otherwise
//!    become spendable with the real-money switch off.
//!
//! What a mint says about ITSELF (its NUT-06 `/v1/info` document) is not a source: no production
//! path reads a mint's info to obtain its class. Whatever is known is sealed into the accept-bind
//! ([`IssuerMintSeal`]) so the pay path re-derives rather than re-decides.
//!
//! Absence of every marker is UNKNOWN, and unknown reads as Lightning: the fence and the hop then
//! behave exactly as they did before this class existed.

use std::collections::BTreeMap;
use std::str::FromStr;

use cdk::mint_url::MintUrl;
use serde::{Deserialize, Serialize};

use crate::home;

/// The reason a buyer reads when a Lightning hop would have to enter or leave an issuer mint.
pub const ISSUER_HOP_REFUSAL: &str = "you hold none of this seller's currency";

/// Which kind of mint a URL names, as far as the wallet can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MintClass {
    /// A mint reachable over Lightning — every mint the wallet knew before issuer mints existed,
    /// and every mint it knows nothing about. The default, because unknown reads as Lightning.
    #[default]
    Lightning,
    /// A seat's own mint: no Lightning in, none out; its tokens buy that seat's work and nothing
    /// else.
    Issuer,
}

/// WHO declared a mint an issuer mint. Every marker refuses the Lightning hop; only the marker
/// this seat can stand behind — its own config — widens its real-mint fence (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssuerMarker {
    /// Another seat's kind-30340 `issuer_mint` tag, read at accept. Refuses; does not admit.
    ///
    /// Also what a seal written under the retired `info` marker (a class once inferred from the
    /// mint's own NUT-06 document) reads back as: an old bind still loads, the hop it refused it
    /// still refuses, and a class nobody declared never widens the fence.
    #[serde(alias = "info")]
    Declared,
    /// This seat's own issuer mint, from its config. Refuses and admits.
    Own,
}

impl IssuerMarker {
    /// Whether this marker is one the seat may widen its OWN real-mint fence on.
    fn admits(self) -> bool {
        matches!(self, Self::Own)
    }
}

/// One sealed issuer-mint fact: a normalized URL and who said it. The unit the accept-bind stores
/// so the pay path re-derives the identical fence and hop decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuerMintSeal {
    pub url: String,
    pub marker: IssuerMarker,
}

/// The issuer mints known for ONE payment decision, each with its marker. Built once, passed by
/// reference into [`crate::crossmint::plan_payment`] and [`crate::crossmint::select_source_mint`],
/// and sealed into the accept-bind so the pay path re-derives the identical decision.
///
/// URLs are normalized through [`MintUrl`] (trailing slash, case) so `contains` agrees with the
/// comparisons the planner makes. An entry that does not parse as a mint URL is dropped: it could
/// never match a planned mint, and keeping it would only let a malformed config line masquerade
/// as knowledge. When two markers name one URL the stronger stands: an admitting marker is never
/// downgraded by a declaration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssuerMints {
    marks: BTreeMap<String, IssuerMarker>,
}

/// [`IssuerMints::none`] with a `'static` address, for a borrower that outlives any local — the
/// award filters are `Copy` and hold a reference.
pub static NO_ISSUER_MINTS: IssuerMints = IssuerMints {
    marks: BTreeMap::new(),
};

impl IssuerMints {
    /// No issuer mint is known. Every decision then runs exactly as it did before the class
    /// existed.
    pub fn none() -> Self {
        Self::default()
    }

    /// Rebuild from a sealed bind's list, marker by marker. The pay path's ONLY constructor.
    pub fn from_seal(seal: &[IssuerMintSeal]) -> Self {
        let mut known = Self::default();
        for entry in seal {
            known.insert(&entry.url, entry.marker);
        }
        known
    }

    /// Add the seat's OWN issuer mint ([`IssuerMarker::Own`]), when it has one. `None` adds nothing.
    pub fn with_own(mut self, own_mint: Option<&str>) -> Self {
        if let Some(own) = own_mint {
            self.insert(own, IssuerMarker::Own);
        }
        self
    }

    /// Add a mint another seat DECLARED its issuer mint on its announcement
    /// ([`IssuerMarker::Declared`]), when it stated one. `None` adds nothing.
    pub fn with_declared(mut self, declared: Option<&str>) -> Self {
        if let Some(url) = declared {
            self.insert(url, IssuerMarker::Declared);
        }
        self
    }

    /// Add one issuer mint under `marker`. Silently ignores a URL that does not parse (see the type
    /// docs); never downgrades an admitting marker to a declaration.
    pub fn insert(&mut self, url: &str, marker: IssuerMarker) {
        if let Ok(parsed) = MintUrl::from_str(url) {
            let slot = self.marks.entry(parsed.to_string()).or_insert(marker);
            if marker > *slot {
                *slot = marker;
            }
        }
    }

    /// Whether `url` names a known issuer mint under ANY marker — the hop-refusal question. An
    /// unparseable `url` is never one.
    pub fn contains(&self, url: &str) -> bool {
        self.marker_of(url).is_some()
    }

    /// Whether `url` names an issuer mint this seat may widen its real-mint fence on — the fence
    /// question. A declared-only mint answers `false`: it refuses hops but is fenced as before.
    pub fn admits(&self, url: &str) -> bool {
        self.marker_of(url).is_some_and(IssuerMarker::admits)
    }

    /// The marker recorded for `url`, if any.
    pub fn marker_of(&self, url: &str) -> Option<IssuerMarker> {
        MintUrl::from_str(url)
            .ok()
            .and_then(|parsed| self.marks.get(&parsed.to_string()).copied())
    }

    /// The class of `url` given what is known: `Issuer` when listed under any marker, `Lightning`
    /// otherwise.
    pub fn class_of(&self, url: &str) -> MintClass {
        if self.contains(url) {
            MintClass::Issuer
        } else {
            MintClass::Lightning
        }
    }

    /// Nothing known.
    pub fn is_empty(&self) -> bool {
        self.marks.is_empty()
    }

    /// The known issuer mints, normalized, in a stable order, every marker.
    pub fn urls(&self) -> Vec<String> {
        self.marks.keys().cloned().collect()
    }

    /// What gets sealed into a bind: every known mint with its marker, in a stable order.
    pub fn seal(&self) -> Vec<IssuerMintSeal> {
        self.marks
            .iter()
            .map(|(url, marker)| IssuerMintSeal {
                url: url.clone(),
                marker: *marker,
            })
            .collect()
    }
}

/// The real-mint fence, class-aware: an issuer mint under an ADMITTING marker passes
/// unconditionally; every other mint answers to [`home::mint_allowed`] exactly as before.
///
/// This is the ONE place the class widens the fence, and it widens it only for a mint the seat
/// itself can stand behind — its own, from its config (sealed at accept). A mint a seller merely
/// declared, and a mint nobody declared, are fenced as they always were.
pub fn mint_admitted(mint_url: &str, allow_real_mints: bool, issuers: &IssuerMints) -> bool {
    issuers.admits(mint_url) || home::mint_allowed(mint_url, allow_real_mints)
}

/// Refuse a Lightning operation (`op`) that would run against an issuer mint. `Ok(())` for a
/// Lightning mint. The one sentence every such refusal carries, so an operator reading a `wallet
/// fund`, a `wallet melt`, or a hop refusal sees the same reason.
pub fn refuse_lightning_at_issuer(
    op: &str,
    mint_url: &str,
    class: MintClass,
) -> Result<(), String> {
    match class {
        MintClass::Lightning => Ok(()),
        MintClass::Issuer => Err(format!(
            "{op} refused: {mint_url} is an issuer mint with no Lightning — its tokens are only good \
             for hiring the seat that issued them; {ISSUER_HOP_REFUSAL}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issuer_mints_normalize_and_compare_like_the_planner() {
        let known = IssuerMints::none().with_own(Some("https://Issuer.example/Bitcoin/"));
        assert!(known.contains("https://issuer.example/Bitcoin"));
        assert!(known.contains("https://issuer.example/Bitcoin/"));
        assert!(!known.contains("https://other.example/Bitcoin"));
        assert!(!known.contains("not a url"));
        assert_eq!(
            known.class_of("https://issuer.example/Bitcoin"),
            MintClass::Issuer
        );
        assert_eq!(
            known.class_of("https://other.example/Bitcoin"),
            MintClass::Lightning
        );
        assert_eq!(
            known.urls(),
            vec!["https://issuer.example/Bitcoin".to_owned()]
        );
    }

    #[test]
    fn own_mint_is_added_only_when_stated_and_a_bad_url_is_dropped() {
        assert!(IssuerMints::none().with_own(None).is_empty());
        assert!(IssuerMints::none().with_own(Some("")).is_empty());
        assert!(
            IssuerMints::none()
                .with_own(Some("::not-a-url::"))
                .is_empty()
        );
        assert!(
            IssuerMints::none()
                .with_declared(Some("::not-a-url::"))
                .is_empty()
        );
        let own = IssuerMints::none().with_own(Some("http://127.0.0.1:3338"));
        assert!(own.contains("http://127.0.0.1:3338/"));
        assert_eq!(
            own.marker_of("http://127.0.0.1:3338/"),
            Some(IssuerMarker::Own)
        );
    }

    /// Both markers REFUSE (each is an issuer mint to the hop), but only the one the seat can stand
    /// behind — its own config — ADMITS. A seller's declaration is knowledge for the hop and
    /// nothing for the fence. Each half on its own.
    #[test]
    fn every_marker_refuses_but_only_own_admits() {
        // Ad-declared: refuses, does not admit.
        let declared = IssuerMints::none().with_declared(Some("https://issuer.example"));
        assert!(declared.contains("https://issuer.example"));
        assert!(!declared.admits("https://issuer.example"));
        assert_eq!(
            declared.marker_of("https://issuer.example"),
            Some(IssuerMarker::Declared)
        );
        assert!(!IssuerMarker::Declared.admits());

        // Config-declared: refuses and admits.
        let own = IssuerMints::none().with_own(Some("https://issuer.example"));
        assert!(own.contains("https://issuer.example") && own.admits("https://issuer.example"));
        assert_eq!(
            own.marker_of("https://issuer.example"),
            Some(IssuerMarker::Own)
        );
        assert!(IssuerMarker::Own.admits());

        // Unknown: neither.
        assert!(!IssuerMints::none().contains("https://issuer.example"));
        assert!(!IssuerMints::none().admits("https://issuer.example"));
    }

    /// One URL, two markers: the admitting one stands whichever order they arrive in. A seller's
    /// declaration can never downgrade what the seat itself stated.
    #[test]
    fn a_declaration_never_downgrades_an_admitting_marker() {
        let own_then_declared = IssuerMints::none()
            .with_own(Some("https://issuer.example"))
            .with_declared(Some("https://issuer.example/"));
        assert_eq!(
            own_then_declared.marker_of("https://issuer.example"),
            Some(IssuerMarker::Own)
        );
        let declared_then_own = IssuerMints::none()
            .with_declared(Some("https://issuer.example"))
            .with_own(Some("https://issuer.example"));
        assert_eq!(
            declared_then_own.marker_of("https://issuer.example"),
            Some(IssuerMarker::Own)
        );
        assert_eq!(
            declared_then_own.seal().len(),
            1,
            "one URL is one sealed fact"
        );
    }

    /// The seal round-trips: what accept knew, marker by marker, is what pay rebuilds — and the
    /// JSON shape is stable, because it lives in every accept-bind on disk.
    #[test]
    fn the_seal_round_trips_with_its_markers() {
        let known = IssuerMints::none()
            .with_own(Some("http://127.0.0.1:3338"))
            .with_declared(Some("https://declared.example"));
        let seal = known.seal();
        assert_eq!(seal.len(), 2);
        assert_eq!(IssuerMints::from_seal(&seal), known);

        let json = serde_json::to_string(&seal).expect("serializes");
        assert!(json.contains(r#""marker":"declared""#), "{json}");
        assert!(json.contains(r#""marker":"own""#), "{json}");
        assert!(!json.contains(r#""marker":"info""#), "{json}");
        let back: Vec<IssuerMintSeal> = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(IssuerMints::from_seal(&back), known);
        // Rebuilt, the fence and the hop answer as they did at accept.
        let rebuilt = IssuerMints::from_seal(&back);
        assert!(rebuilt.admits("http://127.0.0.1:3338"));
        assert!(
            rebuilt.contains("https://declared.example")
                && !rebuilt.admits("https://declared.example")
        );
    }

    /// A seal written BEFORE the class became declaration-only carried a third marker, `info` (a
    /// class inferred from the mint's own NUT-06 document). Such a bind must still load: the entry
    /// reads back as a declaration — the hop still refuses it — and it admits nothing, because no
    /// operator ever stated it. It is written back as `declared`, never as `info`.
    #[test]
    fn a_legacy_info_seal_still_reads_as_a_declaration_and_admits_nothing() {
        let legacy = r#"[
            {"url":"https://info.example","marker":"info"},
            {"url":"http://127.0.0.1:3338","marker":"own"},
            {"url":"https://declared.example","marker":"declared"}
        ]"#;
        let seal: Vec<IssuerMintSeal> = serde_json::from_str(legacy).expect("a legacy seal loads");
        assert_eq!(seal.len(), 3);
        assert_eq!(seal[0].marker, IssuerMarker::Declared);
        assert_eq!(seal[1].marker, IssuerMarker::Own);
        assert_eq!(seal[2].marker, IssuerMarker::Declared);

        let rebuilt = IssuerMints::from_seal(&seal);
        assert_eq!(rebuilt.class_of("https://info.example"), MintClass::Issuer);
        assert!(
            !rebuilt.admits("https://info.example"),
            "a class nobody declared never widens the fence"
        );
        assert!(!mint_admitted("https://info.example", false, &rebuilt));
        assert!(rebuilt.admits("http://127.0.0.1:3338"));

        let rewritten = serde_json::to_string(&rebuilt.seal()).expect("serializes");
        assert!(!rewritten.contains(r#""marker":"info""#), "{rewritten}");
        assert!(rewritten.contains(r#""marker":"declared""#), "{rewritten}");
        assert!(
            serde_json::from_str::<IssuerMarker>(r#""sniffed""#).is_err(),
            "only the named markers read"
        );
    }

    /// The fence: an issuer mint this seat declared passes regardless of `allow_real_mints` and of
    /// scheme; every other mint answers to the unchanged `home::mint_allowed`.
    #[test]
    fn the_fence_admits_a_known_issuer_mint_and_nothing_else_new() {
        let sidecar = "http://127.0.0.1:3338";
        let real = "https://mint.minibits.cash/Bitcoin";
        let known = IssuerMints::none().with_own(Some(sidecar));

        // Issuer: admitted with the fence closed, and over plain http.
        assert!(mint_admitted(sidecar, false, &known));
        assert!(mint_admitted(sidecar, true, &known));
        // Unknown http URL: refused as before, even with the real-money switch on.
        assert!(!mint_admitted(sidecar, true, &IssuerMints::none()));
        assert!(!mint_admitted(sidecar, false, &IssuerMints::none()));
        // A real mint still answers to the switch alone — knowing an issuer changes nothing for it.
        assert!(!mint_admitted(real, false, &known));
        assert!(mint_admitted(real, true, &known));
        // The dev allow-list entry still passes with the switch off.
        assert!(mint_admitted(home::DEFAULT_MINT_URL, false, &known));
    }

    /// NEGATIVE: a mint nobody declared is fenced EXACTLY as `home::mint_allowed` fences it, for
    /// every URL and either switch setting — including a mint that is unreachable, one that is
    /// malformed, and one that would answer "no bolt11" if asked. None of that can matter, because
    /// nothing asks: the class comes from a declaration or it does not exist.
    #[test]
    fn an_undeclared_mint_is_fenced_exactly_as_before_whatever_it_is() {
        let nobody = IssuerMints::none();
        let candidates = [
            "https://mint.minibits.cash/Bitcoin",
            home::DEFAULT_MINT_URL,
            "http://127.0.0.1:1",            // nothing listens here
            "http://10.255.255.1:3338",      // unroutable
            "https://no-such-host.invalid/", // does not resolve
            "::not-a-url::",                 // malformed
            "",
        ];
        for url in candidates {
            for allow in [false, true] {
                assert_eq!(
                    mint_admitted(url, allow, &nobody),
                    home::mint_allowed(url, allow),
                    "undeclared {url:?} with allow_real_mints={allow}"
                );
            }
            assert_eq!(nobody.class_of(url), MintClass::Lightning, "{url:?}");
            assert_eq!(nobody.marker_of(url), None, "{url:?}");
        }
    }

    /// NEGATIVE: a seller DECLARING a mint its issuer mint opens nothing. A real mint so declared is
    /// fenced exactly as an undeclared one — the switch alone decides — and the seller's sidecar so
    /// declared stays fenced too, unless this seat's own config names it.
    #[test]
    fn a_sellers_declaration_does_not_widen_the_fence() {
        let real = "https://mint.minibits.cash/Bitcoin";
        let sidecar = "http://10.0.0.7:3338";
        let declared = IssuerMints::none()
            .with_declared(Some(real))
            .with_declared(Some(sidecar));
        assert!(
            !mint_admitted(real, false, &declared),
            "a declared real mint is still fenced"
        );
        assert!(
            mint_admitted(real, true, &declared),
            "...and still opt-in-able, as before"
        );
        assert!(!mint_admitted(sidecar, false, &declared));
        assert!(
            !mint_admitted(sidecar, true, &declared),
            "http is not https; a seller's declaration is not this seat's"
        );
    }

    #[test]
    fn a_lightning_op_at_an_issuer_mint_is_refused_with_the_plain_reason() {
        assert_eq!(
            refuse_lightning_at_issuer(
                "wallet fund",
                "http://127.0.0.1:3338",
                MintClass::Lightning
            ),
            Ok(())
        );
        let refusal =
            refuse_lightning_at_issuer("wallet fund", "http://127.0.0.1:3338", MintClass::Issuer)
                .expect_err("an issuer mint refuses");
        assert!(refusal.contains("wallet fund refused"), "{refusal}");
        assert!(refusal.contains("http://127.0.0.1:3338"), "{refusal}");
        assert!(refusal.contains(ISSUER_HOP_REFUSAL), "{refusal}");
    }
}
