//! Load driver shaped like ten copies of one application.
//! Each process migrates the shared file on startup, then does 1000 random
//! reads or writes. Writes are checked afterwards by opening the real file
//! with /usr/bin/sqlite3.

use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::capi::Db;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u32) -> u32 {
        (self.next() % u64::from(n)) as u32
    }
}

struct Claim {
    worker: u64,
    ops: u64,
    reads: u64,
    writes: u64,
    inserts: u64,
    updates: u64,
    deletes: u64,
    sum_seq: u64,
    sample: String,
    last: String,
}

pub fn run_worker(db_path: &Path, worker: u64, ops: u32, seed: u64) -> Result<(), String> {
    let started = Instant::now();
    let mut rng = Rng(seed | 1);
    let mut db = open_ready(db_path)?;
    let boot_ddl = boot(&db)?;
    let mut live: Vec<String> = Vec::new();
    let mut executed = 0u32;
    let mut reads = 0u32;
    let mut inserts = 0u32;
    let mut updates = 0u32;
    let mut deletes = 0u32;
    let mut sum_seq = 0u64;
    let mut sample = String::new();
    let mut last = String::new();
    for step in 0..ops {
        if step > 0 && step % 200 == 0 {
            drop(db);
            db = open_ready(db_path)?;
        }
        let write = rng.below(2) == 0;
        if write {
            let fresh = format!("w{worker}-s{step}");
            let kind = rng.below(3);
            let (sql, recorded) = if kind == 0 || live.is_empty() {
                live.push(fresh.clone());
                inserts += 1;
                (
                    format!(
                        "BEGIN IMMEDIATE; INSERT INTO item(worker, n, name, note, price, flag, seen) VALUES ({worker}, {step}, '{fresh}', 'row {step}', {step}.25, {flag}, 0); INSERT INTO event(worker, seq, kind, token) VALUES ({worker}, {step}, 'insert', '{fresh}'); COMMIT",
                        flag = step % 2
                    ),
                    fresh,
                )
            } else if kind == 1 {
                let name = live[rng.below(live.len() as u32) as usize].clone();
                updates += 1;
                (
                    format!(
                        "BEGIN IMMEDIATE; UPDATE item SET note = 'edited {step}', seen = seen + 1, price = COALESCE(price, 0) + 0.5 WHERE name = '{name}'; INSERT INTO event(worker, seq, kind, token) VALUES ({worker}, {step}, 'update', '{name}'); COMMIT"
                    ),
                    name,
                )
            } else {
                let name = live.pop().unwrap();
                deletes += 1;
                (
                    format!(
                        "BEGIN IMMEDIATE; DELETE FROM item WHERE name = '{name}'; INSERT INTO event(worker, seq, kind, token) VALUES ({worker}, {step}, 'delete', '{name}'); COMMIT"
                    ),
                    name,
                )
            };
            db.exec(&sql)?;
            if sample.is_empty() {
                sample = recorded.clone();
            }
            last = recorded;
            sum_seq += u64::from(step);
        } else {
            let sql = match rng.below(4) {
                0 => format!(
                    "SELECT id, name, note, price, seen FROM item WHERE worker = {worker} ORDER BY id DESC LIMIT 8"
                ),
                1 => "SELECT worker, c FROM item_count ORDER BY worker".to_string(),
                2 => format!(
                    "SELECT kind, count(*) FROM event WHERE worker = {worker} GROUP BY kind"
                ),
                _ => format!(
                    "SELECT i.name, e.kind FROM item i LEFT JOIN event e ON e.token = i.name AND e.worker = i.worker WHERE i.worker = {worker} ORDER BY i.id DESC LIMIT 8"
                ),
            };
            db.query(&sql)?;
            reads += 1;
        }
        executed += 1;
    }
    let writes = inserts + updates + deletes;
    let ms = started.elapsed().as_millis();
    println!(
        "worker={worker} ops={ops} executed={executed} reads={reads} writes={writes} inserts={inserts} updates={updates} deletes={deletes} sum_seq={sum_seq} sample={sample} last={last} boot_ddl={boot_ddl} sql_err=0 fatal=0 ms={ms}"
    );
    if executed != ops {
        return Err(format!("worker {worker} executed {executed} of {ops}"));
    }
    if writes == 0 {
        return Err(format!("worker {worker} performed no writes"));
    }
    Ok(())
}

fn open_ready(path: &Path) -> Result<Db, String> {
    let db = Db::open(path)?;
    db.exec("PRAGMA foreign_keys = ON")?;
    Ok(db)
}

fn boot(db: &Db) -> Result<u32, String> {
    db.exec(
        "CREATE TABLE IF NOT EXISTS schema_migration (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL
        )",
    )?;
    let mut applied = 1u32;
    applied += migrate(
        db,
        1,
        "base tables",
        "CREATE TABLE IF NOT EXISTS item (
            id INTEGER PRIMARY KEY,
            worker INTEGER NOT NULL,
            n INTEGER NOT NULL,
            name TEXT NOT NULL UNIQUE,
            note TEXT,
            price REAL,
            flag INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS event (
            worker INTEGER NOT NULL,
            seq INTEGER NOT NULL,
            kind TEXT NOT NULL,
            token TEXT NOT NULL,
            PRIMARY KEY (worker, seq)
        );
        CREATE INDEX IF NOT EXISTS item_worker ON item(worker, name);
        CREATE VIEW IF NOT EXISTS item_count AS
            SELECT worker, count(*) AS c FROM item GROUP BY worker",
    )?;
    applied += migrate(
        db,
        2,
        "add item.seen",
        "ALTER TABLE item ADD COLUMN seen INTEGER NOT NULL DEFAULT 0",
    )?;
    applied += migrate(
        db,
        3,
        "index item.seen",
        "CREATE INDEX IF NOT EXISTS item_seen ON item(seen)",
    )?;
    Ok(applied)
}

fn migrate(db: &Db, version: i32, name: &str, sql: &str) -> Result<u32, String> {
    db.exec("BEGIN IMMEDIATE")?;
    let already = match db.query(&format!(
        "SELECT count(*) FROM schema_migration WHERE version = {version}"
    )) {
        Ok(rows) => rows.first().and_then(|row| row.first()).map(String::as_str) == Some("1"),
        Err(err) => {
            let _ = db.exec("ROLLBACK");
            return Err(err);
        }
    };
    if already {
        db.exec("COMMIT")?;
        return Ok(0);
    }
    if let Err(err) = db.exec(sql) {
        if !benign_migration(&err) {
            let _ = db.exec("ROLLBACK");
            return Err(err);
        }
    }
    if let Err(err) = db.exec(&format!(
        "INSERT OR IGNORE INTO schema_migration(version, name) VALUES ({version}, '{name}')"
    )) {
        let _ = db.exec("ROLLBACK");
        return Err(err);
    }
    db.exec("COMMIT")?;
    Ok(1)
}

fn benign_migration(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("duplicate column") || lower.contains("already exists")
}

pub fn storm(apps: u32, ops: u32, dir: Option<PathBuf>) -> Result<(), String> {
    if apps < 2 {
        return Err("storm wants at least 2 apps".to_string());
    }
    let exe = env::current_exe().map_err(|err| err.to_string())?;
    let shim = exe
        .parent()
        .ok_or("binary has no directory")?
        .join("libsqlite_broker.so");
    if !shim.is_file() {
        return Err(format!("missing shim {}", shim.display()));
    }
    let root = match dir {
        Some(path) => path,
        None => {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|err| err.to_string())?
                .as_nanos();
            env::temp_dir().join(format!(
                "sqlite-broker-storm-{}-{nanos}",
                std::process::id()
            ))
        }
    };
    fs::create_dir_all(&root).map_err(|err| err.to_string())?;
    let storage = root.join("storage.sqlite");
    let stub = root.join("db.sqlite");
    let mut server = Command::new(&exe)
        .args([
            "serve",
            "--storage",
            storage.to_str().ok_or("storage path")?,
            "--stub",
            stub.to_str().ok_or("stub path")?,
        ])
        .env_remove("LD_PRELOAD")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(File::create(root.join("server.err")).map_err(|err| err.to_string())?)
        .spawn()
        .map_err(|err| format!("spawn serve: {err}"))?;
    let mut ready_lines = BufReader::new(server.stdout.take().unwrap()).lines();
    let ready = ready_lines
        .next()
        .transpose()
        .map_err(|err| err.to_string())?;
    if ready
        .as_deref()
        .is_none_or(|line| !line.starts_with("ready "))
    {
        let _ = server.kill();
        return Err(format!("server did not become ready: {ready:?}"));
    }

    let started = Instant::now();
    let mut children = Vec::new();
    for worker in 0..apps {
        let log = File::create(root.join(format!("worker-{worker}.log")))
            .map_err(|err| err.to_string())?;
        let err = File::create(root.join(format!("worker-{worker}.err")))
            .map_err(|err| err.to_string())?;
        let child = Command::new(&exe)
            .args([
                "hammer",
                "--db",
                stub.to_str().unwrap(),
                "--worker",
                &worker.to_string(),
                "--ops",
                &ops.to_string(),
                "--seed",
                &format!(
                    "{}",
                    0x9E37_79B9_u64.wrapping_add(u64::from(worker).wrapping_mul(0x1000))
                ),
            ])
            .env("LD_PRELOAD", &shim)
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(err)
            .spawn()
            .map_err(|err| format!("spawn worker {worker}: {err}"))?;
        children.push(child);
    }

    let mut failed = Vec::new();
    let mut claims = Vec::new();
    for (worker, mut child) in children.into_iter().enumerate() {
        let status = wait_child(&mut child, Duration::from_secs(180))?;
        let summary =
            fs::read_to_string(root.join(format!("worker-{worker}.log"))).unwrap_or_default();
        println!("{}", summary.trim());
        if !status.success() {
            let err =
                fs::read_to_string(root.join(format!("worker-{worker}.err"))).unwrap_or_default();
            failed.push(format!("worker {worker} {status}: {}", err.trim()));
            continue;
        }
        claims.push(parse_claim(summary.trim())?);
    }
    let elapsed = started.elapsed();
    let audit_error = audit(&storage, &stub, &claims);
    let _ = server.kill();
    let _ = server.wait();
    let bytes = fs::metadata(&storage).map(|meta| meta.len()).unwrap_or(0);
    println!(
        "apps={apps} ops_each={ops} elapsed_ms={} file_bytes={bytes} dir={}",
        elapsed.as_millis(),
        root.display()
    );
    if let Err(err) = audit_error {
        return Err(err);
    }
    if !failed.is_empty() {
        return Err(failed.join("\n"));
    }
    println!("storm_ok file_matches_writes");
    Ok(())
}

fn parse_claim(line: &str) -> Result<Claim, String> {
    let mut claim = Claim {
        worker: 0,
        ops: 0,
        reads: 0,
        writes: 0,
        inserts: 0,
        updates: 0,
        deletes: 0,
        sum_seq: 0,
        sample: String::new(),
        last: String::new(),
    };
    for part in line.split_whitespace() {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key {
            "worker" => claim.worker = value.parse().map_err(|err| format!("{err}"))?,
            "ops" => claim.ops = value.parse().map_err(|err| format!("{err}"))?,
            "reads" => claim.reads = value.parse().map_err(|err| format!("{err}"))?,
            "writes" => claim.writes = value.parse().map_err(|err| format!("{err}"))?,
            "inserts" => claim.inserts = value.parse().map_err(|err| format!("{err}"))?,
            "updates" => claim.updates = value.parse().map_err(|err| format!("{err}"))?,
            "deletes" => claim.deletes = value.parse().map_err(|err| format!("{err}"))?,
            "sum_seq" => claim.sum_seq = value.parse().map_err(|err| format!("{err}"))?,
            "sample" => claim.sample = value.to_string(),
            "last" => claim.last = value.to_string(),
            _ => {}
        }
    }
    if claim.reads + claim.writes != claim.ops {
        return Err(format!("worker {} reads+writes != ops", claim.worker));
    }
    Ok(claim)
}

fn audit(storage: &Path, stub: &Path, claims: &[Claim]) -> Result<(), String> {
    let header = fs::read(storage).map_err(|err| err.to_string())?;
    if !header.starts_with(b"SQLite format 3") {
        return Err("storage is not a SQLite file".to_string());
    }
    let stub_bytes = fs::read(stub).map_err(|err| err.to_string())?;
    if !stub_bytes.starts_with(b"SQLITEBROKER1\n") {
        return Err("replacement path is no longer a stub".to_string());
    }
    let integrity = raw_sql(storage, "PRAGMA integrity_check;")?;
    println!("--- /usr/bin/sqlite3 ---");
    println!("integrity={}", integrity.trim());
    if integrity.trim() != "ok" {
        return Err(format!("integrity check failed: {integrity}"));
    }
    let versions = raw_sql(
        storage,
        "SELECT version || ':' || name FROM schema_migration ORDER BY version;",
    )?;
    println!("migrations:\n{}", versions.trim());
    let seen = raw_sql(
        storage,
        "SELECT count(*) FROM pragma_table_info('item') WHERE name = 'seen';",
    )?;
    if seen.trim() != "1" {
        return Err(format!("ALTER column seen missing in the file: {seen}"));
    }
    let events = raw_sql(
        storage,
        "SELECT worker, count(*), coalesce(sum(seq), 0) FROM event GROUP BY worker ORDER BY worker;",
    )?;
    println!("events:\n{}", events.trim());
    let mut by_worker = std::collections::BTreeMap::new();
    for line in events.lines() {
        let mut cols = line.split('|');
        let worker: u64 = cols
            .next()
            .unwrap_or("")
            .parse()
            .map_err(|err| format!("event row {line}: {err}"))?;
        let count: u64 = cols
            .next()
            .unwrap_or("")
            .parse()
            .map_err(|err| format!("event row {line}: {err}"))?;
        let sum_seq: u64 = cols
            .next()
            .unwrap_or("")
            .parse()
            .map_err(|err| format!("event row {line}: {err}"))?;
        by_worker.insert(worker, (count, sum_seq));
    }
    for claim in claims {
        let Some((count, sum_seq)) = by_worker.get(&claim.worker) else {
            return Err(format!(
                "worker {} reported {} writes but the file has none",
                claim.worker, claim.writes
            ));
        };
        if *count != claim.writes || *sum_seq != claim.sum_seq {
            return Err(format!(
                "worker {} file has {count} writes sum {sum_seq}, process reported {} writes sum {}",
                claim.worker, claim.writes, claim.sum_seq
            ));
        }
        for token in [&claim.sample, &claim.last] {
            let found = raw_sql(
                storage,
                &format!(
                    "SELECT count(*) FROM event WHERE worker = {} AND token = '{token}'",
                    claim.worker
                ),
            )?;
            if found.trim() == "0" {
                return Err(format!(
                    "worker {} token {token} is missing from the file",
                    claim.worker
                ));
            }
        }
        let balance = raw_sql(
            storage,
            &format!(
                "SELECT (SELECT count(*) FROM event WHERE worker = {w} AND kind = 'insert') - (SELECT count(*) FROM event WHERE worker = {w} AND kind = 'delete'), (SELECT count(*) FROM item WHERE worker = {w})",
                w = claim.worker
            ),
        )?;
        let mut cols = balance.trim().split('|');
        let expected = cols.next().unwrap_or("");
        let actual = cols.next().unwrap_or("");
        if expected != actual {
            return Err(format!(
                "worker {} item rows {actual} but inserts-deletes is {expected}",
                claim.worker
            ));
        }
        println!(
            "audit worker={} writes={} sum_seq={} items={} match",
            claim.worker, count, sum_seq, actual
        );
    }
    Ok(())
}

fn raw_sql(storage: &Path, sql: &str) -> Result<String, String> {
    let out = Command::new("/usr/bin/sqlite3")
        .arg(storage)
        .arg(sql)
        .env_remove("LD_PRELOAD")
        .output()
        .map_err(|err| format!("sqlite3: {err}"))?;
    if !out.status.success() {
        return Err(format!(
            "sqlite3 failed {}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn wait_child(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<std::process::ExitStatus, String> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
            return Ok(status);
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("worker pid {} hung", child.id()));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

pub fn hammer_cli(args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut db = None;
    let mut worker = None;
    let mut ops = 1000u32;
    let mut seed = None;
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--db" => db = Some(PathBuf::from(need(&mut args, "--db")?)),
            "--worker" => {
                worker = Some(
                    need(&mut args, "--worker")?
                        .parse::<u64>()
                        .map_err(|err| err.to_string())?,
                )
            }
            "--ops" => {
                ops = need(&mut args, "--ops")?
                    .parse::<u32>()
                    .map_err(|err| err.to_string())?
            }
            "--seed" => {
                seed = Some(
                    need(&mut args, "--seed")?
                        .parse::<u64>()
                        .map_err(|err| err.to_string())?,
                )
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let db = db.ok_or("missing --db")?;
    let worker = worker.ok_or("missing --worker")?;
    let seed = seed.unwrap_or(0xA5A5_5A5A_u64 ^ worker.wrapping_mul(0x9E37));
    run_worker(&db, worker, ops, seed)
}

pub fn storm_cli(args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut apps = 10u32;
    let mut ops = 1000u32;
    let mut dir = None;
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--apps" => {
                apps = need(&mut args, "--apps")?
                    .parse::<u32>()
                    .map_err(|err| err.to_string())?
            }
            "--ops" => {
                ops = need(&mut args, "--ops")?
                    .parse::<u32>()
                    .map_err(|err| err.to_string())?
            }
            "--dir" => dir = Some(PathBuf::from(need(&mut args, "--dir")?)),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    storm(apps, ops, dir)
}

fn need(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("missing value for {flag}"))
}
