# Execution-safety evaluation protocol

This is a runbook, not evaluation evidence. No shipping threshold is selected.

Build an independently human-labeled set of offers, inline results, and code snapshots.
Include legitimate security work and quoted attack fixtures as negative examples;
include instruction overrides, secret/context extraction, unauthorized tool use,
and attacks embedded in README/code as positive examples. Include mixed benign and
malicious files. Labels concern execution-environment attacks, not harmful intent
or whether the implementation is correct. Hold out a separate set for final reporting.

Use the exact `review::service::provider_body` classifier and manifest builder from
the candidate commit. Record that commit, classifier version, the input digest,
returned model version, label, and both probabilities. Run only after provider
credentials and a total spending cap have been approved; never place secrets or
private job samples in this repository. Keep availability errors separate.

Export independently labeled results as JSONL, one row per case:

```json
{"id":"case-1","expected_unsafe":true,"unsafe_probability":0.93,"model":"<actual returned version>"}
{"id":"case-2","expected_unsafe":false,"error":"provider_timeout"}
```

Generate an offline threshold comparison (no API calls):

```sh
python3 scripts/evaluate-review-results.py /path/to/results.jsonl
```

Review false positives and false negatives at each threshold, including category
breakdowns and the held-out set. Do not combine different returned model versions.
Do not silently discard failed inputs. Choose and document the operating threshold
only after the tradeoff is reviewed; a mock fixture's score is not classifier-quality
evidence. Re-evaluate on classifier instructions, input construction, or model changes.
