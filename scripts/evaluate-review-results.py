#!/usr/bin/env python3
"""Offline threshold report for independently labeled, recorded classifier responses.
No network, credentials, or automatic threshold selection. Input JSONL rows:
{"id":"case-1","expected_unsafe":true,"unsafe_probability":0.9,"model":"jev-..."}
Errors use {"id":...,"expected_unsafe":...,"error":"provider_timeout"} and are
counted separately, never treated as safe. Use held-out labels, not model labels.
"""
import argparse
import json
import math


def report(rows, thresholds):
    seen = set()
    completed = []
    errors = 0
    models = set()
    for row in rows:
        case_id = row.get("id")
        if not isinstance(case_id, str) or not case_id or case_id in seen:
            raise ValueError("missing or duplicate case id")
        seen.add(case_id)
        if type(row.get("expected_unsafe")) is not bool:
            raise ValueError("expected_unsafe must be an independent boolean label")
        if row.get("error"):
            errors += 1
            continue
        probability = row.get("unsafe_probability")
        if type(probability) not in (float, int) or not math.isfinite(probability) or not 0 <= probability <= 1:
            raise ValueError("invalid unsafe_probability")
        model = row.get("model")
        if not isinstance(model, str) or not model or model.endswith("latest"):
            raise ValueError("record the returned model identity, not a latest alias")
        models.add(model)
        completed.append(row)
    if not seen:
        raise ValueError("empty evaluation")
    if len(models) > 1:
        raise ValueError("evaluate different returned model versions separately")
    results = []
    for threshold in thresholds:
        if not math.isfinite(threshold) or not 0 <= threshold <= 1:
            raise ValueError("invalid threshold")
        tp = fp = tn = fn = 0
        for row in completed:
            rejected = row["unsafe_probability"] >= threshold
            unsafe = row["expected_unsafe"]
            tp += rejected and unsafe
            fp += rejected and not unsafe
            tn += not rejected and not unsafe
            fn += not rejected and unsafe
        results.append(dict(threshold=threshold, true_positive=tp, false_positive=fp,
                            true_negative=tn, false_negative=fn,
                            false_positive_rate=fp / (fp + tn) if fp + tn else None,
                            false_negative_rate=fn / (fn + tp) if fn + tp else None))
    return dict(cases=len(seen), completed=len(completed), errors=errors,
                models=sorted(models), thresholds=results,
                shipping_threshold_selected=False)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results")
    parser.add_argument("--threshold", type=float, action="append", default=None)
    args = parser.parse_args()
    try:
        with open(args.results, encoding="utf-8") as source:
            rows = [json.loads(line) for line in source if line.strip()]
        print(json.dumps(report(rows, args.threshold or [0.1, 0.25, 0.5, 0.75, 0.9]), indent=2))
    except (OSError, ValueError) as error:
        parser.error(str(error))
