//! The control module's controller, heard inside the state machine.
//!
//! With `interventionChannel: state_machine` in the control config, this operation takes the
//! controller's pending interventions for the session's actor (corrections, reminders) and puts
//! them in front of the model as one agent-only message, before its next inference. The
//! interventions wait in the control ledger until then, so a compaction in between loses none.

use anyhow::Result;
use async_trait::async_trait;

use crate::agents::state_machine::{
    applied, not_applicable, Emitter, GooseEffect, Operation, OperationResult,
};
use crate::conversation::message::{Message, MessageMetadata};
use crate::conversation::Conversation;
use crate::session::Session;

pub struct ControlInterventionOperation;

#[async_trait]
impl Operation<Session, GooseEffect> for ControlInterventionOperation {
    fn name(&self) -> &'static str {
        "control_interventions"
    }

    async fn run(
        &self,
        session: &Session,
        _conversation: &Conversation,
        _emit: &Emitter,
    ) -> Result<OperationResult<GooseEffect>> {
        if !crate::control::pulls_interventions() {
            return not_applicable();
        }
        let interventions = crate::control::take_interventions(session).await;
        if interventions.is_empty() {
            return not_applicable();
        }
        let text = interventions
            .iter()
            .map(|(kind, text)| format!("[control {kind}] {text}"))
            .collect::<Vec<_>>()
            .join("\n");
        applied([Message::user()
            .with_text(text)
            .with_metadata(MessageMetadata::agent_only())
            .into()])
    }
}
