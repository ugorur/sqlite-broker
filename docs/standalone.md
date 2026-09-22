# Standalone

One machine, one broker process, as many application processes as you need. The broker is the only process that opens the real SQLite file.

## Build

```bash
cargo build --release --workspace
```

Binaries:

- `target/release/sqlite-broker`
- `target/release/libsqlite_broker.so`

`libsqlite3.so` must be on the loader path. Install the system SQLite package first.

## Start the broker

```bash
mkdir -p /var/lib/sqlite-broker
./target/release/sqlite-broker serve \
  --storage /var/lib/sqlite-broker/app.sqlite \
  --stub /var/lib/sqlite-broker/db.sqlite \
  --listen 127.0.0.1:7432
```

`--listen 127.0.0.1:7432` keeps clients on this machine. The stub is written as:

```text
SQLITEBROKER1
127.0.0.1:7432
```

Leave the stub where the application already expects its database path, or point the application at this path.

## Open it from an application

The application process must load `libsqlite3.so` and the shim:

```bash
export LD_PRELOAD=/absolute/path/libsqlite_broker.so
export LD_LIBRARY_PATH=/absolute/path   # only if the loader cannot find libsqlite3.so
./your-app --database /var/lib/sqlite-broker/db.sqlite
```

A process that does not load the shim sees a file that is not a SQLite database and stops. It does not create a second database.

Check the real file with the system shell. Do not set `LD_PRELOAD` for this command:

```bash
sqlite3 /var/lib/sqlite-broker/app.sqlite "PRAGMA integrity_check"
```

## One transaction at a time

Each `sqlite3_open` is one session. `BEGIN` … `COMMIT` on that session runs without another session's statements mixed in. Other sessions block until the transaction finishes, then run in arrival order. The caller waits for its own result. There is no `SQLITE_BUSY` for the application to retry.

## Stop

Stop the application processes, then stop the broker. The storage file is an ordinary SQLite database and can be copied while the broker is stopped.
