# Docker

The image `ghcr.io/ugorur/sqlite-broker` contains both the broker binary and `libsqlite_broker.so`. The broker container owns the database volume. Application containers use the same image, or copy the library into their own image, and set `LD_PRELOAD`.

The database volume is mounted only on the broker. Application containers mount the stub, which is a few lines of text naming the broker. They never mount `app.sqlite`.

## Build locally

```bash
docker build -t sqlite-broker:0.1.1 .
```

Published tags follow [versioning](versioning.md).

## Compose

[`examples/docker-compose.yml`](../examples/docker-compose.yml) starts one broker and three application replicas.

```bash
docker compose -f examples/docker-compose.yml up --build
```

The broker listens on `0.0.0.0:7432` inside the network and writes this stub:

```text
SQLITEBROKER1
broker:7432
```

`broker` is the Compose service name. Other containers resolve it. `--advertise` is required here: the bound address is `0.0.0.0`, which is not a host another container can dial.

The application service runs `sqlite-broker call` in a loop so you can watch rows land. Replace that command with your own process. The process must be linked to `libsqlite3.so`, and the container needs:

```text
LD_PRELOAD=/usr/local/lib/libsqlite_broker.so
```

## Put the shim in your image

[`examples/Dockerfile.app`](../examples/Dockerfile.app) copies the library out of the broker image:

```dockerfile
FROM ghcr.io/ugorur/sqlite-broker:0.1.1 AS sqlite-broker
FROM your-app
COPY --from=sqlite-broker /usr/local/lib/libsqlite_broker.so /usr/local/lib/libsqlite_broker.so
ENV LD_PRELOAD=/usr/local/lib/libsqlite_broker.so
```

Your image still needs `libsqlite3.so.0`. The application opens the stub path, for example `/etc/sqlite-broker/db.sqlite`.

## Inspect

```bash
docker compose -f examples/docker-compose.yml exec broker \
  sqlite-broker version
```

The broker image's `sqlite3` client is not installed. From the host, copy the file out or run a shell that has SQLite and mount only a stopped volume. While the broker is running, another container must not open `app.sqlite`.
