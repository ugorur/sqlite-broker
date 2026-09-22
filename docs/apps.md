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
| [Datasette](https://datasette.io/) | Python standard-library `sqlite3` | [`examples/apps/datasette/docker-compose.yml`](../examples/apps/datasette/docker-compose.yml) |
| [Nextcloud](https://nextcloud.com/) | Official image defaults to SQLite through PHP `pdo_sqlite` | [`examples/apps/nextcloud/docker-compose.yml`](../examples/apps/nextcloud/docker-compose.yml) |
| [FreshRSS](https://freshrss.org/) | PHP, SQLite is a normal install option, database file under the data volume | [`examples/apps/freshrss/docker-compose.yml`](../examples/apps/freshrss/docker-compose.yml) |

The same shape covers other PHP applications that select SQLite (Wallabag, Shaarli) and other Python applications whose `sqlite3` module is the system one. Mount the stub at the path the application opens, set `LD_PRELOAD`, and leave `app.sqlite` on the broker only.

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
