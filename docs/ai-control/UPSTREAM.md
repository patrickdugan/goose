# Upstreaming the ai-control branch: issue and PR drafts

goose's contribution rules (AGENTS.md) require an external PR to link an issue that is on the board in
status Ready, and to stay within that issue's scope. So each piece below starts as an issue; the PR follows
once the issue is Ready. The pieces are listed in order, and each builds on the one before.

Nothing here is filed yet.

---

## 1. Opt-in control hook at the extension-lease dispatch point

**Issue (feature request)**

*Title:* Opt-in, fail-closed control hook where every tool dispatch meets

*Problem.* Teams running goose under an external control layer (a policy service that must approve each
tool call before it runs) cannot cover every call with the existing interception points:
- `PreToolUse` hooks do not see tool calls made from inside code-mode scripts.
- Every dispatch path (the state machine, code mode, MCP app calls, subagents) meets in
  `ExtensionLease::call_resolved`, but there is no extension point there.
- Providers that run their own tool loop (Claude Code, Gemini CLI, `*_acp`) execute tools outside goose
  altogether.

*Proposal.* An optional hook at `ExtensionLease::call_resolved`, configured by environment and off by
default:
- When the hook is configured, each call is described as `(session, parent session, call id, tool,
  extension, arguments, surface)` and runs only if the hook allows it. The surface is one of
  `agent_loop`, `code_mode`, `app_call`, `final_output`, `skill_load`.
- An unusable configuration denies every call.
- goose's own `recipe__final_output` and state-machine `load_skill` go through the same hook.
- While the hook is configured, `ProviderEntry` constructors refuse providers that run their own tool
  loop.

*Verification plan.*
- **Unit tests** of the call description and of fail-closed configuration loading.
- **No behaviour change when off.** The existing `agents::` suites give the same results with the hook
  unconfigured (checked on our fork: identical outcomes for an external 13-test no-fork suite and for
  goose's own tests).
- **Integration.** An external harness (Goose-Experiments) drives `goose acp` with a scripted model. It
  shows that every call is proposed, that a refused call never reaches its server, that code-mode calls
  arrive with their own surface, that a subagent is its own actor, and that a down control service
  blocks every call (10 of 10 on our fork).

**PR (after the issue is Ready)**

- **Title:** `control: opt-in fail-closed hook at the extension-lease dispatch point`
- **Changes:**
  - `crates/goose/src/control/mod.rs` (new)
  - `extension_manager/lease.rs` (`call_resolved`)
  - `tool_execution.rs` (`surface`)
  - `code_execution.rs` (`code_mode` surface)
  - `ops_recipe.rs`, `ops_skills.rs`
  - `provider_registry.rs`
- **Off unless configured.** Tests: unit, plus the unchanged `agents::` suites.

🤖 Generated with [Claude Code](https://claude.com/claude-code)

---

## 2. Optional phase contracts

**Issue (feature request)**

*Title:* Optional phase contracts: offer and admit tools by the current phase of a skill

*Problem.* Lean skills (Hermes Lite's ultra-lean contracts are one example) run as a conveyor of phases,
INSPECT, then ACT, then VERIFY, and each phase is allowed only some tool profiles. goose has no way to
follow such a contract in a way that survives a restart or a compaction. The model cannot be trusted to
hold the phase, and neither can text in its context.

*Proposal.* Building on the hook in (1), an optional conveyor whose state lives with the control service,
not in the conversation:
- **Entering.** goose starts the actor at the contract's first phase.
- **Moving on.** A `control__phase_complete {phase}` tool reports the current phase done. The service
  accepts it only for the current phase and picks the next phase itself.
- **Offering.** The tool-execution operation offers only the tools the current phase admits.
- **Showing.** The current phase is shown in the per-turn context.
- **Enforcing.** The hook refuses anything else at dispatch, including calls from code-mode scripts.

*Verification plan.*
- Unit test of the phase description.
- The platform-extensions snapshot updated.
- Integration through `goose acp`:
  - a write is not offered and not run in INSPECT;
  - a write is offered and admitted in ACT;
  - a code-mode write in INSPECT is refused;
  - a model that names its next phase is refused;
  - the phase survives a restart of the control service.

*Note for reviewers.* goose leaves out the per-turn context when the context limit is under 32k tokens
(`MIN_CONTEXT_FOR_MOIM`). Lean-context runs therefore learn the phase from the tools offered and from
tool results only.

**PR:** `control: optional phase-contract conveyor (control platform extension, per-phase tool offering)`

🤖 Generated with [Claude Code](https://claude.com/claude-code)

---

## 3. Interventions from a control service, delivered in the state machine

**Issue (feature request)**

*Title:* Deliver a control service's queued interventions before the next inference; report compactions

*Problem.* A controller that watches a run may want to correct or remind the model ("pay only alice or
bob", "cap 50 per payment"). Today that needs a client to steer the ACP session at the right moment. That
is racy, works only over ACP, and loses nothing only if the client is timely.

*Proposal.* With the hook from (1) configured for it, a state-machine operation placed after the tool
operations and before inference:
- takes the actor's pending interventions from the control service;
- records each one as delivered;
- puts them in front of the model as one agent-only message, which does not move the turn's kickoff.

goose also reports compactions, automatic and `/compact`, so a controller can remind the actor of what
the summary dropped.

*Verification plan.*
- A correction issued after a refusal reaches the model before its next inference, and only once.
- After `/compact`, a reminder reaches the model before its next proposal.
- Without the configuration, the operation is never applied, so the `agents::` suites are unchanged.

**PR:** `control: deliver queued interventions before inference; report compactions`

🤖 Generated with [Claude Code](https://claude.com/claude-code)
