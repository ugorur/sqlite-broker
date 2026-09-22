# Popular applications

sqlite-broker sits in front of an application that already calls `sqlite3_open` through the system `libsqlite3.so`. The application keeps its own files, caches, and configuration. Only the database path changes: it must be the stub, and the process must load `libsqlite_broker.so`.

## Check the binary first

```bash
docker run --rm --entrypoint sh IMAGE -c 'ldd /path/to/program | grep libsqlite3 || true'
```

For Python:

```bash
docker run --rm --entrypoint python IMAGE -c 'import sqlite3,os; print(sqlite3.__file__)'
```

Then run `ldd` on that `_sqlite3` extension. If the line `libsqlite3.so` is there, the shim can interpose `sqlite3_open`. If it is not, the application has its own copy of SQLite and this project cannot see the calls.

## Works with the shim

These stacks link the system SQLite library. The Compose files live under [`examples/apps/`](../examples/apps/).

| Application | Why it fits | Example |
| --- | --- | --- |
| [Datasette](https://datasette.io/) | Python standard-library `sqlite3` on Debian | [`examples/apps/datasette/docker-compose.yml`](../examples/apps/datasette/docker-compose.yml) |
| [Linkding](https://github.com/sissbruecker/linkding) | Django, SQLite by default, CPython linked to `libsqlite3.so` | [`examples/apps/linkding/docker-compose.yml`](../examples/apps/linkding/docker-compose.yml) |
| [Nextcloud](https://nextcloud.com/) | Official Apache image uses PHP `pdo_sqlite` | [`examples/apps/nextcloud/docker-compose.yml`](../examples/apps/nextcloud/docker-compose.yml) |
| [FreshRSS](https://freshrss.org/) | PHP, SQLite is a normal install option, database file under the data volume | [`examples/apps/freshrss/docker-compose.yml`](../examples/apps/freshrss/docker-compose.yml) |

The shim covers the calls those stacks make on a connection: prepare, bind (including named parameters, floats, and blobs), step, and the busy-timeout and limit calls they issue at startup. One broker connection runs each statement to completion, so a transaction still blocks every other session.

CPython follows `LD_PRELOAD`. PHP loads `pdo_sqlite` with `RTLD_DEEPBIND`, which skips preload and binds `sqlite3_open` inside `libsqlite3.so.0`. The Nextcloud and FreshRSS examples therefore also set `LD_LIBRARY_PATH` to a directory where that file name is the shim. Symbols the shim does not implement jump to the system library.

A few SQLite features are accepted and then ignored, because they would have to run inside the application process:

- SQL functions and collations registered with `sqlite3_create_function` stay in the client. Linkding ships `libicu.so` for that. The Compose file removes it before startup, and search falls back to SQLite's own collation.
- Authorizer, trace, and progress hooks are not called.
- `sqlite3_backup_*`, incremental blob I/O, and `load_extension` return an error if the application actually uses them.

The published library is built on Debian and links glibc. An Alpine image can see `libsqlite3.so` in `ldd` and still refuse to load `libsqlite_broker.so`. Kanboard's official image is that case: its PHP `pdo_sqlite` module is a normal SQLite client, and a glibc build of Kanboard can use the same Compose shape as Nextcloud. The Alpine image cannot.

Linkding also opens `data/tasks.sqlite3` for background jobs. That is a second database. The example turns those jobs off. The broker serves the one file named by `--storage`.

## Does not work

These embed SQLite. `LD_PRELOAD` never sees `sqlite3_open`, so pointing them at the stub makes them treat it as a corrupt database file.

| Application | What it links instead |
| --- | --- |
| Grafana | `modernc.org/sqlite`, pure Go |
| Gitea and Forgejo current images | `modernc.org/sqlite` by default |
| n8n, Uptime Kuma, Ghost | `better-sqlite3`, SQLite compiled into the Node addon |
| Vaultwarden | `rusqlite` with SQLite bundled into the binary |
| Jellyfin, Sonarr, Radarr, Lidarr | a private native SQLite library, not `libsqlite3.so` |

A rebuild that links the system `libsqlite3.so` can join the table above. The stock image cannot.
