# GGUF off the Mac: which feature to build, and what was in the way

Status: implemented 2026-08-09. `crates/rozum-gguf`, the admission gate in `src/main.rs`, the RAM
probes in `rozum-core::concurrency`.

## The one-line answer for a user

| Machine | Build |
|---|---|
| Apple Silicon | `cargo build --features gguf` — Metal, as before |
| x86 CPU (any OS) | `cargo build --features gguf` |
| NVIDIA | `cargo build --features gguf-cuda` |
| Intel/AMD iGPU, or anything with Vulkan | `cargo build --features gguf-vulkan` |
| AMD ROCm | `cargo build --features gguf-rocm` |

## What was actually wrong

The engine was described as portable and was not, in three separate ways — each found by trying to
run it rather than by reading it.

**1. `metal` was welded to the feature.** `llama-cpp-2` was declared with `features = ["metal"]`, so
`--features gguf` asked for an Apple backend on every platform. A Linux user had to edit
`Cargo.toml`. Metal now follows the TARGET (`[target.'cfg(target_os = "macos")'.dependencies]`), so a
Mac still gets the GPU from a plain `--features gguf` and the same command elsewhere does not ask for
Metal; the GPU backends are their own features, because which one exists is a property of the machine.

**2. The admission gate refused every GGUF.** Footprint estimation only consulted the HF/MLX catalog,
so a `.gguf` path — which is how every GGUF is named — never matched, and the load was refused with
"its size is UNKNOWN" on a host with 237 GB free and a 138 MB model. A file's size is not a mystery:
the gate now measures it with the same resolver the engine loads through, so both open the same file.
`--offline` had the same hole and would tell an operator to download the file they had just pointed
at.

**3. The safety lever did not exist on the platform it protects.** Both RAM probes shelled out to
macOS-only tools (`sysctl hw.memsize`, `vm_stat`). On Linux they returned nothing, so the gate that
exists to prevent an OOM reboot measured NOTHING and failed open — on exactly the x86 machines this
engine is for. `/proc/meminfo` is read there now (`MemTotal`, `MemAvailable`), with the kB unit
parsed rather than assumed: dropping it under-counts RAM by 1024×, which as a budget means admitting
everything.

## What is proven, and what is not

Proven on this Apple machine:

- feature resolution per target, measured with `cargo tree --format {f}`: `gguf` gives Metal on
  `aarch64-apple-darwin` and no Metal on `x86_64-unknown-linux-gnu` / `x86_64-pc-windows-msvc`, while
  `gguf-cuda` / `gguf-vulkan` / `gguf-rocm` reach `llama-cpp-2` on the x86 targets;
- the Mac build is unchanged and still Metal;
- **real inference**: a 138 MB GGUF answered through the gateway's OpenAI endpoint, with
  `offloaded 0/31 layers to GPU` — the CPU path an x86 build without a GPU feature takes;
- `rozum-stamp`, `rozum-core`, `rozum-gguf` and `rozum-models` compile for `x86_64-pc-windows-gnu`
  with zero errors.

NOT proven, and it needs the hardware: that CUDA, Vulkan or ROCm actually COMPILE and run. Those
build llama.cpp against vendor SDKs that are not on this machine. The Cargo plumbing is correct and
the code path is the one just exercised on CPU, but "the features resolve" is not "it runs", and this
spec will not pretend otherwise.

## Known limits

- A Mac cannot build a CPU-only GGUF: Metal follows the target, and Cargo has no way to make a
  target-conditional dependency feature optional. Use `ROZUM_GGUF_GPU_LAYERS=0` to run on CPU with
  the Metal backend merely present — which is how the CPU path above was exercised.
- A load failure now names the loader and its reason. It used to surface as a bare "no backend found
  for <path>", because the only report was a `tracing::warn` and nothing installs a subscriber.
