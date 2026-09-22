# Contributing

Issues and pull requests are welcome.

## Setup

Install Rust and the system SQLite shared library. On Debian or Ubuntu:

```bash
sudo apt install libsqlite3-dev
```

On Arch Linux the `sqlite` package is enough.

## Checks

```bash
cargo test --workspace
cargo fmt --all --check
```

Run the load driver when a change touches queuing or the shim:

```bash
cargo build --workspace
./target/debug/sqlite-broker storm --apps 10 --ops 1000
```

The storm opens `storage.sqlite` with `/usr/bin/sqlite3` and fails if a reported write is missing from that file.

## Scope

Keep the broker the only process that opens the real database file. A change that lets a client write its own SQLite file, or that returns `SQLITE_BUSY` for the caller to retry, is a bug.
