## Requirements

- Rust 1.75+ ([rustup.rs](https://rustup.rs))
- C compiler (gcc or clang)
- ~5 GB free disk space (V8 compiles from source on first build)

First build takes about 5 minutes. Incremental builds are seconds.

## Build

```bash
git clone https://github.com/h4ckf0r0day/obscura.git
cd obscura
cargo build --release
```

Binary is at `./target/release/obscura`.

## With stealth

```bash
cargo build --release --features stealth
```

Adds TLS fingerprint randomization and the tracker blocklist. See [Configure stealth and proxies](Configure-stealth-and-proxies.md).

## OpenSSL on older systems

If the build fails on the vendored OpenSSL with an AVX-512 assembler error (common on older VPS hosts):

```bash
OPENSSL_NO_VENDOR=1 cargo build --release
```

Uses the system OpenSSL instead.

## Run from the build

```bash
./target/release/obscura --version
./target/release/obscura fetch https://example.com --eval "document.title"
```

Install system-wide:

```bash
cargo install --path crates/obscura-cli
```

## Tests

```bash
cargo test --release
```

Integration suite:

```bash
python3 tests/test_all.py
```
