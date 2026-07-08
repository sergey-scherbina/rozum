# UCC defects

Run: 18 cases, **2 failing**.

## Failing

- **mem-refresh** — ↻ tapped but NO new GET /control/status (11→11)
- **models-fit** — fits=false btnRight=381 (overflow / off-screen)

## All cases

- ✓ nav-home — h1="rozum — control center"
- ✓ nav#/agents — #/agents
- ✓ nav#/coders — #/coders
- ✓ nav#/sessions — #/sessions
- ✓ mem-render — 16.7 GiB, 27.0 GiB, 0.0 GiB
- ✓ mem-no-source
- ✗ mem-refresh — ↻ tapped but NO new GET /control/status (11→11)
- ✗ models-fit — fits=false btnRight=381 (overflow / off-screen)
- ✓ models-one-btn
- ✓ models-name
- ✓ models-feedback — "загрузить …"
- ✓ models-load-post — POST /control/gateway/load
- ✓ chat-wrap — cell 350×481
- ✓ chat-send-present
- ✓ picker-select
- ✓ api-status — 12 models
- ✓ api-stop-guard — HTTP 404: no shared gateway running
- ✓ api-stop-deadlease — not blocked by dead lease (HTTP 404)
