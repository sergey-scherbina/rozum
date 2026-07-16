# CI green baseline

## Overview

Restore CI as a truthful release gate after the Cargo workspace and binary split.
The workflow must build the binaries that actually exist, compile every in-scope
test target, and describe platform exclusions explicitly instead of reporting a
green or red result for a command that no longer represents the product.

## Interface

The public interface is `.github/workflows/ci.yml` on pushes and pull requests to
`master`:

- **macOS** builds the engine-bearing `rozum-gateway` and thin `rozum` dispatcher
  with their shipped defaults, then runs every workspace library test.
- **Linux** builds the same entrypoints with workspace default features disabled,
  then runs every workspace library test with default features disabled.
- **Windows** compiles and tests the explicitly named portable-core packages with
  default features disabled. Packages that still own documented Unix-only daemon,
  PTY, control-server, or service-install seams are excluded by name until those
  seams are ported; the job must not imply that the full Windows host is supported.

Cargo package and binary names are always explicit where a single artifact is
selected: package `rozum` produces `rozum-gateway`; package `rozum-cli` produces
the user-facing `rozum` dispatcher.

## Behavior

- [x] No CI command refers to a non-existent root `rozum` binary target.
- [x] macOS reaches and passes `cargo test --workspace --lib` with shipped default
      features, so every workspace library's `#[cfg(test)]` code is compiled.
- [x] Linux reaches and passes
      `cargo test --workspace --no-default-features --lib`.
- [x] Windows reaches and passes build/tests for every package listed as its
      portable-core scope; the list and exclusions are documented in the workflow.
- [x] The Windows-tested residency queue can inspect every live waiter's footprint
      without reading through another process's exclusive file lock.
- [x] The Windows-tested residency ledger keeps liveness locks separate from readable
      reservation metadata, so a held lock does not collapse committed RAM to zero.
- [x] Multi-command steps fail immediately on the first non-zero native process on
      every shell, including PowerShell.
- [x] A successful latest workflow run contains a green macOS, Linux, and Windows
      job; a failed build cannot be masked by a later command.
- [x] The global spec, binary/workspace split specs, Cargo comments, sprint, backlog,
      and changelog describe the same binary names, default feature set, and CI
      coverage.

## Out of scope

- Porting Unix-domain meeting IPC, PTY/tmux control, launchd/systemd installation,
  or the UCC WebAuthn server to Windows.
- Enabling MLX or Metal-dependent engines on Linux or Windows.
- Making bare default-feature builds platform-dependent; non-macOS CI continues to
  select `--no-default-features` explicitly.
- Treating ignored real-model tests as CI-safe hardware tests.

## Design

Keep the three jobs independent and use package/target names from Cargo metadata.
The macOS and Linux jobs exercise the whole workspace library graph. Windows uses
an allow-list rather than a broad workspace command because exclusions are known,
architectural platform seams, not flaky tests; adding a package to that list is the
portable-support promotion gate.

After every workflow change, push a commit and inspect the real GitHub Actions job.
Local macOS checks are necessary but cannot substitute for Linux/Windows evidence.

## Decisions

- **Honest Windows allow-list** — chosen because the global portability spec already
  records Unix-only meeting/control/service seams. Rejected: `continue-on-error`
  (hides a broken gate) and claiming whole-workspace Windows support (false today).
- **Whole-workspace library tests on macOS/Linux** — chosen because root-only
  `cargo test --lib` allowed the gateway suite to rot without compiling. Rejected:
  package-by-package hand lists on platforms where the whole library graph is
  intended to work.
- **Separate spec and implementation commits** — chosen so the CI coverage contract
  is reviewable before YAML or portability code changes.

## Results

Completed 2026-07-16.

- `06de22c` replaced the dead post-split `--bin rozum` commands with real workspace
  binary builds and whole-library gates; the Windows job uses its documented package
  allow-list and builds the portable dispatcher.
- Real Windows runs exposed two lock-semantic bugs hidden by Unix: a process could not
  read a locked waiter/resident file. `645f5e7` supplied the held ticket directly;
  `5b675ea` completed the design by publishing waiter footprint in the filename and
  separating resident lifetime locks from readable metadata, with legacy Unix fallback.
- GitHub Actions run
  [`29533946535`](https://github.com/sergey-scherbina/rozum/actions/runs/29533946535)
  passed all three jobs: Linux whole-workspace no-default build/tests, macOS whole-workspace
  build/tests with shipped defaults, and the Windows dispatcher + portable-core suite.
- Local verification also passed the complete 133-test `rozum-core` library suite,
  focused queue/residency tests, both workspace library/build profiles, and a Windows
  GNU cross-target compile check. The authoritative Windows behavioral result is the
  hosted MSVC job above.
