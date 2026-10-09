# Control gate (fork of goose v1.53.0)

This branch adds a control gate to goose: every tool call is proposed to an
external control module (Goose-Experiments, `runtime_control`) before it runs,
and runs only if the answer is `allow`. Without `GOOSE_CONTROL_CONFIG` or
`GOOSE_CONTROL_REQUIRED` set, goose behaves exactly as upstream.

## Why a fork

goose's no-fork interception points (plugin hooks, tool inspectors) miss two
paths: tool calls made from inside code-mode scripts, and every call a subagent
makes (subagents run with hooks disabled and in Auto mode). Hooks also cannot
ask, and default to allowing when they fail. The gate sits where all paths meet.

## What changed

| File | Change |
|---|---|
| `crates/goose/src/control/mod.rs` | New. The gate: config loading, proposal, fingerprint, canonical JSON (golden-vector tests), subagent actors, provider refusal |
| `crates/goose/src/agents/extension_manager/mod.rs` | `dispatch_tool_call_inner` asks the gate before `call_tool`; the main loop, the state machine, code mode, app calls and subagents all reach it |
| `crates/goose/src/agents/tool_execution.rs` | `ToolCallContext` carries the call's `surface` and the actor id for the gateway |
| `crates/goose/src/agents/platform_extensions/code_execution.rs` | Calls from scripts are tagged `code_mode` |
| `crates/goose/src/agents/mcp_client.rs` | Gateway calls carry `_meta.agentId`, so the gateway finds the lease the gate obtained |
| `crates/goose/src/agents/agent.rs` | `recipe__final_output` in the legacy loop is gated |
| `crates/goose/src/agents/state_machine/ops_skills.rs`, `ops_recipe.rs` | The state machine's own `load_skill` and final-output execution are gated |
| `crates/goose/src/providers/init.rs` | Providers that run tools themselves (Claude Code, Gemini CLI, `*_acp`) are refused while control is on |

## Configuration

- `GOOSE_CONTROL_CONFIG`: path of a `control.goose_hook_config.v1` JSON file
  (control module URL on loopback, shim token, run id, contract and registry
  hashes, actor id, gateway extension name, registry tool ids). A named but
  unusable file blocks every call.
- `GOOSE_CONTROL_REQUIRED=1`: without a config, block every call; refuse
  self-executing providers.

Fail-closed everywhere: a refusal, an unreachable control module, an
unparsable answer or an unusable config each deny the call, and the model is
told why.

## Build (Windows, as used)

```powershell
$env:RUSTUP_TOOLCHAIN = "1.96.1-x86_64-pc-windows-msvc"
cargo build --release -p goose-cli --bin goose --no-default-features `
  --features code-mode,tree-sitter,rustls-tls,bundled-mcp,platform-apps,chat-recall,acp-http,scheduler,disable-update
```

`local-inference` is left out: it needs libclang, and experiments use an
external model server through the control proxy. Tests for the gate run from
Goose-Experiments (`tests/test_goose_fork.py`, `GOOSE_FORK_BIN`), plus the unit
tests in `control/mod.rs`.
