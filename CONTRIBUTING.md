# Contributing to Obscura

Thanks for your interest in Obscura. This guide covers how to build, test, and
submit changes. For the deeper, non-obvious engine details (architecture,
gotchas, robustness invariants), read [AGENTS.md](AGENTS.md) first; this file
does not repeat it.

## Code of conduct

Be respectful and constructive. We want Obscura to be a welcoming project, so
keep discussion focused on the work and assume good faith. Harassment or abuse
is not tolerated.

## Before you start

For anything beyond a minor documentation fix, **please open an issue first** (or
comment on an existing one) and say you intend to work on it. This lets us give
early feedback and avoids two people building the same thing or a PR that does
not fit the project's direction.

A few notes to keep the project maintainable:

- **Link an issue.** PRs without a linked issue or prior discussion may be
  closed, except for small doc fixes.
- **Human oversight required.** AI-assisted contributions are fine, but
  low-quality or unreviewed agent output will be closed. Understand and test
  what you submit.
- New to the codebase? Look for issues labeled `good first issue`.

## Building

```bash
cargo build --release        # binary at ./target/release/obscura
```

- The first build compiles V8 from source: roughly 5 minutes and a few GB of
  disk. Incremental builds are seconds.
- Iterating on one crate? Scope it: `cargo build -p obscura-cli`.
- **Stealth** (`--features stealth`) pulls in BoringSSL through CMake, so `cmake`
  must be installed. The default build uses rustls and needs neither CMake nor
  OpenSSL.
- If the vendored OpenSSL build hits an AVX-512 assembler error on your host,
  build with `OPENSSL_NO_VENDOR=1`.

## Testing

Run tests with **`cargo nextest`, not `cargo test`**:

```bash
cargo nextest run --workspace        # or -p <crate> while iterating
```

`cargo test` runs the whole test binary in one process, but the engine holds a
single V8 isolate per process, so the runtime tests fail under it. `nextest`
runs each test in its own process, which is the only supported way.

The authoritative behavioral gate is the **obstacle course** in the companion
repo [`obscura-benchmark`](https://github.com/h4ckf0r0day/obscura-benchmark)
(33 capability and speed stages, must stay 33/33):

```bash
OBSCURA_BIN=./target/release/obscura python3 obstacle-course/run.py --runs 1 --warmup 0
```

It serves local fixtures, so it is deterministic and offline.

## Before you open a PR

For any code change:

1. `cargo build --release` (or `-p <crate>`) compiles clean.
2. `cargo nextest run` passes for the crates you touched.
3. The obstacle course still reports **33/33**.
4. **Performance is a hard constraint.** Obscura is roughly 12x faster and uses
   about 6x less memory than headless Chrome on framework pages. Keep the native
   Rust fast paths and add a JS fallback only for real spec edge cases. If your
   change could affect performance, benchmark old vs new interleaved, min-of-N
   (the noise floor is about plus or minus 10%).
5. For stealth changes, re-test with `--stealth`. A non-stealth binary does not
   exercise the `wreq` path.

Keep ops panic-safe: a panic in an op must degrade to a null result, never
unwind into V8's FFI frame. Do not remove the robustness guards described in
AGENTS.md (the V8 watchdog, the `tree.rs` reparenting guards, the CLI deadline).

Do not bulk-run `cargo fmt`. The tree is not rustfmt-clean, so a blanket format
produces a large unrelated diff. Match the surrounding style in the files you
edit. Comments should explain non-obvious "why", not restate the code.

## Commit messages

Keep them short and factual: what changed and why. We use a lightweight
`type(scope): summary` style, matching the existing history:

```
fix(cdp): honor text selection on Backspace and typing

Backspace trimmed the last character and typing always appended, ignoring
any selection. Both now respect a non-collapsed selection.

Fixes #316.
```

- `type` is one of `fix`, `feat`, `docs`, `test`, `perf`, `chore`. The `scope`
  is optional and lowercase (for example `cdp`, `js`, `net`, `stealth`, `cli`).
- No em dashes. Use commas, periods, or restructure the sentence.
- No AI-generated filler ("This commit improves...", "As an AI...").
- Do not add `Co-Authored-By` lines or list yourself as a co-author.

## Pull requests

- All submissions are reviewed; a maintainer merges after approval.
- Keep the diff small and readable. One logical change per PR. Split large
  contributions into several PRs.
- Reference the issue the PR closes, and say how you verified it (the test or
  repro that now passes).

## Reporting bugs

Open an issue with enough detail to reproduce:

- The obscura version or commit, plus OS and architecture.
- A repro: a URL, an `--eval` snippet, or a short CDP sequence.
- What you expected and what actually happened.
- If it is a rendering or compatibility issue, whether headless Chrome behaves
  the same. Anti-bot, CAPTCHA, and login walls block headless Chrome too from a
  datacenter IP, so those are not engine bugs.

Multi-statement `--eval` that starts with `const` returns `null` (V8 gives
`const` an empty completion value). Wrap repro snippets in an IIFE:
`(function(){ ...; return result; })()`.

**Security issues:** do not open a public issue. See [SECURITY.md](SECURITY.md)
for private reporting.

## Scope and direction

Obscura targets web scraping and AI-agent automation, and is heading toward a
hosted cloud scraping service. The priorities are real-world render success and
robustness (no crashes or hangs). Conformance and new Web APIs are welcome when
they do not regress performance or stability.

The stealth features are privacy-first anti-fingerprinting: they present a
normal, consistent browser identity so ordinary automation is not singled out.
Contributions that add detection-evasion for abusive purposes are out of scope.

## License

By contributing, you agree that your contributions are licensed under the
[Apache License 2.0](LICENSE), the same license as the project.
