# syntax=docker/dockerfile:1.11@sha256:10c699f1b6c8bdc8f6b4ce8974855dd8542f1768c26eb240237b8f1c9c6c9976
# check=error=true

FROM rust:1.99.1-bookworm@sha256:48b7f435b8fc6378ce43b44f15e3a47fd8919b2825f835c39bd4bd3da50af5b8 AS build

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --locked --release \
    && install -D -o 10001 -g 10001 -m 0755 target/release/kanata /out/usr/local/bin/kanata

FROM gcr.io/distroless/cc-debian13:nonroot@sha256:83b9b50586253821cf63ffcec87d3d94265a093ad30da232039af962dd8f4edf AS runtime

COPY --from=build --chown=10001:10001 /out/usr/local/bin/kanata /usr/local/bin/kanata

USER 10001:10001
WORKDIR /var/lib/kanata
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/kanata"]
CMD ["serve", "--config", "/etc/kanata/config.toml"]
