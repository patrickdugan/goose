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
- **Entering.** The gate starts each actor at the contract's first phase before its first proposal
  (`POST /v1/phase`, `op: enter`). The call is idempotent: a restarted goose is told the phase the
  ledger already holds.
- **Moving on.** A new `control` platform extension offers the model one tool,
  `control__phase_complete {phase}`. The module accepts it only when `phase` is the actor's current
  phase. It then moves the actor to the next phase of the contract and records `phase_entered`. The model
  never names the phase it moves to, so it cannot skip ahead.
- **What the model sees.** The same extension puts the current phase and its tool profiles into the
  model's context every turn, through goose's top-of-mind (MOIM) channel.
- **Enforcement.** The `phase-contract-v1` engine refuses a consequential call whose tool profile is not
  in the current phase (`tool_profile_not_in_phase`).
- **Restarts and compaction.** The phase lives in the control ledger, outside the conversation.
  Compaction cannot change it, and a restart reads it back.

## Status (2026-10-14)

All results below are from a Windows debug build of this branch, with `GOOSE_CONTROL_REQUIRED=1`,
driven by the Goose-Experiments harness (`goose-ai-control` branch). The model is scripted and every
non-loopback request is refused.

| Check | Result |
|---|---|
| Gate coverage carried over from v1.53.0 (`tests/test_goose_fork.py`). Every call is proposed (C1). A refused call never reaches the gateway (C2). Control module down blocks every call (C3). Code-mode calls are their own surface (C9). A subagent is its own actor, and its lease reaches the gateway (C10). An unusable config blocks every call | 10 of 10. Both test classes now take the same path, since the state machine is the only loop |
| Phase conveyor on the control module alone: idempotent entry, one move per completion and only from the current phase, the last phase is final, the phase survives a module restart, unknown skills, actors and runs without the engine are refused | 4 of 4 |
| Phase conveyor through goose over ACP, recorded in the ledger. A write is refused in INSPECT (`tool_profile_not_in_phase`), the model completes INSPECT, the module moves it to ACT, and the same write is admitted (one payment). The model's per-turn context shows INSPECT, then ACT. A model that names the next phase itself is refused (`phase_claim_mismatch`) and stays in INSPECT | 3 of 3 |
| Goose-Experiments' whole suite with the module's conveyor added | 132 tests, OK (12 skipped without a goose binary set) |

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
