//! Advertised compatibility is evidence, not proof of execution or future availability.
use serde_json::{Value, json};

pub fn assess(info: &Value) -> Value {
    let caps = &info["maxplayer_mint"];
    // Only a block this tool recognises (an object with integer schema 1) can prove that a
    // required capability is missing. Any other shape, including a newer schema, is unknown:
    // the mint may be newer than this tool, so it warns instead of refusing.
    let recognised = caps.is_object() && caps["schema"].as_u64() == Some(1);
    let unrecognised = !caps.is_null() && !recognised;
    let mut missing = Vec::new();
    if recognised {
        if !caps["envelope_versions"]
            .as_array()
            .is_some_and(|v| v.contains(&json!(1)))
        {
            missing.push("envelope v1");
        }
        for op in [
            "info",
            "keys",
            "keyset",
            "keysets",
            "swap",
            "checkstate",
            "restore",
        ] {
            if !caps["holder_ops"]
                .as_array()
                .is_some_and(|v| v.contains(&json!(op)))
            {
                missing.push(op);
            }
        }
        if caps["durable_replay"] != true {
            missing.push("durable replay");
        }
        if caps["expiry_rule"] != "now_gte_exp" {
            missing.push("request expiry enforcement");
        }
    }
    let status = if !missing.is_empty() {
        "incompatible"
    } else if !recognised || info["version"].as_str().is_none() {
        "unknown"
    } else {
        "supported_advertised"
    };
    json!({"status":status,"version":safe_label(&info["version"]),
    "implementation":safe_label(&caps["implementation"]),"build":safe_label(&caps["build"]),"missing":missing,
    "message":match status {
        "incompatible" => "Trade not started: upgrade the mint sidecar and repeat preflight",
        "unknown" if unrecognised => "WARNING: compatibility unknown; the mint advertises a capability schema this tool does not recognise (this tool may need upgrading)",
        "unknown" => "WARNING: compatibility unknown; ask the operator for the sidecar build or upgrade its diagnostics",
        _ => "Required capabilities advertised; witness emission and future availability are not proven"
    }})
}
fn safe_label(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| {
        s.len() <= 128
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"./_-+".contains(&b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn current() -> Value {
        json!({"version":"maxplayer-mint/0.5.11","maxplayer_mint":{"schema":1,"envelope_versions":[1],"holder_ops":["info","keys","keyset","keysets","swap","checkstate","restore"],"durable_replay":true,"expiry_rule":"now_gte_exp"}})
    }
    #[test]
    fn unknown_is_not_outdated_and_missing_restore_is_incompatible() {
        assert_eq!(assess(&json!({"version":null}))["status"], "unknown");
        assert_eq!(assess(&current())["status"], "supported_advertised");
        let mut missing = current();
        missing["maxplayer_mint"]["holder_ops"]
            .as_array_mut()
            .unwrap()
            .pop();
        assert_eq!(assess(&missing)["status"], "incompatible");
        // Not recognised: never proof of incompatibility, even when fields are missing.
        for caps in [
            json!({"schema":2,"envelope_versions":[1],"holder_ops":["info"]}),
            json!({"schema":2}),
            json!({"schema":1.0,"envelope_versions":[1]}),
            json!({"schema":"1"}),
            json!({}),
            json!("x"),
            json!([1]),
        ] {
            let mut info = current();
            info["maxplayer_mint"] = caps.clone();
            let v = assess(&info);
            assert_eq!(v["status"], "unknown", "{caps}");
            assert!(
                v["message"]
                    .as_str()
                    .unwrap()
                    .contains("may need upgrading")
            );
        }
        let mut newer = current();
        newer["maxplayer_mint"]["schema"] = 2.into();
        assert_eq!(assess(&newer)["status"], "unknown", "newer schema warns");
        // Recognised schema 1 missing a field stays incompatible.
        let mut no_replay = current();
        no_replay["maxplayer_mint"]
            .as_object_mut()
            .unwrap()
            .remove("durable_replay");
        assert_eq!(assess(&no_replay)["status"], "incompatible");
        for version in [
            "maxplayer/0.6.1-rc9",
            "other/999.0",
            "maxplayer-mint/0.0.1-alpha",
        ] {
            let mut info = current();
            info["version"] = version.into();
            assert_eq!(
                assess(&info)["status"],
                "supported_advertised",
                "no historic floor exists"
            );
        }
    }
}
