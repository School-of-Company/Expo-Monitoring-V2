# Rust digest (docs: README.md). Static musl binary on distroless: CA certs and a nonroot user included,
# time zone data is compiled into the binary (chrono-tz).
FROM rust:1.97-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
# dependencies first, so source edits reuse the cached layer
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs && cargo build --release --locked && rm -rf src
COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /src/target/release/digest-rs /usr/local/bin/digest
CMD ["digest", "serve"]
