# Agent guidelines

Buildbox is a Bazel Remote Execution API (REAPI) server written in Rust and
built with Bazel. These guidelines apply to every change, by humans and agents
alike.

## Priorities

1. **Technical robustness first.** Correct behaviour under errors, partial
   writes, concurrent requests and malformed input matters more than features
   or speed. Never panic on input from a client; return a proper error.
2. **A well-factored, layered architecture.** Each package has one job and
   depends only on the layers beneath it. Protocol adapters (gRPC services)
   stay thin and delegate to storage and execution layers behind traits.
   Concrete implementations are wired together in one place (the server).
3. **Simple, human-readable code.** Optimise for the next reader. Prefer
   obvious code over clever code.
4. **Efficiency without premature optimisation.** Don't make choices that
   rule out later optimisations (for example, keep storage APIs streaming
   rather than whole-buffer), but don't add complexity for speed until it is
   needed.

## Code style

- **Prefer straight-line code.** Use intermediate variables with descriptive
  names and early returns instead of long chains of fluent calls
  (`.map().and_then().ok_or_else()...`). A short chain of two or three calls
  is fine; beyond that, break it up.
- **Keep `match` arms small.** If an arm needs more than a few lines, move
  the body into a named function.
- **Small functions with one purpose.** If a function needs a comment to
  separate its sections, split it.
- **Errors carry context.** Use typed errors (`thiserror`) and say what was
  being attempted. Don't discard the underlying error.
- **Don't block the runtime's workers.** A worker stuck in a syscall can't
  run the other tasks queued on it, including the gRPC server's. Run work
  that can take a long or unbounded time (subprocesses, network, large or
  many-file I/O) on blocking threads (`tokio::task::spawn_blocking`,
  `tokio::process`). Measure before moving work either way.
- **Document APIs, not implementations.** Give every module a short `//!`
  comment and every public type and function a short `///` comment saying
  what it is for, written as you go. Keep them concise. Avoid `//` comments
  narrating implementation details inside function bodies; if code needs
  explaining, prefer clearer code or put it in the API docs. Brief `SAFETY:`
  notes on `unsafe` blocks are the exception.
- Match the comment density, naming and idioms of the surrounding code.

## Repository layout

- `buildbox/` — all Rust sources, one Bazel package per component.
- `bazel/` — Bazel definitions: toolchains, macros, tooling targets.
- `third_party/` — `*.MODULE.bazel` files included from the root
  `MODULE.bazel` via `third_party/third_party.MODULE.bazel`, plus BUILD
  wrappers for external dependencies.

## Bazel conventions

- List `srcs` explicitly; don't use recursive globs.
- Packages are private by default; grant visibility deliberately.
- Interfaces (traits) and implementations are separate targets, so the build
  graph enforces the layering.

## Workflow

- `bazel build //...` and `bazel test //...` must pass before committing.
- Make small, focused commits that each leave the tree building.
- Always benchmark and profile release builds: `bazel build --config=release`
  (`-c opt` with LTO; see `.bazelrc`). The default fastbuild mode has no
  optimizations and keeps debug assertions, which distorts every CPU-bound
  measurement.
