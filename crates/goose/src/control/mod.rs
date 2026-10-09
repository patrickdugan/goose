//! Control gate: propose every tool call to an external control module before it runs.
//!
//! Enabled by `GOOSE_CONTROL_CONFIG`, the path of a `control.goose_hook_config.v1`
//! JSON file (the same file the no-fork `PreToolUse` hook reads). When it is set,
//! every tool call that reaches `ExtensionManager::dispatch_tool_call_inner`, plus
//! the calls goose executes itself (`recipe__final_output`, the state machine's
//! `load_skill`), is sent to the control module's `/v1/decide` and runs only on
//! `allow`. Anything else blocks: a refusal, an unreachable control module, or an
//! unusable config. `GOOSE_CONTROL_REQUIRED=1` makes a missing config a denial of
//! every call, and refuses providers that execute tools themselves.
//!
//! Unlike the hook, this point is also reached by tool calls made from inside
//! code mode and by subagents, which run with hooks disabled. A subagent is
//! proposed as its own actor, `<root agent>/<session id>`, registered with the
//! control module by a `delegation_spawned` event on its first call. Calls to
//! the tool gateway carry that actor id in `_meta.agentId`, so the gateway
//! fingerprints them the same way and finds the lease.
//!
//! The wire contract (proposal shape, canonical JSON, fingerprint) is the one in
//! Goose-Experiments `src/runtime_control/wire.py`; `canonical_json` must match
//! Python's `json.dumps(sort_keys=True, separators=(",", ":"), ensure_ascii=True)`
//! byte for byte, which the golden-vector tests below pin.

use once_cell::sync::OnceCell;
use rmcp::model::{ErrorCode, ErrorData};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Duration;

pub const CONFIG_ENV: &str = "GOOSE_CONTROL_CONFIG";
pub const REQUIRED_ENV: &str = "GOOSE_CONTROL_REQUIRED";
const CONFIG_SCHEMA: &str = "control.goose_hook_config.v1";
const PROPOSAL_SCHEMA: &str = "control.tool_call_proposal.v1";
const PROVENANCE_SCHEMA: &str = "control.provenance.v1";
/// Providers that run tools inside their own agent, out of goose's sight.
const SELF_EXECUTING_PROVIDERS: &[&str] = &[
    "claude-code",
    "claude_code",
    "gemini-cli",
    "gemini_cli",
    "codex",
    "cursor-agent",
];

/// Where in goose a tool call was dispatched from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    AgentLoop,
    CodeMode,
    AppCall,
    FinalOutput,
    SkillLoad,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Surface::AgentLoop => "agent_loop",
            Surface::CodeMode => "code_mode",
            Surface::AppCall => "app_call",
            Surface::FinalOutput => "final_output",
            Surface::SkillLoad => "skill_load",
        }
    }
}

/// One tool call as the gate sees it.
pub struct CallSite<'a> {
    pub session_id: &'a str,
    pub parent_session_id: Option<&'a str>,
    pub is_subagent: bool,
    pub request_id: Option<&'a str>,
    /// The name the model used, e.g. `gateway__ledger_pay` or `shell`.
    pub native_name: &'a str,
    /// The owning extension and the extension's own tool name, when resolved.
    pub extension: Option<&'a str>,
    pub actual_tool: &'a str,
    pub args: Option<&'a Map<String, Value>>,
    pub surface: Surface,
}

/// An allowed call. `gateway_agent` is set when the call goes to the tool gateway.
#[derive(Debug, Default)]
pub struct Allowed {
    pub gateway_agent: Option<String>,
}

struct Gate {
    control_url: String,
    shim_token: String,
    run_id: String,
    contract_hash: String,
    registry_hash: String,
    agent_id: String,
    runtime: Value,
    gateway_extension: String,
    registry_tools: HashSet<String>,
    wait_ms: u64,
    client: reqwest::Client,
    registered_children: Mutex<HashSet<String>>,
}

enum GateState {
    Disabled,
    Active(Box<Gate>),
    Broken(String),
}

static GATE: OnceCell<GateState> = OnceCell::new();

fn state() -> &'static GateState {
    GATE.get_or_init(load)
}

fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes")
    )
}

fn load() -> GateState {
    let required = env_flag(REQUIRED_ENV);
    let Ok(path) = std::env::var(CONFIG_ENV) else {
        return if required {
            GateState::Broken(format!("{REQUIRED_ENV} is set but {CONFIG_ENV} is not"))
        } else {
            GateState::Disabled
        };
    };
    // A config that was named but cannot be used blocks everything, required or not.
    match Gate::from_file(&path) {
        Ok(gate) => GateState::Active(Box::new(gate)),
        Err(error) => GateState::Broken(format!("control config {path} is unusable: {error}")),
    }
}

/// True when the gate will be consulted (active, or broken and therefore denying).
pub fn enabled() -> bool {
    !matches!(state(), GateState::Disabled)
}

fn deny(message: impl Into<String>) -> ErrorData {
    ErrorData::new(
        ErrorCode::INTERNAL_ERROR,
        format!(
            "Tool call denied by control gate: {}. Do not retry; this is a policy denial, not a transient failure.",
            message.into()
        ),
        None,
    )
}

/// Ask the control module about one call. `Ok` means run it.
pub async fn check(site: CallSite<'_>) -> Result<Allowed, ErrorData> {
    match state() {
        GateState::Disabled => Ok(Allowed::default()),
        GateState::Broken(reason) => Err(deny(format!("control: {reason}"))),
        GateState::Active(gate) => gate.decide(&site).await,
    }
}

/// Gate a call goose executes itself (final output, a state-machine skill load).
pub async fn check_session_call(
    session: &crate::session::Session,
    request_id: &str,
    tool_name: &str,
    args: Option<&Map<String, Value>>,
    surface: Surface,
) -> Result<(), ErrorData> {
    check(CallSite {
        session_id: &session.id,
        parent_session_id: session.parent_session_id.as_deref(),
        is_subagent: session.session_type == crate::session::SessionType::SubAgent,
        request_id: Some(request_id),
        native_name: tool_name,
        extension: None,
        actual_tool: tool_name,
        args,
        surface,
    })
    .await
    .map(|_| ())
}

/// Refuse providers that execute tools inside their own agent while control is on.
pub fn check_provider(name: &str, manages_own_context: bool) -> anyhow::Result<()> {
    if !enabled() {
        return Ok(());
    }
    let self_executing = manages_own_context
        || name.ends_with("_acp")
        || name.ends_with("-acp")
        || SELF_EXECUTING_PROVIDERS.contains(&name);
    if self_executing {
        anyhow::bail!("provider `{name}` runs tools itself, outside the control gate; refused while {CONFIG_ENV} or {REQUIRED_ENV} is set");
    }
    Ok(())
}

impl Gate {
    fn from_file(path: &str) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let config: Value = serde_json::from_str(&text)?;
        let field = |name: &str| -> anyhow::Result<String> {
            config
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("missing string field `{name}`"))
        };
        if config.get("schema").and_then(Value::as_str) != Some(CONFIG_SCHEMA) {
            anyhow::bail!("schema must be {CONFIG_SCHEMA}");
        }
        let registry_tools = config
            .get("registryTools")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("missing `registryTools`"))?
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        let control_url = field("controlUrl")?;
        let host_ok = control_url.starts_with("http://127.0.0.1:")
            || control_url.starts_with("http://localhost:")
            || control_url.starts_with("http://[::1]:");
        if !host_ok {
            anyhow::bail!("controlUrl must be loopback http");
        }
        let timeout_s = config.get("timeoutS").and_then(Value::as_u64).unwrap_or(20);
        let wait_ms = config.get("waitMs").and_then(Value::as_u64).unwrap_or(0);
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(timeout_s) + Duration::from_millis(wait_ms))
            .build()?;
        Ok(Self {
            control_url: control_url.trim_end_matches('/').to_string(),
            shim_token: field("shimToken")?,
            run_id: field("runId")?,
            contract_hash: field("contractHash")?,
            registry_hash: field("registryHash")?,
            agent_id: field("agentId")?,
            runtime: config.get("runtime").cloned().unwrap_or(Value::Null),
            gateway_extension: field("gatewayExtension")?,
            registry_tools,
            wait_ms,
            client,
            registered_children: Mutex::new(HashSet::new()),
        })
    }

    fn is_gateway(&self, site: &CallSite<'_>) -> bool {
        site.extension == Some(self.gateway_extension.as_str())
            && self.registry_tools.contains(site.actual_tool)
    }

    fn tool_id<'a>(&self, site: &'a CallSite<'a>) -> &'a str {
        if self.is_gateway(site) {
            site.actual_tool
        } else {
            site.native_name
        }
    }

    fn actor(&self, site: &CallSite<'_>) -> (String, Vec<String>) {
        if site.is_subagent {
            let mut lineage: Vec<String> = site
                .parent_session_id
                .map(str::to_string)
                .into_iter()
                .collect();
            lineage.push(site.session_id.to_string());
            (format!("{}/{}", self.agent_id, site.session_id), lineage)
        } else {
            (self.agent_id.clone(), vec![site.session_id.to_string()])
        }
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        let response = self
            .client
            .post(format!("{}{}", self.control_url, path))
            .bearer_auth(&self.shim_token)
            .json(body)
            .send()
            .await
            .map_err(|_| "control_unavailable".to_string())?;
        let status = response.status();
        let value: Value = response
            .json()
            .await
            .map_err(|_| "control_unavailable".to_string())?;
        if !status.is_success() {
            let code = value
                .pointer("/error/code")
                .and_then(Value::as_str)
                .unwrap_or("control_error")
                .to_string();
            return Err(code);
        }
        Ok(value)
    }

    async fn register_child(&self, agent: &str, lineage: &[String]) -> Result<(), String> {
        if self
            .registered_children
            .lock()
            .map(|set| set.contains(agent))
            .unwrap_or(false)
        {
            return Ok(());
        }
        let payload = json!({
            "kind": "delegation_spawned",
            "payload": {
                "childAgentId": agent,
                "lineage": lineage,
                "parentAgentId": self.agent_id,
            },
        });
        match self.post("/v1/events", &payload).await {
            Ok(_) => {}
            Err(code) if code == "duplicate_actor" => {}
            Err(code) => return Err(code),
        }
        if let Ok(mut set) = self.registered_children.lock() {
            set.insert(agent.to_string());
        }
        Ok(())
    }

    async fn decide(&self, site: &CallSite<'_>) -> Result<Allowed, ErrorData> {
        let (agent, lineage) = self.actor(site);
        if site.is_subagent {
            self.register_child(&agent, &lineage)
                .await
                .map_err(|code| deny(format!("control: {code}")))?;
        }
        let tool_id = self.tool_id(site);
        let args = Value::Object(site.args.cloned().unwrap_or_default());
        let fingerprint = fingerprint(&self.run_id, &agent, tool_id, &args);
        let surface = if site.is_subagent && site.surface == Surface::AgentLoop {
            "subagent"
        } else {
            site.surface.as_str()
        };
        let mut provenance = json!({
            "schema": PROVENANCE_SCHEMA,
            "attestation": "shim",
            "contextTaint": [],
            "surface": surface,
        });
        if let Some(request_id) = site.request_id.filter(|id| !id.is_empty()) {
            provenance["nativeCallId"] = Value::String(request_id.to_string());
        }
        let proposal = json!({
            "schema": PROPOSAL_SCHEMA,
            "actor": {"agentId": agent, "lineage": lineage},
            "args": args,
            "contractHash": self.contract_hash,
            "fingerprint": fingerprint,
            "proposedAt": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3f+00:00").to_string(),
            "provenance": provenance,
            "runId": self.run_id,
            "runtime": self.runtime,
            "seq": chrono::Utc::now().timestamp_micros().max(0),
            "tool": {"id": tool_id, "nativeName": site.native_name, "registryHash": self.registry_hash},
        });
        let decision = self
            .post(
                "/v1/decide",
                &json!({"proposal": proposal, "waitMs": self.wait_ms}),
            )
            .await
            .map_err(|code| deny(format!("control: {code}")))?;
        let verdict = decision
            .get("verdict")
            .and_then(Value::as_str)
            .unwrap_or("deny");
        if verdict == "allow" {
            return Ok(Allowed {
                gateway_agent: self.is_gateway(site).then_some(agent),
            });
        }
        let codes: Vec<&str> = decision
            .get("reasonCodes")
            .and_then(Value::as_array)
            .map(|codes| codes.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let reasons = if codes.is_empty() {
            "refused".to_string()
        } else {
            codes.join(",")
        };
        Err(deny(format!("control: {verdict} {reasons}")))
    }
}

/// `sha256(canonical({"agentId", "args", "runId", "toolId"}))`, as in wire.py.
pub fn fingerprint(run_id: &str, agent_id: &str, tool_id: &str, args: &Value) -> String {
    let material = json!({"agentId": agent_id, "args": args, "runId": run_id, "toolId": tool_id});
    let digest = Sha256::digest(canonical_json(&material).as_bytes());
    digest
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Python's `json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True)`.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_canonical(&map[key], out);
            }
            out.push('}');
        }
    }
}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ' '..='~' => out.push(ch),
            _ => {
                let mut units = [0u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_matches_the_python_golden_vector() {
        let args = json!({"kind": "sandbox_trade", "amount_units": 10});
        assert_eq!(
            canonical_json(
                &json!({"agentId": "root", "args": args, "runId": "run-1", "toolId": "treasury_propose"})
            ),
            r#"{"agentId":"root","args":{"amount_units":10,"kind":"sandbox_trade"},"runId":"run-1","toolId":"treasury_propose"}"#
        );
        assert_eq!(
            fingerprint("run-1", "root", "treasury_propose", &args),
            "bf34de40d12f95f57f51238697cf776e52bf5ddf859d48be75b0203d0f70118d"
        );
    }

    #[test]
    fn strings_escape_like_python_ensure_ascii() {
        assert_eq!(
            canonical_json(&json!({"to": "zoë"})),
            r#"{"to":"zo\u00eb"}"#
        );
        assert_eq!(
            canonical_json(&json!("a\"b\\c\n\u{7f}\u{1}")),
            r#""a\"b\\c\n\u007f\u0001""#
        );
        assert_eq!(canonical_json(&json!("🙂")), r#""\ud83d\ude42""#);
        assert_eq!(
            canonical_json(&json!({"b": [1, true, null], "a": {}})),
            r#"{"a":{},"b":[1,true,null]}"#
        );
    }

    #[test]
    fn self_executing_providers_are_named() {
        assert!(SELF_EXECUTING_PROVIDERS.contains(&"claude-code"));
    }
}
