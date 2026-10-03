//! Generate Xray outbound snippets for 3x-ui.
//!
//! Each local fanout port becomes one socks outbound on 127.0.0.1, ready to
//! paste into 3x-ui → 设置 → Xray 配置 (outbounds), plus routing-rule
//! examples and a full template for panels whose DB has no template yet.

use serde_json::{json, Value};

use crate::models::PortEntry;

/// `mode`: "direct" = one inbound per port (fanout style);
///         "balancer" = a single inbound balanced over every fanout port
/// via an Xray observatory + balancer.
pub fn build_with(entries: &[PortEntry], prefix: &str, mode: &str, inbound: &str) -> Value {
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

    let balancer = mode == "balancer";
    let in_tag = if inbound.is_empty() {
        "<你的入站tag>".to_string()
    } else {
        inbound.to_string()
    };
    if let Some(rules) = full["routing"]["rules"].as_array_mut() {
        // panel api rule first, then ours
        let api_rule = rules.first().cloned();
        rules.clear();
        if let Some(r) = api_rule {
            rules.push(r);
        }
        if balancer {
            rules.push(json!({
                "type": "field",
                "inboundTag": [in_tag],
                "balancerTag": "resi-bal"
            }));
        } else {
            for e in entries {
                rules.push(json!({
                    "type": "field",
                    "inboundTag": [format!("{}-in-{}", prefix, e.port)],
                    "outboundTag": tag(e.port)
                }));
            }
        }
    }

    let mut extra = json!({});
    if balancer {
        full["observatory"] = json!({
            "subjectSelector": [format!("{prefix}-")],
            "probeURL": "http://www.gstatic.com/generate_204",
            "probeInterval": "5m",
            "enableConcurrency": true
        });
        full["routing"]["balancers"] = json!([{
            "tag": "resi-bal",
            "selector": [format!("{prefix}-")],
            "strategy": { "type": "leastPing" }
        }]);
        extra = json!({
            "observatory": full["observatory"],
            "balancers": full["routing"]["balancers"]
        });
    }

    json!({
        "prefix": prefix,
        "mode": mode,
        "ports": entries.iter().map(|e| e.port).collect::<Vec<_>>(),
        "outbounds": outbounds,
        "rules_example": rules_example,
        "balancer_extra": extra,
        "full_template": full,
        "usage": if balancer {
            "负载均衡模式：把 outbounds 合并进 Xray 配置，再把 observatory / routing.balancers 合并进去（full_template 已含全部），规则里的 inboundTag 改成你的入站 tag，即可让一个入站在所有住宅出口间轮换（leastPing）。"
        } else {
            "把 outbounds 合并进 3x-ui 面板 设置→Xray配置 的 outbounds 数组；需要分流时把 rules_example 里的 inboundTag 换成你的入站 tag 后加入 routing.rules。保存后重启 Xray。"
        }
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
