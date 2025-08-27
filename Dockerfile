# --- Builder stage ---
    FROM rust:1.83-bookworm AS builder
    WORKDIR /app
    
    # If you don't have Cargo.lock locally, omit it:
    COPY Cargo.toml .
    RUN mkdir -p src && echo "fn main() {}" > src/main.rs && cargo build --release || true
    
    COPY src ./src
    RUN cargo build --release
    
    # --- Runtime stage ---
    FROM debian:bookworm-slim
    RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata && rm -rf /var/lib/apt/lists/*
    ENV TZ=America/New_York
    WORKDIR /app
    COPY --from=builder /app/target/release/spaceweather-watcher /usr/local/bin/spaceweather-watcher
    USER 1000:1000
    ENTRYPOINT ["/usr/local/bin/spaceweather-watcher"]
    