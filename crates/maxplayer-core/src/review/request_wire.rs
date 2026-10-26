//! Compact review transport only. Persisted payment evidence keeps its original shape.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RequestWire {
    subject: Subject,
    offer: Event,
    // The exact original i-tag bytes, encoded once to avoid repeated JSON escaping.
    // When present, offer's i value is an empty placeholder, restored before validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    public_task_b64: Option<String>,
    task_envelope: Option<String>,
    delivery: Option<DeliveryWire>,
    // Session follow-up evidence (§9). Absent on every other request, so their bytes are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    followup: Option<FollowUpEvidence>,
}

// Updated reviewers accept old clients first; only then deploy compact writers.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum DeliveryWire {
    Legacy(pc::evidence::PrivateEvidence),
    Compact(Delivery),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Delivery {
    claim: Event,
    award: Event,
    result: Event,
    answer_envelope: Option<String>,
}

impl TryFrom<RequestWire> for Request {
    type Error = &'static str;
    fn try_from(mut w: RequestWire) -> Result<Self, Self::Error> {
        if let Some(encoded) = w.public_task_b64.take() {
            let bytes = STANDARD
                .decode(&encoded)
                .map_err(|_| "invalid encoded task")?;
            if STANDARD.encode(&bytes) != encoded {
                return Err("noncanonical encoded task");
            }
            let text = String::from_utf8(bytes).map_err(|_| "invalid encoded task")?;
            if w.task_envelope.is_some() {
                return Err("conflicting task representations");
            }
            replace_task_tag(&mut w.offer, Some(""), &text)?;
            w.offer.verify().map_err(|_| "invalid restored offer")?;
        }
        let delivery = match w.delivery {
            None => None,
            Some(DeliveryWire::Legacy(e)) => {
                if e.offer != w.offer || e.task_envelope != w.task_envelope {
                    return Err("inconsistent private review evidence");
                }
                Some(e)
            }
            Some(DeliveryWire::Compact(e)) => Some(pc::evidence::PrivateEvidence {
                offer: w.offer.clone(),
                task_envelope: w.task_envelope.clone(),
                claim: e.claim,
                award: e.award,
                result: e.result,
                answer_envelope: e.answer_envelope,
            }),
        };
        Ok(Self {
            subject: w.subject,
            offer: w.offer,
            task_envelope: w.task_envelope,
            delivery,
            followup: w.followup,
        })
    }
}

impl Serialize for Request {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let delivery = self
            .delivery
            .as_ref()
            .map(|e| {
                if e.offer != self.offer || e.task_envelope != self.task_envelope {
                    return Err(serde::ser::Error::custom(
                        "inconsistent private review evidence",
                    ));
                }
                Ok(DeliveryWire::Compact(Delivery {
                    claim: e.claim.clone(),
                    award: e.award.clone(),
                    result: e.result.clone(),
                    answer_envelope: e.answer_envelope.clone(),
                }))
            })
            .transpose()?;
        let mut offer = self.offer.clone();
        let public_task_b64 = offer
            .tags
            .iter()
            .find(|t| t.as_slice().first().map(String::as_str) == Some("i"))
            .and_then(|t| t.as_slice().get(1))
            .map(|text| STANDARD.encode(text.as_bytes()));
        if public_task_b64.is_some() {
            replace_task_tag(&mut offer, None, "").map_err(serde::ser::Error::custom)?;
        }
        RequestWire {
            subject: self.subject.clone(),
            offer,
            public_task_b64,
            task_envelope: self.task_envelope.clone(),
            delivery,
            followup: self.followup.clone(),
        }
        .serialize(serializer)
    }
}

fn replace_task_tag(
    event: &mut Event,
    expected: Option<&str>,
    replacement: &str,
) -> Result<(), &'static str> {
    let mut found = false;
    let tags = event
        .tags
        .iter()
        .map(|tag| {
            if tag.as_slice().first().map(String::as_str) != Some("i") {
                return Ok(tag.clone());
            }
            if found
                || tag.as_slice().len() != 2
                || expected.is_some_and(|s| tag.as_slice()[1] != s)
            {
                return Err("ambiguous encoded task");
            }
            found = true;
            Tag::parse(["i", replacement]).map_err(|_| "invalid encoded task tag")
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !found {
        return Err("missing encoded task tag");
    }
    event.tags = Tags::from_list(tags);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_review_deduplicates_delivery_and_accepts_legacy() {
        for targeted in [false, true] {
            for delivery in [false, true] {
                let (request, policy) = super::super::tests::fixture(targeted, delivery);
                let legacy = serde_json::json!({"subject": request.subject, "offer": request.offer,
                    "task_envelope": request.task_envelope, "delivery": request.delivery});
                let old: Request = serde_json::from_value(legacy.clone()).unwrap();
                let compact = serde_json::to_value(&old).unwrap();
                if delivery {
                    assert!(compact["delivery"].get("offer").is_none());
                    assert!(compact["delivery"].get("task_envelope").is_none());
                    assert!(compact.to_string().len() < legacy.to_string().len());
                }
                let restored: Request = serde_json::from_value(compact).unwrap();
                assert!(restored.delivery == request.delivery);
                restored.validate(&request.offer.pubkey, &policy).unwrap();
                if delivery {
                    let mut bad = legacy.clone();
                    bad["delivery"]["task_envelope"] = serde_json::json!("different");
                    assert!(serde_json::from_value::<Request>(bad).is_err());
                }
            }
        }
    }
    #[tokio::test]
    async fn private_review_maximum_admitted_tasks_and_answers_roundtrip() {
        for targeted in [false, true] {
            // Exact serialized admission boundary, not a character-count estimate.
            let (small, _, _, _) = pc::evidence::inline_fixture_with_text(
                targeted,
                false,
                2_000_000_000,
                vec!["https://testnut.cashu.space".into()],
                "x",
                "x",
            );
            let overhead = if let Some(envelope) = small.task_envelope {
                pc::PreparedContent::decode(&envelope)
                    .unwrap()
                    .body_bytes()
                    .len()
                    - 1
            } else {
                small
                    .offer
                    .tags
                    .iter()
                    .find(|t| t.as_slice()[0] == "i")
                    .unwrap()
                    .as_slice()[1]
                    .len()
                    - 1
            };
            for unit in ["x", "\"", "é"] {
                let cost = serde_json::to_string(unit).unwrap().len() - 2;
                let task = unit.repeat((pc::MAX_BODY_BYTES - overhead) / cost);
                for answer_unit in ["a", "\"", "\n", "é"] {
                    let (evidence, _, buyer, policy) = pc::evidence::inline_fixture_with_text(
                        targeted,
                        false,
                        2_000_000_000,
                        vec!["https://testnut.cashu.space".into()],
                        &task,
                        &format!(
                            "x{}",
                            answer_unit
                                .repeat((pc::MAX_INLINE_ANSWER_BYTES - 1) / answer_unit.len())
                        ),
                    );
                    for delivery in [false, true] {
                        let request = Request {
                            subject: Subject {
                                offer: evidence.offer.id.to_hex(),
                                event: if delivery {
                                    evidence.result.id.to_hex()
                                } else {
                                    evidence.offer.id.to_hex()
                                },
                                kind: if delivery {
                                    JOB_RESULT_KIND
                                } else {
                                    JOB_OFFER_KIND
                                },
                                commit: None,
                            },
                            offer: evidence.offer.clone(),
                            task_envelope: evidence.task_envelope.clone(),
                            delivery: delivery.then_some(evidence.clone()),
                            followup: None,
                        };
                        let service = super::super::tests::key(3);
                        let draft = request.draft(&service.public_key().to_hex()).unwrap();
                        let inner = crate::gateway::nostr::event_builder(&draft)
                            .unwrap()
                            .sign_with_keys(&buyer)
                            .unwrap();
                        let outer = wrap(&buyer, service.public_key(), &inner).await.unwrap();
                        let decoded = unwrap(&service, &outer).await.unwrap();
                        let restored: Request = serde_json::from_str(&decoded.content).unwrap();
                        restored.validate(&buyer.public_key(), &policy).unwrap();
                        assert!(restored.delivery == request.delivery);
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn private_review_actual_encryption_boundary_reports_sizes_without_contents() {
        let buyer = super::super::tests::key(1);
        let service = super::super::tests::key(3);
        let (request, _) = super::super::tests::fixture(true, false);
        let mut draft = request.draft(&service.public_key().to_hex()).unwrap();
        let mut low = 1;
        let mut high = 60 * 1024;
        while low + 1 < high {
            let mid = (low + high) / 2;
            draft.content = "x".repeat(mid);
            let inner = crate::gateway::nostr::event_builder(&draft)
                .unwrap()
                .sign_with_keys(&buyer)
                .unwrap();
            if wrap(&buyer, service.public_key(), &inner).await.is_ok() {
                low = mid;
            } else {
                high = mid;
            }
        }
        assert!(low > 30 * 1024);
        draft.content = "x".repeat(high);
        let inner = crate::gateway::nostr::event_builder(&draft)
            .unwrap()
            .sign_with_keys(&buyer)
            .unwrap();
        let error = wrap(&buyer, service.public_key(), &inner)
            .await
            .unwrap_err();
        assert!(error.contains("limit 65408 bytes"), "{error}");
        assert!(super::super::permanent_error(&error));
        assert!(!error.contains("xxxx"));
    }
    #[test]
    fn private_review_encoded_open_task_cannot_change_signed_bytes() {
        let (request, _) = super::super::tests::fixture(false, true);
        let compact = serde_json::to_value(&request).unwrap();
        assert!(compact.get("public_task_b64").is_some());
        for change in ["payload", "placeholder", "unknown"] {
            let mut bad = compact.clone();
            match change {
                "payload" => {
                    bad["public_task_b64"] = serde_json::json!(STANDARD.encode(b"changed"))
                }
                "placeholder" => {
                    let tags = bad["offer"]["tags"].as_array_mut().unwrap();
                    tags.iter_mut().find(|tag| tag[0] == "i").unwrap()[1] =
                        serde_json::json!("not empty");
                }
                _ => bad["unexpected"] = serde_json::json!(true),
            }
            assert!(serde_json::from_value::<Request>(bad).is_err());
        }
    }

    #[test]
    fn private_review_admission_names_task_limit_and_preserves_old_answers() {
        let (evidence, _, _, _) = pc::evidence::inline_fixture();
        let original =
            pc::PreparedContent::decode(evidence.task_envelope.as_ref().unwrap()).unwrap();
        let mut body = original.body().clone();
        body.text = "x".into();
        let overhead = serde_json::to_vec(&body).unwrap().len() - 1;
        body.text = "x".repeat(pc::MAX_BODY_BYTES - overhead);
        assert!(pc::PreparedContent::new(body.clone()).is_ok());
        body.text.push('x');
        let error = pc::PreparedContent::new(body).err().unwrap().to_string();
        assert!(error.contains("16384-byte"));
        assert!(error.contains("input files"));

        let task_tag = serde_json::json!({"schema":"maxplayer.public-task.v2", "text":"x",
            "requested_output":"text/plain", "dispatch":{}})
        .to_string();
        let mut task: serde_json::Value = serde_json::from_str(&task_tag).unwrap();
        let overhead = task_tag.len() - 1;
        task["text"] = serde_json::json!("x".repeat(pc::MAX_BODY_BYTES - overhead));
        assert!(pc::wire::PublicTask::parse(&task.to_string(), &"33".repeat(32)).is_ok());
        task["text"] = serde_json::json!("x".repeat(pc::MAX_BODY_BYTES - overhead + 1));
        assert!(
            pc::wire::PublicTask::parse(&task.to_string(), &"33".repeat(32))
                .err()
                .unwrap()
                .to_string()
                .contains("16384-byte")
        );

        let original =
            pc::PreparedContent::decode(evidence.answer_envelope.as_ref().unwrap()).unwrap();
        let mut body = original.body().clone();
        body.text = "x".repeat(pc::MAX_INLINE_ANSWER_BYTES);
        assert!(pc::PreparedContent::new(body.clone()).is_ok());
        body.text.push('x');
        assert!(
            pc::PreparedContent::new(body.clone())
                .err()
                .unwrap()
                .to_string()
                .contains("4096-byte")
        );
        // Persisted envelopes predate the new writer limit. They still decode,
        // and any review still verifies their signed commitment normally.
        let old = serde_json::json!({"schema":pc::ENVELOPE_SCHEMA, "nonce":"11".repeat(32),
            "body_b64": STANDARD.encode(serde_json::to_vec(&body).unwrap())})
        .to_string();
        assert!(pc::PreparedContent::decode(&old).is_ok());
    }
}
