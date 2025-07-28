FROM rust:1.88-alpine3.22 AS builder
RUN apk add --no-cache musl-dev gcc openssl-dev pkgconfig

WORKDIR /usr/local/bin
COPY . .

RUN cargo build --release --manifest-path ./Cargo.toml

FROM amd64/alpine:3.22
RUN apk add --no-cache uv openssl python3 rust cargo ca-certificates curl 
COPY --from=builder /usr/local/bin/target/release/rsky-relay /usr/local/bin/rsky-relay
COPY --from=builder /usr/local/bin/rsky-relay/crawler.py /usr/local/bin/crawler.py

LABEL xyz.blacksky.version="0.0.9-beta"

WORKDIR /usr/local/bin

# HEALTHCHECK --interval=30s --timeout=30s --start-period=5s --retries=3 CMD [ "curl", "-f", "http://localhost:9000/", "||", "exit", "1" ]

RUN uv init . && uv add requests
RUN timeout -s INT 5 uv run crawler.py

ENV RUST_LOG="rsky-relay=debug"
ENTRYPOINT [ "rsky-relay" ]
CMD [ "--no-plc-export" ]
EXPOSE 9000

