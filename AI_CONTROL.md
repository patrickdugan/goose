# AI control in goose (`ai-control` branch)

This branch turns goose into a controlled runtime for AI-control experiments. Every tool call goose
dispatches is proposed to an external control module and runs only on `allow`. The model can work only
inside the phase its contract allows. The control module's hash-chained ledger records every proposal,
decision, phase move and outcome. With no control configured, goose behaves exactly as upstream.

Base: upstream `main` at `dbac28060` ("agent loop: make tool operations own tool call checks", #12865).
It is forward-ported from the `control-gate` branch (goose v1.53.0, `89d309741` and `25a68aed9`).

## Where the parts come from

| Source | What this branch takes |
|---|---|
| `control-gate` (this fork, v1.53.0) | The control gate (`crates/goose/src/control/`). It loads the `control.goose_hook_config.v1` config, builds proposals, fingerprints calls with canonical JSON (golden vectors shared with Goose-Experiments), treats subagents as their own actors, refuses self-executing providers, and fails closed |
| Goose-Experiments `runtime_control` | The control module the gate talks to: proposals, `decide`, leases, the tool gateway's `commit`, the ledger in Control-Harness receipt format, policy engines, the controller and interventions, replay. Its tests drive this binary (`tests/test_goose_fork.py`, `tests/test_goose_ai_control.py`) |
| Hermes Lite (`control-shim`) | The phase contract: an ultra-lean skill's conveyor (`general-lean`: INSPECT, ACT, VERIFY, FINAL), each phase naming the tool profiles it may use. Goose-Experiments' `phase-contract-v1` engine enforces it from Hermes Lite's own normalization (`agent/lean_contracts.py`) |
| Control-Harness | The receipt ledger semantics and the contract control ladder, which the control module implements and replays |
| Goose Reentrant Control Bench (GRCB, private) | Fault injection over goose's real operations: scenarios, a seeded crash campaign and a TLA+ model. Used to evaluate the branch |

## What is built

### 1. Control gate, ported to main

On main, every tool dispatch goes through `ExtensionLease::call_resolved`: the state machine's tool
execution, code-mode scripts, MCP app calls and subagents. The gate sits there. It replaces the
`ExtensionManager::dispatch_tool_call_inner` hook of v1.53.0, which no longer exists. Goose also executes
some calls itself, outside the lease; those are gated where they run.

| File | Change |
|---|---|
| `crates/goose/src/control/mod.rs` | The gate (from `control-gate`), plus the conveyor client: `phase_skill()`, `phase()`, and entering each actor on the conveyor before its first proposal |
| `crates/goose/src/agents/extension_manager/lease.rs` | `call_resolved` asks the gate before `call_tool` and turns a refusal into the tool's error result. `CallRequest` carries the call's surface (`call_for_app` marks `app_call`). The lease holds the session manager, so the gate can tell a subagent's call from its parent's |
| `crates/goose/src/agents/extension_manager/mod.rs` | Leases are built with the session manager |
| `crates/goose/src/agents/tool_execution.rs` | `ToolCallContext` carries `surface` and the gateway actor id |
| `crates/goose/src/agents/platform_extensions/code_execution.rs` | Calls from scripts are tagged `code_mode` |
| `crates/goose/src/agents/mcp_client.rs` | Gateway calls carry `_meta.agentId`, so the gateway finds the lease the gate obtained |
| `crates/goose/src/agents/state_machine/ops_recipe.rs`, `ops_skills.rs` | The state machine's own `recipe__final_output` and `load_skill` are gated |
| `crates/goose/src/providers/provider_registry.rs` | Every `ProviderEntry` constructor refuses a provider that runs its own tool loop (`runs_own_tool_loop`, plus the named CLI and `*_acp` providers) while control is on, before the provider starts |

The legacy agent loop, which v1.53.0 gated in `agent.rs`, has been removed upstream. The state machine is
now the only loop, so `GOOSE_STATE_MACHINE` no longer selects anything.

### 2. Phase contracts (Hermes Lite) through goose

- **Who owns the conveyor.** The control module, not goose and not the model.
- **Entering.** goose starts each actor at the contract's first phase the first time it offers that actor
  tools, or at its first proposal if that comes first (`POST /v1/phase`, `op: enter`). The call is
  idempotent: a restarted goose is told the phase the ledger already holds.
- **Moving on.** A new `control` platform extension offers the model one tool,
  `control__phase_complete {phase}`. The module accepts it only when `phase` is the actor's current
  phase. It then moves the actor to the next phase of the contract and records `phase_entered`. The model
  never names the phase it moves to, so it cannot skip ahead.
- **What the model sees.** The same extension puts the current phase and its tool profiles into the
  model's context every turn, through goose's top-of-mind (MOIM) channel.
- **Tools offered.** Under a phase contract, the tool-execution operation offers the model only the tools
  the module would admit now (`ops_toolcalling.rs`, `control::offered_tools`). Those are tools that are
  registered, and either not consequential or with a profile in the current phase. A call to a tool goose
  did not offer is answered as unavailable before approval and never runs. If the module does not
  answer, goose offers nothing.
- **Enforcement.** The gate remains the enforcement point. The `phase-contract-v1` engine refuses a
  consequential call whose tool profile is not in the current phase (`tool_profile_not_in_phase`). This
  matters for code-mode scripts, which can call any tool of the session whether it was offered or not.
- **No separate pre-approval operation.** It is not needed. With tools filtered at inference, an
  out-of-phase call reaches approval only when the phase changes between inference and approval, and
  then the gate refuses it at dispatch.
- **Restarts and compaction.** The phase lives in the control ledger, outside the conversation.
  Compaction cannot change it, and a restart reads it back.

## Status (2026-10-14)

All results below are from a Windows debug build of this branch, with `GOOSE_CONTROL_REQUIRED=1`,
driven by the Goose-Experiments harness (`goose-ai-control` branch). The model is scripted and every
non-loopback request is refused.

| Check | Result |
|---|---|
| Gate coverage carried over from v1.53.0 (`tests/test_goose_fork.py`). Every call is proposed (C1). A refused call never reaches the gateway (C2). Control module down blocks every call (C3). Code-mode calls are their own surface (C9). A subagent is its own actor, and its lease reaches the gateway (C10). An unusable config blocks every call | 10 of 10. Both test classes now take the same path, since the state machine is the only loop |
| Phase conveyor on the control module alone: idempotent entry, one move per completion and only from the current phase, the last phase is final, the phase survives a module restart, the admissible-tool view follows the phase, unknown skills, actors and runs without the engine are refused | 5 of 5 |
| Phase conveyor through goose over ACP, recorded in the ledger. In INSPECT the model is offered the read and the phase tool but not the write. After it completes INSPECT, the module moves it to ACT, the write is offered, and the write is admitted (one payment). The model's per-turn context shows INSPECT, then ACT. A write called in INSPECT anyway is answered as unavailable and never runs. A code-mode script that calls the write in INSPECT is proposed (`code_mode`) and refused (`tool_profile_not_in_phase`). A model that names the next phase itself is refused (`phase_claim_mismatch`) and stays in INSPECT. Below a 32k context the conveyor still holds | 5 of 5 |
| Goose-Experiments' whole suite with the module's conveyor added | 132 tests, OK (12 skipped without a goose binary set) |
| With control off, this branch behaves as upstream. The no-fork suite (`tests/test_goose.py`, 13 tests, hooks and gateway tiers) was run against this branch and against unmodified `dbac28060`, built with the same features | Identical outcomes for every test, including two that differ from v1.53.0 because upstream changed (below) |
| goose's own tests (`cargo test -p goose --lib -- agents:: control::`, features `rustls-tls,scheduler`, WSL) | 486 passed, 5 failed. The same 5 tests fail on unmodified `dbac28060` with these features |
| `cargo clippy -p goose --all-targets -- -D warnings` (features `rustls-tls,scheduler,acp-http`), `cargo fmt --check` | clean |
| The platform-extensions prompt snapshot, updated with the `control` section, run with the extension features on Windows | The `control` section matches. The only other difference is the `developer` extension's Windows-specific instructions, which the Linux/macOS snapshot does not contain |

Two upstream changes since v1.53.0 affect the no-fork tiers:
- **Subagent calls.** The `PreToolUse` hook now sees them, so the hooks tier no longer misses them.
- **Code-mode calls.** The hook still misses these.
- **`load_skill`.** The state machine always offers its own `load_skill` tool, even with the skills
  extension off.

The fork is therefore still needed for code mode, for subagents as their own actors whose gateway leases
follow them (`_meta.agentId`), and for the conveyor. Fail-closed behaviour is available to hooks too:
the hooks tier sets `on_failure: block`, and a hook that cannot start blocks the call.

Finding: goose leaves out its per-turn context (MOIM) when the context limit is under 32,000 tokens
(`MIN_CONTEXT_FOR_MOIM`). Lean-context runs (the harness defaults to 12,000) therefore never show the
model its phase in that context. The model still learns the phase from the extension's instructions,
from `phase_complete`'s result and from refusals, and the conveyor holds either way. A test pins this
behaviour.

## Configuration

- `GOOSE_CONTROL_CONFIG` names a `control.goose_hook_config.v1` JSON file. It holds:
  - the control module URL (loopback only), the shim token and the run id;
  - the contract and registry hashes;
  - the actor id, the gateway extension name and the registry tool ids;
  - optionally `phaseSkill`, the skill whose phase contract the run follows.

  A file that is named but unusable blocks every call.
- `GOOSE_CONTROL_REQUIRED=1` blocks every call when no config is set, and refuses self-executing providers.
- The `control` platform extension is off by default. Enable it as a builtin (`--with-builtin control`, or
  `extensions.control.enabled: true`) in runs that use a phase contract.

A refusal, an unreachable control module, an unparsable answer or an unusable config each deny the call,
and the model is told why.

## Further work

Some further work is held back from this branch for now, pending coordination with the goose
maintainers. It will be added when that is done.

## Build

```powershell
$env:RUSTUP_TOOLCHAIN = "1.99.0-x86_64-pc-windows-msvc"
cargo build -p goose-cli --bin goose --no-default-features `
  --features code-mode,tree-sitter,rustls-tls,bundled-mcp,platform-apps,chat-recall,acp-http,scheduler,disable-update
```

`local-inference` is left out because it needs libclang, and experiments use an external model server
through the control proxy. `online-model-meta` is left out because experiments refuse all non-loopback
egress.
