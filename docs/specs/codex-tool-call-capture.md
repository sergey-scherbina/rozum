# Codex Tool-Call Capture

## Overview

Add an opt-in diagnostic trace for Codex's OpenAI Responses path so matrix runs
can inspect the raw tool-call shapes emitted by a local model before the gateway
normalizes or rewrites them. The immediate use case is the create-from-scratch
codex failures in `docs/matrix-failure-analysis.md`: determine whether the model
chooses shell `echo > file`, calls meta-tools, calls `apply_patch`, or has its
call rewritten by the gateway.

## Interface

Set `ROZUM_CODEX_TOOL_CAPTURE=1` to append capture events to the existing
gateway JSONL log (`ROZUM_GATEWAY_LOG`, else `~/.rozum/gateway.jsonl`).

Optional:

- `ROZUM_CODEX_TOOL_CAPTURE_MAX_BYTES=<N>` caps each captured argument string.
  Default is `65536`. `0` means no cap.

Events:

- `codex_tool_inventory`
  - Emitted once per `/v1/responses` request.
  - Includes the model, stream mode, original Responses tool names, the
    post-lean/post-injection tool names shown to the backend, whether codex
    offered `apply_patch` as a real function tool, and whether experimental
    apply_patch injection was enabled.
- `codex_tool_call`
  - Emitted once per completed tool call, before it is returned to Codex.
  - Includes response id, call id, raw tool name, emitted tool name, whether the
    call was an apply_patch function reroute, whether args changed, raw args,
    final args, and truncation metadata.

## Behavior

- [ ] Capture is completely off unless `ROZUM_CODEX_TOOL_CAPTURE` is set and is
      not `0`.
- [ ] Captured `raw_args` are the exact buffered model tool-call arguments before
      `rewrite_apply_patch_function_args` or `normalize_codex_tool_args`.
- [ ] Captured `final_args` are the arguments returned to Codex after gateway
      rewrites.
- [ ] Both streaming and non-streaming `/v1/responses` paths produce equivalent
      capture events.
- [ ] Capture uses the existing file-backed gateway log and never writes to the
      agent terminal.

## Out of scope

- Running or re-running the agentic matrix.
- Changing rewrite behavior, codex-lean behavior, tool schemas, or model
  prompting.
- Capturing non-Codex Chat Completions / Anthropic traffic.
