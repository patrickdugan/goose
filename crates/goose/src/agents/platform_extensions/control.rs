//! The control conveyor, as the model sees it.
//!
//! When the control gate's config names a `phaseSkill`, this extension shows the
//! model its current phase every turn and offers one tool, `phase_complete`, to
//! report the phase done. The control module, not the model and not goose, decides
//! which phase comes next, and a report naming any phase but the current one is
//! refused. The phase is the module's: it lives in the control ledger, so it
//! survives goose restarts and context compaction.

use crate::agents::extension::PlatformExtensionContext;
use crate::agents::mcp_client::{Error, McpClientTrait};
use crate::agents::tool_execution::ToolCallContext;
use anyhow::Result;
use async_trait::async_trait;
use indoc::indoc;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, InitializeResult, JsonObject, ListToolsResult,
    ServerCapabilities, Tool, ToolAnnotations,
};
use schemars::{schema_for, JsonSchema};
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub static EXTENSION_NAME: &str = "control";
pub const PHASE_COMPLETE_TOOL_NAME: &str = "phase_complete";

#[derive(Debug, Deserialize, JsonSchema)]
struct PhaseCompleteParams {
    /// The phase you are completing: the one named in your current control phase.
    phase: String,
}

pub struct ControlClient {
    info: InitializeResult,
    context: PlatformExtensionContext,
}

impl ControlClient {
    pub fn new(context: PlatformExtensionContext) -> Result<Self> {
        let info = InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new(EXTENSION_NAME.to_string(), "1.0.0".to_string())
                    .with_title("Control"),
            )
            .with_instructions(
                indoc! {r#"
                This session follows a phase contract kept by an external control module.
                Each phase allows only some kinds of tools; calls outside the current
                phase's tools are refused. When the work of the current phase is done,
                call phase_complete with that phase's name to move to the next phase.
            "#}
                .to_string(),
            );
        Ok(Self { info, context })
    }

    fn tools() -> Vec<Tool> {
        let schema = serde_json::to_value(schema_for!(PhaseCompleteParams))
            .expect("PhaseCompleteParams schema serializes");
        vec![Tool::new(
            PHASE_COMPLETE_TOOL_NAME.to_string(),
            "Report the current control phase complete. The control module moves you to the next phase of the contract.".to_string(),
            schema.as_object().cloned().unwrap_or_default(),
        )
        .annotate(ToolAnnotations::from_raw(
            Some("Complete phase".to_string()),
            Some(false),
            Some(false),
            Some(false),
            Some(false),
        ))]
    }

    async fn phase(
        &self,
        session_id: &str,
        op: &str,
        completed: Option<&str>,
    ) -> Result<Value, String> {
        let session = self
            .context
            .session_manager
            .get_session(session_id, false)
            .await
            .ok();
        crate::control::phase(session.as_ref(), session_id, op, completed).await
    }
}

/// `INSPECT (1 of 4, tools: search, read)`.
fn describe(view: &Value) -> Option<String> {
    let phase = view.get("phase")?.as_str()?;
    let phases: Vec<&str> = view
        .get("phases")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let position = phases.iter().position(|item| *item == phase)? + 1;
    let tools: Vec<&str> = view
        .get("toolProfile")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let tools = if tools.is_empty() {
        "none".to_string()
    } else {
        tools.join(", ")
    };
    Some(format!(
        "{phase} ({position} of {}, tools: {tools})",
        phases.len()
    ))
}

#[async_trait]
impl McpClientTrait for ControlClient {
    async fn list_tools(
        &self,
        _session_id: &str,
        _next_cursor: Option<String>,
        _cancellation_token: CancellationToken,
    ) -> Result<ListToolsResult, Error> {
        let tools = if crate::control::phase_skill().is_some() {
            Self::tools()
        } else {
            vec![]
        };
        Ok(ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        ctx: &ToolCallContext,
        name: &str,
        arguments: Option<JsonObject>,
        _cancellation_token: CancellationToken,
    ) -> Result<CallToolResult, Error> {
        if name != PHASE_COMPLETE_TOOL_NAME {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Unknown tool: {name}"
            ))]));
        }
        let params: PhaseCompleteParams =
            match serde_json::from_value(Value::Object(arguments.unwrap_or_default())) {
                Ok(params) => params,
                Err(error) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                        "Invalid arguments: {error}"
                    ))]))
                }
            };
        let completed = params.phase.as_str();
        Ok(
            match self
                .phase(&ctx.session_id, "complete", Some(completed))
                .await
            {
                Ok(view) => CallToolResult::success(vec![ContentBlock::text(format!(
                    "{completed} complete. Now in phase {}.",
                    describe(&view).unwrap_or_else(|| "unknown".to_string())
                ))]),
                Err(code) => CallToolResult::error(vec![ContentBlock::text(format!(
                    "Phase not completed: {code}. The current phase is unchanged."
                ))]),
            },
        )
    }

    fn get_info(&self) -> Option<&InitializeResult> {
        Some(&self.info)
    }

    async fn get_moim(&self, session_id: &str, _tools: &[Tool]) -> Option<String> {
        crate::control::phase_skill()?;
        let view = self.phase(session_id, "view", None).await.ok()?;
        Some(match describe(&view) {
            Some(phase) => format!(
                "Control phase: {phase}. Call control__{PHASE_COMPLETE_TOOL_NAME} when this phase's work is done.\n"
            ),
            None => "Control phase: not entered yet; it starts at your first tool call.\n"
                .to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::describe;
    use serde_json::json;

    #[test]
    fn describes_the_phase_its_position_and_its_tools() {
        let phases = ["INSPECT", "ACT", "VERIFY", "FINAL"];
        assert_eq!(
            describe(&json!({"phase": "ACT", "phases": phases, "toolProfile": ["run", "write"]}))
                .as_deref(),
            Some("ACT (2 of 4, tools: run, write)")
        );
        assert_eq!(
            describe(&json!({"phase": "VERIFY", "phases": phases, "toolProfile": []})).as_deref(),
            Some("VERIFY (3 of 4, tools: none)")
        );
        assert_eq!(describe(&json!({"phase": null, "phases": phases})), None);
    }
}
