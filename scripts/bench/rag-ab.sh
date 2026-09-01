#!/bin/bash
# A/B: does `rag.search` help an agent LOCATE code in a real repository?
#
# Deliberately NOT part of agentic.sh: the matrix measures create/fix in tiny sandboxes, where
# retrieval has nothing to retrieve — its tasks would measure only that an extra tool does not
# break anything. Location in a big unfamiliar repo is the case RAG was built for (BACKLOG
# "agents doing ORDINARY CODE WORK"), so that is what this measures: same model, same prompt,
# same jailed runner, with and without the standalone `rozum rag mcp` server registered.
#
# CAREFUL-BY-DESIGN, because other agents share this machine (operator, 2026-09-01):
#   - runs against the RESIDENT shared gateway (--gateway-url), never loading a second chat
#     model — the no-hijack rule;
#   - strictly sequential, one agent turn at a time;
#   - the agent works in a THROWAWAY git worktree of this repo, so a stray Edit from a
#     skip-permissions run can never touch anyone's tree;
#   - new files only; agentic.sh is not modified.
#
# Usage: scripts/bench/rag-ab.sh [runs-per-arm]   (default 1)

set -u
cd "$(dirname "$0")/../.."
REPO="$PWD"
# The index/vectors SOURCE. Explicit, because the first run of this script measured "RAG with no
# index" without noticing: it copied from $REPO/.rozum, and when the script runs from a bench
# WORKTREE that directory is empty — untracked state does not follow worktrees. The rag arm's
# model dutifully called rag.search, got `no_index`, and the A/B silently became bare-vs-bare
# with extra schema tokens. An arm must FAIL LOUDLY if its premise is missing.
INDEX_SRC="${ROZUM_RAG_INDEX_SRC:-$HOME/work/my/rozum/.rozum}"
[ -f "$INDEX_SRC/rag-index.json" ] || { echo "no index at $INDEX_SRC — build it first" >&2; exit 2; }
[ -f "$INDEX_SRC/rag-vectors.bin" ] || { echo "no vectors at $INDEX_SRC — warm up first" >&2; exit 2; }
BIN="${BIN:-$HOME/.cargo/bin/rozum}"
GATEWAY="${ROZUM_BENCH_GATEWAY:-http://127.0.0.1:8089}"
RUNS="${1:-1}"
MAX_TURNS="${MAX_TURNS:-8}"
TIMEOUT_S="${TIMEOUT_S:-240}"
OUT="/tmp/rag-ab-$(date +%s)"
mkdir -p "$OUT"

# Questions phrased the eval-set way — the agent does NOT get the symbol. `expect` is the
# repo-relative file that counts as a correct localisation.
Q1="Which file contains the function that decides whether a model is allowed to become resident in memory?";        E1="crates/rozum-core/src/share.rs"
Q2="Which file decides which meeting room a telegram group chat belongs to?";                                       E2="crates/rozum-meeting/src/messenger_groups.rs"
Q3="Which file detects that a model's chat template does not render tool definitions?";                             E3="crates/rozum-mlx/src/mlx_native_backend.rs"
Q4="Which file implements breaking an agent out of a loop of repeated identical actions?";                          E4="crates/rozum-gateway/src/loopbreak.rs"
Q5="Which file shortens a conversation so it fits into the model's context window?";                                E5="crates/rozum-gateway/src/auto_context.rs"
Q6="Which file splits a markdown document into heading-bounded sections for retrieval?";                            E6="crates/rozum-agent/src/rag_chunk.rs"
# 9 more (rag-ab-expanded, 2026-09-01) — same rules: no symbol names in the question, one
# unambiguous repo-relative file as the key, each verified against the file's own header
# comment before being added.
Q7="Which file implements speculative decoding that needs no draft model, guessing continuations from n-grams already present in the context?";  E7="crates/rozum-mlx/src/specdecode_plookup.rs"
Q8="Which file implements the standalone MCP server that serves only the retrieval search tool, without any meeting features?";                  E8="crates/rozum-agent/src/rag_mcp.rs"
Q9="Which file trims the codex client's tool set and system prompt so small local models can handle it?";                                        E9="crates/rozum-gateway/src/codex_lean.rs"
Q10="Which file rewrites and applies the patches the codex client submits in its own diff format?";                                             E10="crates/rozum-gateway/src/codex_patch.rs"
Q11="Which file manages per-user access control for the messenger bridge, editable live from inside the chat?";                                  E11="crates/rozum-meeting/src/messenger_acl.rs"
Q12="Which file implements the engine-agnostic token sampler shared by the model backends?";                                                    E12="crates/rozum-core/src/sampler.rs"
Q13="Which file constrains generation so tool-call arguments come out as valid JSON?";                                                          E13="crates/rozum-core/src/constrain.rs"
Q14="Which file guards the bug ledger where defects are recorded and tracked?";                                                                 E14="crates/rozum-core/src/bug_ledger.rs"
Q15="Which file defines the backend trait every chat model engine implements?";                                                                 E15="crates/rozum-core/src/backend.rs"

MCP_CFG="$OUT/mcp.json"
cat > "$MCP_CFG" <<EOF
{ "mcpServers": { "rozum-rag": { "command": "$HOME/.cargo/bin/rozum-gateway", "args": ["rag", "mcp"] } } }
EOF

run_one() { # $1=arm(rag|bare) $2=qnum $3=question $4=expect $5=run
  local arm=$1 qn=$2 q=$3 expect=$4 rn=$5
  local wt="$OUT/wt-$arm-$qn-$rn"
  git worktree add --detach "$wt" origin/master >/dev/null 2>&1
  # The throwaway worktree shares .rozum? No: .rozum is untracked, so the worktree has NO index —
  # copy the built index+vectors in, read-only inputs for the tool.
  mkdir -p "$wt/.rozum"
  cp "$INDEX_SRC/rag-index.json" "$INDEX_SRC/rag-vectors.bin" "$wt/.rozum/" || return 3
  printf '*\n' > "$wt/.rozum/.gitignore"

  local prompt="You are in a large Rust repository you have not seen before. $q Answer with the single repository-relative file path on the last line. Investigate as needed but do not modify any files."
  local aargs=(claude -p "$prompt" --output-format stream-json --verbose
               --dangerously-skip-permissions --max-turns "$MAX_TURNS")
  if [ "$arm" = rag ]; then
    aargs+=(--mcp-config "$MCP_CFG")
  fi
  local log="$OUT/$arm-$qn-$rn.jsonl"
  # ROZUM_VERIFY=0: without it `rozum launch` follows the agent run with its verify-gate —
  # `cargo build && cargo test` over the throwaway worktree of this whole workspace — which
  # dwarfs the agent's own time (measured: agent done in 103 s, then the gate ate the rest of
  # the 240 s cap). agentic.sh sets the same for the same reason.
  ( cd "$wt" && timeout "$TIMEOUT_S" env ROZUM_VERIFY=0 "$BIN" launch --gateway-url "$GATEWAY" \
      --no-channel-wakeup --no-piggyback --lean "${aargs[@]}" ) > "$log" 2>"$log.err"
  local rc=$?

  # Score: expected path appears in the FINAL assistant text; count rag.search calls.
  python3 - "$log" "$expect" <<'PY'
import json,sys
log,expect=sys.argv[1],sys.argv[2]
final=""; rag_calls=0; turns=0
try:
    for line in open(log):
        try: m=json.loads(line)
        except: continue
        t=m.get("type")
        if t=="assistant":
            turns+=1
            for c in m.get("message",{}).get("content",[]):
                if c.get("type")=="text": final=c.get("text","")
                if c.get("type")=="tool_use" and "rag" in c.get("name",""): rag_calls+=1
except FileNotFoundError: pass
ok = expect in final
print(f"RESULT ok={int(ok)} rag_calls={rag_calls} turns={turns}")
PY
  git worktree remove --force "$wt" >/dev/null 2>&1
  return $rc
}

echo "arm,q,run,ok,rag_calls,turns,secs" > "$OUT/results.csv"
for rn in $(seq 1 "$RUNS"); do
  for qn in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
    eval "q=\$Q$qn"; eval "e=\$E$qn"
    for arm in bare rag; do
      t0=$(date +%s)
      line=$(run_one "$arm" "$qn" "$q" "$e" "$rn" | grep '^RESULT')
      t1=$(date +%s)
      ok=$(echo "$line" | sed 's/.*ok=\([01]\).*/\1/')
      rc_calls=$(echo "$line" | sed 's/.*rag_calls=\([0-9]*\).*/\1/')
      turns=$(echo "$line" | sed 's/.*turns=\([0-9]*\).*/\1/')
      echo "$arm,$qn,$rn,${ok:-0},${rc_calls:-0},${turns:-0},$((t1-t0))" | tee -a "$OUT/results.csv"
    done
  done
done
echo; echo "== summary =="
python3 - "$OUT/results.csv" <<'PY'
import csv,sys
rows=list(csv.DictReader(open(sys.argv[1])))
for arm in ("bare","rag"):
    r=[x for x in rows if x["arm"]==arm]
    ok=sum(int(x["ok"]) for x in r); calls=sum(int(x["rag_calls"]) for x in r)
    secs=sum(int(x["secs"]) for x in r)
    print(f"{arm:5} ok {ok}/{len(r)}  rag_calls {calls}  total {secs}s")
PY
echo "logs: $OUT"
