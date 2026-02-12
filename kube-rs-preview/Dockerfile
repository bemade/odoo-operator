# ── Build stage ────────────────────────────────────────────────────────────────
FROM rust:1.88-bookworm AS builder

WORKDIR /workspace

# Cache dependency build: copy manifests first, create dummy src, build deps.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src/bin && \
    echo 'fn main() {}' > src/bin/crdgen.rs && \
    echo 'fn main() {}' > src/main.rs && \
    echo '' > src/lib.rs && \
    cargo build --release 2>/dev/null || true && \
    rm -rf src \
          target/release/deps/odoo_operator* target/release/deps/libodoo_operator* \
          target/release/odoo-operator target/release/odoo_operator* \
          target/release/.fingerprint/odoo-operator-*

# Copy real source + scripts.
COPY src/ src/
COPY scripts/ scripts/
COPY tests/ tests/

# Build the operator binary.
RUN cargo build --release --bin odoo-operator

# ── Runtime stage ─────────────────────────────────────────────────────────────
FROM gcr.io/distroless/cc-debian12:nonroot

WORKDIR /
COPY --from=builder /workspace/target/release/odoo-operator /manager

USER 65532:65532

ENTRYPOINT ["/manager"]
