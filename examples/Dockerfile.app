# Copy the preload library into an application image.
# The application binary must link libsqlite3.so.0, not a private copy of SQLite.
#
#   docker build -f examples/Dockerfile.app -t your-app:0.1.0 .

ARG SQLITE_BROKER_IMAGE=ghcr.io/ugorur/sqlite-broker:0.1.0
FROM ${SQLITE_BROKER_IMAGE} AS sqlite-broker

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends libsqlite3-0 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=sqlite-broker /usr/local/lib/libsqlite_broker.so /usr/local/lib/libsqlite_broker.so
COPY --from=sqlite-broker /usr/local/bin/sqlite-broker /usr/local/bin/sqlite-broker
ENV LD_PRELOAD=/usr/local/lib/libsqlite_broker.so
# Replace this with the command that opens /etc/sqlite-broker/db.sqlite.
CMD ["sqlite-broker", "version"]
