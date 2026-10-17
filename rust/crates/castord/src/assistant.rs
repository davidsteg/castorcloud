//! The tenant assistant (Hermes Agent): what is rendered for it. One Hermes profile per household
//! member, each with its own API key and its own token for the read-only
//! data tools, so a member's assistant only ever sees that member's data.
//! The outputs are held to tests/fixtures/render.json.

use sha2::{Digest, Sha256};

use crate::render::Service;

/// Per-member secrets, stored encrypted in "Secret" like all others.
pub fn member_key_name(member_id: &str) -> String {
    format!("ASSISTANT_KEY_{member_id}")
}

pub fn member_tools_token_name(member_id: &str) -> String {
    format!("ASSISTANT_TOOLS_{member_id}")
}

pub fn is_member_secret(key: &str) -> bool {
    key.starts_with("ASSISTANT_KEY_") || key.starts_with("ASSISTANT_TOOLS_")
}

/// Hermes refuses API keys shorter than 16 characters; 256 bits.
pub fn new_secret() -> Result<String, String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| format!("random secret: {e}"))?;
    Ok(hex::encode(b))
}

/// Profile names become URL segments (/p/<profile>/...).
pub fn profile_name(member_id: &str) -> Result<&str, String> {
    let b = member_id.as_bytes();
    let ok = (1..=64).contains(&b.len())
        && b.first().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-');
    if ok {
        Ok(member_id)
    } else {
        Err(format!("member id not usable as profile name: {member_id}"))
    }
}

/// Spendable USD per month for a booked AI credit (aiBudgetChf, aiBudgetUsd in
/// @pc/pricing): VAT and Stripe's percentage off, rounded down to the cent.
pub fn ai_budget_usd(ai_credit_chf: i64, usd_per_chf: f64) -> f64 {
    const VAT_RATE: f64 = 0.081;
    const STRIPE_PERCENT: f64 = 0.029 + 0.007;
    if ai_credit_chf <= 0 {
        return 0.0;
    }
    let credit = ai_credit_chf as f64;
    let net = credit / (1.0 + VAT_RATE) - credit * STRIPE_PERCENT;
    let chf = (net * 100.0).floor() / 100.0;
    (chf * usd_per_chf * 100.0).floor() / 100.0
}

const GATEWAY_URL: &str = "http://pc-node-llm-gateway:4000/v1";

/// Everything Hermes could do beyond talking and remembering: no shell,
/// files, browser or web. Its only tools are our read-only MCP servers.
const DISABLED_TOOLSETS: [&str; 23] = [
    "terminal",
    "file",
    "web",
    "search",
    "browser",
    "code_execution",
    "delegation",
    "cronjob",
    "image_gen",
    "video_gen",
    "vision",
    "video",
    "tts",
    "stt",
    "x_search",
    "skills",
    "todo",
    "clarify",
    "kanban",
    "computer_use",
    "homeassistant",
    "connections",
    "messaging",
];

pub struct Member {
    /// Member row id; also the Hermes profile name.
    pub id: String,
    pub email: String,
    pub authentik_sub: String,
    pub name: Option<String>,
    pub locale: String,
}

pub struct MemberSecrets {
    pub member: Member,
    pub api_key: String,
    pub tools_token: String,
}

fn base_config(mcp: &[&str]) -> Vec<String> {
    let mut toolsets = vec!["memory", "session_search"];
    toolsets.extend(mcp);
    let mut lines: Vec<String> = [
        "# Rendered by the Castor provisioner; changes are overwritten on the next update.",
        "model:",
        "  provider: castor",
        "  default: assistant",
        "providers:",
        "  castor:",
    ]
    .map(str::to_owned)
    .to_vec();
    lines.push(format!("    api: {GATEWAY_URL}"));
    lines.push("    api_key: ${LLM_API_KEY}".into());
    lines.push("platform_toolsets:".into());
    lines.push(format!("  api_server: [{}]", toolsets.join(", ")));
    lines.push("agent:".into());
    lines.push("  max_turns: 12".into());
    lines.push(format!("  disabled_toolsets: [{}]", DISABLED_TOOLSETS.join(", ")));
    lines.extend(
        [
            "memory:",
            "  memory_enabled: true",
            "  user_profile_enabled: true",
            "tools:",
            "  tool_search:",
            "    enabled: off",
        ]
        .map(str::to_owned),
    );
    if !mcp.is_empty() {
        lines.push("mcp_servers:".into());
        for s in mcp {
            lines.push(format!("  {s}:"));
            lines.push(format!("    url: http://assistant-tools:8080/mcp/{s}"));
            lines.extend(
                [
                    "    headers:",
                    "      Authorization: Bearer ${CASTOR_TOOLS_TOKEN}",
                    "    timeout: 60",
                    "    trust: untrusted",
                    "    tools:",
                    "      resources: false",
                    "      prompts: false",
                ]
                .map(str::to_owned),
            );
        }
    }
    lines
}

/// The default profile owns the API listener serving every member at /p/<member>/.
fn default_config() -> String {
    let mut l = base_config(&[]);
    l.extend(
        [
            "gateway:",
            "  multiplex_profiles: true",
            "  api_server:",
            "    enabled: true",
            "    host: 0.0.0.0",
            "    port: 8642",
            "",
        ]
        .map(str::to_owned),
    );
    l.join("\n")
}

fn profile_config(services: &[Service]) -> String {
    let keys: Vec<&str> = services.iter().map(|s| s.key()).collect();
    let mut l = base_config(&keys);
    l.push(String::new());
    l.join("\n")
}

fn env_file(vars: &[(&str, &str)]) -> Result<String, String> {
    let mut out = String::new();
    for (k, v) in vars {
        if v.contains('\n') || v.contains('\r') {
            return Err(format!("{k}: newline in an assistant env value"));
        }
        out.push_str(&format!("{k}={v}\n"));
    }
    Ok(out)
}

fn service_words(s: Service) -> &'static str {
    match s {
        Service::Photos => "photos and videos (Photos)",
        Service::Docs => "scanned documents (Documents)",
        Service::Places => "location history (Timeline)",
        Service::Flights => "flights (Flights)",
    }
}

/// The assistant's personality and rules (Hermes' SOUL.md).
fn soul(brand_name: &str, member: &Member, services: &[Service]) -> String {
    let language = match member.locale.as_str() {
        "de" => "German",
        "fr" => "French",
        "it" => "Italian",
        "en" => "English",
        _ => "the user's language",
    };
    let data = if services.is_empty() {
        "no data services yet".to_owned()
    } else {
        services
            .iter()
            .map(|s| service_words(*s))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let name = member.name.as_deref().filter(|n| !n.is_empty()).unwrap_or("the user");
    format!(
        "# {brand_name} assistant

You are the personal assistant inside {brand_name}, a private cloud hosted in
Switzerland. You help {name} find things in their own data:
{data}.

- Answer in {language} unless the user writes in another language.
- Use the tools to look things up; never invent dates, places, amounts or names.
  If the tools find nothing, say so.
- Ask for as little data as needed: search narrowly, read a document only when
  its excerpt is not enough.
- Every result has a link. Include the links that matter, so the user can open
  the photo, document, map or flight.
- You cannot change, delete, share or upload anything, and you have no internet
  access. Say so when asked.
- The data belongs to this user alone. Other household members have their own
  assistant; you cannot see their data.
"
    )
}

/// For the tools container: who each tools token belongs to (hashes only).
fn members_file(members: &[MemberSecrets]) -> Result<String, String> {
    let list: Vec<serde_json::Value> = members
        .iter()
        .map(|m| {
            let mut o = serde_json::Map::new();
            o.insert("id".into(), m.member.id.clone().into());
            o.insert("email".into(), m.member.email.clone().into());
            o.insert("sub".into(), m.member.authentik_sub.clone().into());
            if let Some(n) = m.member.name.as_deref().filter(|n| !n.is_empty()) {
                o.insert("name".into(), n.into());
            }
            let hash = hex::encode(Sha256::digest(m.tools_token.as_bytes()));
            o.insert("tokenSha256".into(), hash.into());
            serde_json::Value::Object(o)
        })
        .collect();
    let text = serde_json::to_string_pretty(&serde_json::json!({ "members": list })).map_err(|e| e.to_string())?;
    Ok(text + "\n")
}

/// Run by assistant-init before Hermes starts: installs the rendered config
/// and removes the profiles (and their memory) of members who left.
pub const SYNC_SCRIPT: &str = r#"#!/bin/sh
# Rendered by the Castor provisioner.
set -eu
src=/rendered/hermes
dst=/opt/data
mkdir -p "$dst/profiles"
cp "$src/config.yaml" "$src/.env" "$dst/"
for p in "$dst"/profiles/*/; do
  [ -d "$p" ] || continue
  name=$(basename "$p")
  [ -d "$src/profiles/$name" ] || rm -rf "$p"
done
for p in "$src"/profiles/*/; do
  [ -d "$p" ] || continue
  name=$(basename "$p")
  mkdir -p "$dst/profiles/$name"
  cp "$p/config.yaml" "$p/.env" "$p/SOUL.md" "$dst/profiles/$name/"
done
chown -R 10000:10000 "$dst"
"#;

pub struct FilesInput<'a> {
    pub brand_name: &'a str,
    pub services: &'a [Service],
    pub admin_key: &'a str,
    pub llm_key: &'a str,
    pub members: &'a [MemberSecrets],
}

/// Every file below <configDir>/assistant/, by relative path, in order.
pub fn files(o: &FilesInput) -> Result<Vec<(String, String)>, String> {
    let mut f = vec![
        ("sync.sh".to_owned(), SYNC_SCRIPT.to_owned()),
        ("members.json".to_owned(), members_file(o.members)?),
        ("hermes/config.yaml".to_owned(), default_config()),
        (
            "hermes/.env".to_owned(),
            env_file(&[("API_SERVER_KEY", o.admin_key), ("LLM_API_KEY", o.llm_key)])?,
        ),
    ];
    for m in o.members {
        let dir = format!("hermes/profiles/{}", profile_name(&m.member.id)?);
        f.push((format!("{dir}/config.yaml"), profile_config(o.services)));
        f.push((
            format!("{dir}/.env"),
            env_file(&[
                ("API_SERVER_KEY", &m.api_key),
                ("LLM_API_KEY", o.llm_key),
                ("CASTOR_TOOLS_TOKEN", &m.tools_token),
            ])?,
        ));
        f.push((format!("{dir}/SOUL.md"), soul(o.brand_name, &m.member, o.services)));
    }
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_names_and_budgets() {
        assert!(profile_name("cm1abc").is_ok());
        assert!(profile_name("-x").is_err() && profile_name("A").is_err() && profile_name("a/b").is_err());
        assert!(profile_name("").is_err() && profile_name(&"a".repeat(65)).is_err());
        assert!(is_member_secret("ASSISTANT_TOOLS_x") && !is_member_secret("ASSISTANT_ADMIN_KEY"));
        assert!((ai_budget_usd(0, 1.1) - 0.0).abs() < f64::EPSILON);
    }
}
