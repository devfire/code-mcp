# ============================================================================
# Stage 1: Chef — prepare recipe for dependency caching
# ============================================================================
FROM rust:1.87-slim AS chef

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

RUN cargo install cargo-chef --locked
WORKDIR /app

# ============================================================================
# Stage 2: Planner — compute dependency recipe (cache-key)
# ============================================================================
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo chef prepare --recipe-path recipe.json

# ============================================================================
# Stage 3: Builder — build dependencies (cached) then application
# ============================================================================
FROM chef AS builder

COPY --from=planner /app/recipe.json recipe.json

# Build dependencies only — this layer is cached until Cargo.toml/lock change
RUN cargo chef cook --release --recipe-path recipe.json

# Now build the actual application
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release \
    && strip target/release/code-mcp

# ============================================================================
# Stage 4: Runtime — minimal distroless-style image
# ============================================================================
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        git \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --shell /bin/false mcp

COPY --from=builder /app/target/release/code-mcp /usr/local/bin/code-mcp

USER mcp
WORKDIR /project

EXPOSE 8080

# All CLI flags are passed directly via CMD/args — ENTRYPOINT is the binary
ENTRYPOINT ["code-mcp"]
# Sensible defaults: bind all interfaces, project root = /project
CMD ["--bind", "0.0.0.0:8080", "--project", "/project"]
