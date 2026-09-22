# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends libsqlite3-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --workspace \
    && strip target/release/sqlite-broker target/release/libsqlite_broker.so

FROM debian:bookworm-slim
ARG VERSION=0.1.1
LABEL org.opencontainers.image.source="https://github.com/ugorur/sqlite-broker" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.licenses="MIT"
RUN apt-get update \
    && apt-get install -y --no-install-recommends libsqlite3-0 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --home-dir /var/lib/sqlite-broker broker \
    && mkdir -p /data /stub /shim \
    && chown broker:broker /data /stub /shim
COPY --from=build /src/target/release/sqlite-broker /usr/local/bin/sqlite-broker
COPY --from=build /src/target/release/libsqlite_broker.so /usr/local/lib/libsqlite_broker.so
ENV LD_LIBRARY_PATH=/usr/local/lib
USER broker
EXPOSE 7432
ENTRYPOINT ["sqlite-broker"]
CMD ["version"]
