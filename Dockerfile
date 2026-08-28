FROM rust:1-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        curl \
        ca-certificates \
        perl \
        make \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache dependency compilation by copying manifests first
COPY Cargo.toml Cargo.lock ./
COPY crates/obscura-dom/Cargo.toml       crates/obscura-dom/Cargo.toml
COPY crates/obscura-net/Cargo.toml       crates/obscura-net/Cargo.toml
COPY crates/obscura-browser/Cargo.toml   crates/obscura-browser/Cargo.toml
COPY crates/obscura-js/Cargo.toml        crates/obscura-js/Cargo.toml
COPY crates/obscura-gateway/Cargo.toml   crates/obscura-gateway/Cargo.toml

# Create stub src files so cargo can resolve the dependency graph
RUN for crate in obscura-dom obscura-net obscura-browser obscura-js; do \
        mkdir -p crates/$crate/src && echo "// stub" > crates/$crate/src/lib.rs; \
    done && \
    mkdir -p crates/obscura-gateway/src && \
    echo "fn main() {}" > crates/obscura-gateway/src/main.rs

RUN cargo build --release --bin obscura-gateway 2>/dev/null || true

ARG OBSCURA_VERSION

# Copy real sources and build
COPY crates/ crates/
RUN echo "Building Mirage version ${OBSCURA_VERSION:-from Cargo.toml}" && \
    touch crates/*/src/*.rs && cargo build --release --bin obscura-gateway

# ---

# distroless/cc: glibc + libgcc + CA certs only -- no shell, no package manager
FROM gcr.io/distroless/cc-debian12

COPY --from=builder /build/target/release/obscura-gateway /obscura-gateway

EXPOSE 3000

ENTRYPOINT ["/obscura-gateway"]
