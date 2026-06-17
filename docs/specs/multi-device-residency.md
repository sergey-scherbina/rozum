# Multi-Device Residency & Placement

## Overview

Run several models **smartly across whatever compute devices are present right
now**. On Apple Silicon that is one Metal/UMA device (today's case). On a
commodity x86 box it is a **discrete NVIDIA GPU** (own VRAM) + an **integrated
GPU** (UMA over system DRAM) + the **CPU** — none enormous, but together not
small. The runtime detects the available devices and their memory budgets,
places each *resident* model on **exactly one** device by size-class and role,
and routes each request to the right resident. This generalizes the existing
single-device warm cache (`shared-gateway-multislot.md`) along a **device** axis.
It is the hardware-facing embodiment of rozum's North Star (`SPEC.md`): keep
intelligence running on whatever is actually here, with what is available now.

It explicitly does **not** split one model across heterogeneous devices — with a
large throughput gap and a slow iGPU↔dGPU interconnect, tensor/pipeline
parallelism across them is net-negative (the `mistralrs-mlx-direct.md`
cross-runtime sync-floor lesson). Heterogeneity is exploited by (a) running
**different models/roles in parallel** on different devices, and (b) **speculative
decoding** — a small draft model on a weak device, the big target verifying on
the fast one — the one genuine single-stream co-use.

This is an extension of the durable layer; it lives **above** the `ChatBackend`
SPI, so it works for any backend that can be pinned to a device (GGUF/llama.cpp →
CUDA/Vulkan/CPU; HTTP → an endpoint; native MLX → the single Metal device). The
MLX runtime stays Apple-only; the x86 path is GGUF-CUDA/Vulkan + HTTP
(`portability-and-the-backend-spi.md`, `portability-cuda-gguf`).

## Interface

### Device inventory
- `DeviceCatalog` — enumerated at startup:
  `[{ id, kind: Metal|Cuda|Vulkan|Cpu, label, memory_budget_bytes, throughput_class }]`.
  Best-effort detection (llama.cpp device enumeration + `nvidia-smi`/sysinfo for
  budgets; CPU/DRAM always present), each entry overridable by config.

### Config (`rozum.toml`)
```toml
[[device]]              # optional overrides of detection
id = "cuda:0"
memory_budget_mb = 7000
enabled = true

[[resident]]            # optional explicit model→device pinning
model = "qwen2.5-coder-14b"
device = "cuda:0"
role = "worker"         # worker | draft | router | embed | …
n_ctx = 16384

[[resident]]
model = "qwen3-1.7b"
device = "vulkan:0"     # the iGPU
role = "draft"
```
- **Zero-config default:** auto-place — the biggest model that fits becomes the
  `worker` on the highest-throughput device with room; if a secondary device
  exists, a small model is placed there as `draft`/`router`. Explicit
  `[[resident]]` / `[[device]]` fully override the auto-plan.

### Backend builder
- The builder gains a `device: DeviceId` parameter:
  `build(model_spec, n_ctx, device)`. GGUF maps `device` to llama.cpp
  (`main_gpu` / `n_gpu_layers` / `tensor_split` / Vulkan device index); HTTP maps
  it to the upstream endpoint; MLX ignores it (one Metal device).

### Gateway / routing
- The resident set becomes **per-device**. A request routes to the resident
  matching its model/role; the cascade picks `worker` vs a cheap secondary.

## Behavior

- [ ] On startup the gateway enumerates devices + per-device memory budgets;
      with no detectable accelerator it degenerates to one device (today's
      behavior) — **zero regression on Apple Silicon**.
- [ ] A resident model lives on **exactly one** device; no model is split across
      devices.
- [ ] The placement planner assigns models to devices under **per-device**
      memory budgets and **never oversubscribes** a device (OOM is process-fatal,
      per `project-mlx-35b-prefill-oom`); the least-useful **idle** resident is
      evicted to make room (generalizes `resident::plan_residency`).
- [ ] A heavy/worker request routes to the worker resident (fast device); a
      cheap classify/route/embed request prefers a small resident on a secondary
      device (iGPU/CPU).
- [ ] Residents on **different** devices serve **concurrently** (independent);
      two residents on the **same** device share that device's admission gate
      (`concurrency-multi-instance`).
- [ ] Speculative decoding (opt-in) drafts on a secondary device and verifies on
      the worker device — single-stream co-use of two devices.
- [ ] Zero-config produces a sensible placement on any machine; `rozum.toml`
      `[[device]]` / `[[resident]]` override it deterministically.
- [ ] Off by default for single-device machines / single-model traffic — a
      strict no-op vs today's `shared-gateway-multislot` unless ≥2 devices (or an
      explicit multi-resident config) are in play.

## Out of scope

- Splitting one model's tensors/layers across heterogeneous devices (rejected —
  net-negative; see Decisions).
- Cross-machine / distributed inference — `distributed-readiness.md`.
- A shared cross-device GPU admission gate beyond per-device gates — two
  residents on one device already contend (`concurrency-multi-instance`).
- The MLX runtime on non-Apple hardware (MLX is Apple/Metal-only).
- Auto-tuning per-device budgets / throughput classes from live benchmarks
  (start with detection + config; learn later).

## Design

- **`DeviceCatalog`** (new, hardware-keyed module): detection behind a small
  trait so it is testable with an injected catalog (no real GPUs in CI). Apple →
  one Metal device; x86 → CUDA devices (VRAM each) + Vulkan devices (incl. the
  iGPU, budgeted from a DRAM slice) + CPU.
- **Per-device resident table** in the `Switchboard`: replace the single
  primary+warm set with `HashMap<DeviceId, Vec<Resident>>`, each
  `Resident = { device-pinned backend, weight_bytes, inflight, last_used, role }`.
  The existing primary/warm logic becomes the degenerate one-device case.
- **`plan_placement(requested, devices, usage)`** generalizes
  `resident::plan_residency` (today: one budget) to **a budget per device**:
  assign by size-class (big → biggest-VRAM device), evict least-useful idle
  residents per device to fit, refuse to oversubscribe.
- **Builder device-pinning**: thread `DeviceId` through `main.rs`'s backend
  builder; GGUF translates it to llama.cpp args.
- **Routing**: extend `Switchboard::enter(model, role)` to select a resident by
  `(model, role, device)`; the cascade router's strategy stays, gaining
  device/role awareness.
- **Roles** map to a device-class preference: `worker`→fastest, `draft`→any
  secondary, `router`/`embed`→cheapest. Roles come from config or are inferred
  (size-class).

## Decisions

- **Different models per device, never one model split** — chosen because a
  large dGPU/iGPU throughput gap + the PCIe/UMA interconnect make heterogeneous
  tensor/pipeline parallelism lose to a single-device baseline. Rejected:
  device-split of one model (the structural cross-device sync floor, per
  `mistralrs-mlx-direct.md`).
- **Generalize `plan_residency` to per-device budgets** — chosen to reuse the
  proven frequency×recency utility planner. Rejected: a bespoke heterogeneous
  scheduler.
- **Auto-detect + zero-config defaults, fully overridable** — chosen so commodity
  hardware "just works" out of the box while power users pin exact placements.
- **Above the SPI; MLX stays Apple-only, x86 = GGUF-CUDA/Vulkan + HTTP** — chosen
  per the portability spec; device-awareness is orchestration, not a backend.
- **Off by default / no-op for one device** — chosen to keep the common Apple +
  single-model path byte-identical, exactly as `shared-gateway-multislot` did.

## Results

<!-- Fill in after implementation: detected catalog on a real x86+NVIDIA+iGPU box;
     worker(dGPU) + small(iGPU) concurrent throughput; spec-decode draft(iGPU) +
     target(dGPU) speedup; no-regression on Apple single-device. -->
