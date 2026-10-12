//! Advertised compatibility is evidence, not proof of execution or future availability.
use serde_json::{Value, json};

pub fn assess(info: &Value) -> Value {
    let caps = &info["maxplayer_mint"];
    let mut missing = Vec::new();
    if !caps.is_null() {
        if caps["schema"] != 1 {
            missing.push("capability schema 1");
        }
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
    } else if caps.is_null() || info["version"].as_str().is_none() {
        "unknown"
    } else {
        "supported_advertised"
    };
    json!({"status":status,"version":safe_label(&info["version"]),
    "implementation":safe_label(&caps["implementation"]),"build":safe_label(&caps["build"]),"missing":missing,
    "message":match status {
        "incompatible" => "Trade not started: upgrade the mint sidecar and repeat preflight",
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
