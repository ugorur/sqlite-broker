# Docker Swarm

The stack is one broker replica and three application replicas. The SQLite file lives on a volume attached to the broker. The stub is a Swarm config, so every application replica sees the same broker address without sharing the database file.

Deploy [`examples/swarm/stack.yml`](../examples/swarm/stack.yml):

```bash
docker swarm init   # if this node is not already a manager
docker stack deploy -c examples/swarm/stack.yml sqlite-broker
docker service logs sqlite-broker_app
```

## Services

`broker` has `deploy.replicas: 1` and a named volume at `/data`. It listens on `0.0.0.0:7432` and advertises `broker:7432`, which is the Swarm service name on the stack network.

`app` has `deploy.replicas: 3`. The stub config is mounted at `/etc/sqlite-broker/db.sqlite`:

```text
SQLITEBROKER1
broker:7432
```

`LD_PRELOAD` points at `/usr/local/lib/libsqlite_broker.so` inside the image. The example process inserts a row in a loop. Swap in your own image using [`examples/Dockerfile.app`](../examples/Dockerfile.app), and keep the preload and the stub path.

Place the broker volume on one node (`deploy.placement` is left unset so Swarm can schedule it). Do not use a shared network filesystem for `app.sqlite`. The volume must be local to the broker container.

## Checks

```bash
docker service ls
docker service ps sqlite-broker_broker
docker exec $(docker ps -q -f name=sqlite-broker_broker) sqlite-broker version
```

Remove the stack with `docker stack rm sqlite-broker`. The volume remains until you delete it.
