# --- Stage 1: Builder ---
# Use the Alpine-based Rust image for a much smaller footprint and musl compilation
FROM rust:1.89-alpine AS builder

WORKDIR /usr/src/app

# 1. Install build dependencies
# musl-dev: Standard C library for Alpine
# pkgconfig & openssl-dev: Required for compiling reqwest/native-tls
# openssl-libs-static: REQUIRED for linking against OpenSSL statically on Alpine
RUN apk add --no-cache musl-dev pkgconfig openssl-dev openssl-libs-static

# 2. Cache Dependencies Layer
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && \
  echo "fn main() {println!(\"dummy\");}" > src/main.rs
RUN cargo build --release

# 3. Build Actual Application
RUN rm -f target/release/deps/eve_looter*
RUN rm -rf src

COPY src ./src
COPY templates ./templates

RUN cargo build --release

# --- Stage 2: Runtime ---
# Use Alpine for the smallest viable runtime (~5MB base)
FROM alpine:3.20

# 1. Install Runtime Dependencies
# ca-certificates: For HTTPS
# libgcc: For Rust stack unwinding
# openssl: Dynamic library for reqwest (if not statically linked)
RUN apk add --no-cache ca-certificates libgcc openssl

# 2. Security: Create a non-root user
# Alpine uses 'adduser' instead of 'useradd'
RUN adduser -D -g '' eveuser
USER eveuser
WORKDIR /home/eveuser

# 3. Copy the compiled binary from the builder stage
COPY --from=builder /usr/src/app/target/release/eve-looter ./eve-looter

# 4. Configuration
ENV RUST_LOG=info
EXPOSE 3000

# 5. Start
CMD ["./eve-looter"]
