# syntax=docker/dockerfile:1.11@sha256:10c699f1b6c8bdc8f6b4ce8974855dd8542f1768c26eb240237b8f1c9c6c9976
# check=error=true

FROM rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS build

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --locked --release \
    && install -D -o 10001 -g 10001 -m 0755 target/release/kanata /out/usr/local/bin/kanata \
    && install -d -o 10001 -g 10001 -m 0700 /out/var/lib/kanata/codex \
    && install -D -o 10001 -g 10001 -m 0600 /dev/null /out/var/lib/kanata/codex/.volume-owner

FROM gcr.io/distroless/cc-debian13:nonroot@sha256:54df941ed0d06a1bd95ef5e0ce391fd8d9f94b64782dc9a60062727849ee3f97 AS runtime

COPY --from=build --chown=10001:10001 /out/usr/local/bin/kanata /usr/local/bin/kanata
COPY --from=build --chown=10001:10001 --chmod=0700 /out/var/lib/kanata/codex /var/lib/kanata/codex
COPY --from=build --chown=10001:10001 --chmod=0600 /out/var/lib/kanata/codex/.volume-owner /var/lib/kanata/codex/.volume-owner

USER 10001:10001
WORKDIR /var/lib/kanata
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/kanata"]
CMD ["serve", "--config", "/etc/kanata/config.toml"]
