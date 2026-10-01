//! Generate Xray outbound snippets for 3x-ui.
//!
//! Each local fanout port becomes one socks outbound on 127.0.0.1, ready to
//! paste into 3x-ui → 设置 → Xray 配置 (outbounds), plus routing-rule
//! examples and a full template for panels whose DB has no template yet.

use serde_json::{json, Value};

use crate::models::PortEntry;

pub fn build(entries: &[PortEntry], prefix: &str) -> Value {
    let tag = |p: u16| format!("{prefix}-{p}");

    let outbounds: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "tag": tag(e.port),
                "protocol": "socks",
                "settings": {
                    "servers": [ { "address": "127.0.0.1", "port": e.port } ]
                }
            })
        })
        .collect();

    let rules_example: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "type": "field",
                "inboundTag": ["<你的入站tag>"],
                "outboundTag": tag(e.port)
            })
        })
        .collect();

    let mut full = default_template();
    if let Some(arr) = full["outbounds"].as_array_mut() {
        arr.extend(outbounds.iter().cloned());
    }
    if let Some(rules) = full["routing"]["rules"].as_array_mut() {
        // our rules go right after the panel api rule so they win over defaults
        let api_rule = rules.iter().cloned().collect::<Vec<_>>();
        rules.clear();
        for r in api_rule {
            rules.push(r);
            break; // keep only the first (api) rule before ours
        }
        for e in entries {
            rules.push(json!({
                "type": "field",
                "inboundTag": [format!("<你的入站tag-路由到{}>", tag(e.port))],
                "outboundTag": tag(e.port)
            }));
        }
    }

    json!({
        "prefix": prefix,
        "ports": entries.iter().map(|e| e.port).collect::<Vec<_>>(),
        "outbounds": outbounds,
        "rules_example": rules_example,
        "full_template": full,
        "usage": "把 outbounds 合并进 3x-ui 面板 设置→Xray配置 的 outbounds 数组；需要分流时把 rules_example 里的 inboundTag 换成你的入站 tag 后加入 routing.rules。保存后重启 Xray。"
    })
}

/// A conservative 3x-ui-style base template. The panel injects its inbounds
/// on top of this, so `inbounds` is intentionally empty.
fn default_template() -> Value {
    json!({
        "log": {
            "access": "./access.log",
            "error": "./error.log",
            "loglevel": "warning"
        },
        "routing": {
            "domainStrategy": "AsIs",
            "rules": [
                { "type": "field", "inboundTag": ["api"], "outboundTag": "api" }
            ]
        },
        "inbounds": [],
        "outbounds": [
            { "protocol": "freedom", "tag": "direct" },
            { "protocol": "blackhole", "tag": "blocked" }
        ],
        "policy": {
            "levels": {
                "0": { "handshake": 4, "connIdle": 300, "uplinkOnly": 1, "downlinkOnly": 1 }
            }
        },
        "other": {}
    })
}
