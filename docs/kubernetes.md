# Kubernetes

One broker pod owns the PersistentVolumeClaim. Application pods only receive the stub from a ConfigMap. The Service name in that stub is how they reach the broker.

Apply [`examples/kubernetes/sqlite-broker.yaml`](../examples/kubernetes/sqlite-broker.yaml):

```bash
kubectl apply -f examples/kubernetes/sqlite-broker.yaml
kubectl -n sqlite-broker rollout status deploy/sqlite-broker
kubectl -n sqlite-broker logs deploy/app
```

## What the manifest does

- A `PersistentVolumeClaim` named `sqlite-data`, mounted at `/data` on the broker only.
- A broker `Deployment` with `replicas: 1`. The command is:

  ```text
  serve --storage /data/app.sqlite --stub /stub/db.sqlite
        --listen 0.0.0.0:7432 --advertise sqlite-broker:7432
  ```

  `/stub` on the broker is an emptyDir. Applications do not use it. The address applications dial is the Service, which is stable across pod restarts.

- A `Service` named `sqlite-broker` on port 7432.
- A `ConfigMap` whose file `db.sqlite` contains:

  ```text
  SQLITEBROKER1
  sqlite-broker:7432
  ```

- An application `Deployment` with `replicas: 3`. Each pod sets `LD_PRELOAD=/usr/local/lib/libsqlite_broker.so` and opens `/etc/sqlite-broker/db.sqlite`. The example command inserts a row every few seconds. Replace the image command with your process, and copy `libsqlite_broker.so` into that image the same way as [Docker](docker.md).

Do not mount `sqlite-data` into the application pods. Two pods opening `app.sqlite` is the failure this project exists to avoid.

## Checks

```bash
kubectl -n sqlite-broker exec deploy/sqlite-broker -- sqlite-broker version
kubectl -n sqlite-broker get pods
```

To read the database, scale the application deployment to zero, stop the broker, and run `sqlite3` against a copy of the volume. Do not open the live file from a second process.
