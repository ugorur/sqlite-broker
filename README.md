# sqlite-broker

Many processes open one SQLite file. One broker owns the file and runs their SQL in order.

An application keeps calling `sqlite3_open` on a path. That path is a small stub, not the database. A preload library sends each statement to a single broker process, which is the only program that opens the real SQLite file. While one session is inside a transaction, the others wait. They do not see uncommitted rows, and they do not have to retry `SQLITE_BUSY`.

This is for programs that already use SQLite and need more than one process. It is not replication, not a network filesystem, and not a second database engine.

## Requirements

- Rust and Cargo
- A dynamically linked `libsqlite3.so` (`libsqlite3-dev` on Debian, `sqlite` on Arch)
- Linux. The shim is an `LD_PRELOAD` library.

`/usr/bin/sqlite3` on many systems does not link `libsqlite3.so`, so preload does not apply to that binary. Use it to inspect the real storage file. Programs that embed SQLite statically, including `better-sqlite3`, are also outside the shim. Those need a build that links the system library.

## Guides

- [Standalone](docs/standalone.md) on one machine
- [Docker](docs/docker.md) and [`examples/docker-compose.yml`](examples/docker-compose.yml)
- [Kubernetes](docs/kubernetes.md) and [`examples/kubernetes/sqlite-broker.yaml`](examples/kubernetes/sqlite-broker.yaml)
- [Docker Swarm](docs/swarm.md) and [`examples/swarm/stack.yml`](examples/swarm/stack.yml)
- [Popular applications](docs/apps.md): Datasette, Linkding, Nextcloud, FreshRSS
- [Versions and image tags](docs/versioning.md)

## Build

```bash
cargo build --workspace
```

That produces:

- `target/debug/sqlite-broker`
- `target/debug/libsqlite_broker.so`

## Run

```bash
mkdir -p data
./target/debug/sqlite-broker serve \
  --storage data/app.sqlite \
  --stub data/db.sqlite
```

`data/app.sqlite` is the only real database. `data/db.sqlite` is a text stub:

```text
SQLITEBROKER1
127.0.0.1:12345
```

Point each application at the stub and load the shim:

```bash
LD_PRELOAD=./target/debug/libsqlite_broker.so \
  ./target/debug/sqlite-broker call \
  --db data/db.sqlite \
  --sql "CREATE TABLE IF NOT EXISTS note(v TEXT)" \
  --sql "INSERT INTO note(v) VALUES ('hello')" \
  --sql "SELECT v FROM note"
```

Read it back with the system shell, which opens the storage file directly:

```bash
sqlite3 data/app.sqlite "SELECT v FROM note"
```

If the shim is missing, SQLite refuses the stub. It is not a database file, so a process cannot quietly create a private copy.

## What a session guarantees

Each `sqlite3_open` gets its own server-side connection. `BEGIN` through `COMMIT` stay on that connection. Other sessions' statements wait until the transaction ends, then run in order. A caller blocks until its own result is ready.

The broker listens on `127.0.0.1` by default. The stub is an address, not a password. Keep it on a private network if you change `--listen`.

## Load

`storm` starts one broker and several processes. Each process migrates on startup, then mixes reads and writes. After they exit, the command opens the storage file with `/usr/bin/sqlite3` and checks that every reported write is actually there.

```bash
./target/debug/sqlite-broker storm --apps 10 --ops 1000
```

## Limits

- One writer. Throughput is one SQLite connection's write rate.
- The broker is a single process. When it stops, clients stop. The file is not split across them.
- The shim covers the calls ordinary SQL needs: open, exec, prepare, bind, step, and column reads. Custom SQL functions, virtual tables created by the application, and `sqlite3_load_extension` of a local library stay in the client process and are not forwarded.
- `BEGIN`, `COMMIT`, and the writes of a transaction must be separate statements or one script the shim runs as a unit. The broker does not invent a commit the application did not send.

## Layout

| Path | Role |
| --- | --- |
| `server/` | `sqlite-broker` binary. Owns the real file. |
| `shim/` | `libsqlite_broker.so`, loaded with `LD_PRELOAD`. |
| `protocol/` | Stub text and the length-prefixed JSON frames. |

## Development

```bash
cargo test --workspace
```

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE)
