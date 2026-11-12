FROM rust:1-bookworm AS builder
WORKDIR /app

# Cache dependencies separately from source changes.
COPY Cargo.toml Cargo.lock ./
COPY plugin-sdk/Cargo.toml plugin-sdk/Cargo.toml
RUN mkdir -p src plugin-sdk/src plugin-sdk/tests && echo "fn main() {}" > src/main.rs && touch plugin-sdk/src/lib.rs plugin-sdk/tests/roundtrip.rs \
    && cargo build --release \
    && rm -rf src plugin-sdk/src plugin-sdk/tests

COPY plugin-sdk ./plugin-sdk
COPY src ./src
COPY assets ./assets
COPY migrations ./migrations
# Catalogue Zwift embarqué (include_str! dans src/zwift.rs).
COPY data ./data
# sqlx::migrate! embeds migration files at compile time, so touch main.rs to
# force a rebuild past the dummy binary cached above.
RUN touch src/main.rs plugin-sdk/src/lib.rs && cargo build --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/cyclai /usr/local/bin/cyclai

# Persisted outside the image: sqlite db lives here.
VOLUME ["/data"]
ENV DATABASE_URL=sqlite:///data/cyclai.db

EXPOSE 3000 3001
ENTRYPOINT ["cyclai"]
CMD ["serve"]
