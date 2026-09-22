//! Three real processes open the replacement path with sqlite3_open and hit one
//! shared database. One holds a transaction; the others block until it commits.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Lines {
    rx: Receiver<String>,
    buf: Vec<String>,
}

impl Lines {
    fn from_reader<R: Read + Send + 'static>(reader: R) -> Self {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let reader = std::io::BufReader::new(reader);
            for line in std::io::BufRead::lines(reader) {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            rx,
            buf: Vec::new(),
        }
    }

    fn drain(&mut self) {
        while let Ok(line) = self.rx.try_recv() {
            self.buf.push(line);
        }
    }

    fn snapshot(&mut self) -> String {
        self.drain();
        self.buf.join("\n")
    }

    fn wait_has(&mut self, pred: impl Fn(&str) -> bool, timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            self.drain();
            if self.buf.iter().any(|line| pred(line)) {
                return true;
            }
            if start.elapsed() > timeout {
                return false;
            }
            match self.rx.recv_timeout(Duration::from_millis(30)) {
                Ok(line) => self.buf.push(line),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.drain();
                    return self.buf.iter().any(|line| pred(line));
                }
            }
        }
    }
}

struct Server {
    child: Child,
    out: Lines,
    err: Lines,
}

impl Server {
    fn spawn(bin: &Path, storage: &Path, stub: &Path) -> Self {
        let mut child = Command::new(bin)
            .args([
                "serve",
                "--storage",
                storage.to_str().unwrap(),
                "--stub",
                stub.to_str().unwrap(),
            ])
            .env_remove("LD_PRELOAD")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sqlite-broker serve");
        let out = Lines::from_reader(child.stdout.take().unwrap());
        let err = Lines::from_reader(child.stderr.take().unwrap());
        let mut server = Self { child, out, err };
        let ready = server
            .out
            .wait_has(|line| line.starts_with("ready "), Duration::from_secs(5));
        assert!(
            ready,
            "server did not become ready\nstdout:\n{}\nstderr:\n{}",
            server.out.snapshot(),
            server.err.snapshot()
        );
        server
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        reap(&mut self.child);
    }
}

struct Client {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    out: Lines,
    err: Lines,
}

impl Client {
    fn spawn(bin: &Path, shim: &Path, stub: &Path) -> Self {
        let mut child = Command::new(bin)
            .args(["session", "--db", stub.to_str().unwrap()])
            .env("LD_PRELOAD", shim)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sqlite-broker session");
        let out = Lines::from_reader(child.stdout.take().unwrap());
        let err = Lines::from_reader(child.stderr.take().unwrap());
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            out,
            err,
        }
    }

    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}").expect("write session command");
        stdin.flush().expect("flush session command");
    }

    fn close_stdin(&mut self) {
        self.stdin.take();
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        reap(&mut self.child);
    }
}

fn reap(child: &mut Child) {
    match child.try_wait() {
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
        }
        _ => {}
    }
}

fn wait_deadline(child: &mut Child, timeout: Duration) -> ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            return child.wait().expect("wait after kill");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

struct Tmp(PathBuf);

impl Tmp {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "sqlite-broker-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn artifacts() -> (PathBuf, PathBuf) {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_sqlite-broker"));
    let shim = PathBuf::from(env!("SQLITE_BROKER_SHIM"));
    assert!(
        shim.is_file(),
        "missing shim at {} — build the sqlite-broker-shim package",
        shim.display()
    );
    (bin, shim)
}

fn run_call(bin: &Path, shim: &Path, stub: &Path, sqls: &[&str]) -> String {
    let mut cmd = Command::new(bin);
    cmd.arg("call").arg("--db").arg(stub);
    for sql in sqls {
        cmd.arg("--sql").arg(sql);
    }
    cmd.env("LD_PRELOAD", shim)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn sqlite-broker call");
    let status = wait_deadline(&mut child, Duration::from_secs(10));
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr);
    }
    assert!(
        status.success(),
        "sqlite-broker call failed {status}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    stdout
}

fn raw_sqlite(storage: &Path, sql: &str) -> String {
    let out = Command::new("/usr/bin/sqlite3")
        .arg(storage)
        .arg(sql)
        .env_remove("LD_PRELOAD")
        .output()
        .unwrap_or_else(|err| panic!("spawn /usr/bin/sqlite3: {err}"));
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        text.push_str(&format!("\nexit={}\n", out.status));
    }
    text
}

fn assert_not_finished(name: &str, out: &str) {
    let lower = out.to_ascii_lowercase();
    assert!(
        !out.contains("OK") && !out.contains("ROW") && !out.contains("END") && !out.contains("ERR"),
        "{name} finished before the holder committed:\n{out}"
    );
    assert!(!lower.contains("busy"), "{name} saw SQLITE_BUSY:\n{out}");
    assert!(!lower.contains("locked"), "{name} saw a lock error:\n{out}");
}

fn rows_of(out: &str) -> Vec<&str> {
    out.lines()
        .filter_map(|line| line.strip_prefix("ROW "))
        .collect()
}

fn sqlite_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if !path.is_file() {
            continue;
        }
        let mut file = File::open(&path).unwrap();
        let mut buf = [0u8; 16];
        if file.read(&mut buf).ok() == Some(16) && buf.starts_with(b"SQLite format 3") {
            found.push(path);
        }
    }
    found.sort();
    found
}

#[test]
fn binary_dynamically_references_sqlite3_open() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_sqlite-broker"));
    let out = Command::new("nm")
        .args(["-D", bin.to_str().unwrap()])
        .output()
        .expect("nm");
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .find(|line| {
            line.split_whitespace()
                .any(|tok| tok.split('@').next() == Some("sqlite3_open"))
        })
        .unwrap_or_else(|| panic!("nm did not list sqlite3_open\n{text}"));
    assert!(
        line.split_whitespace().any(|tok| tok == "U"),
        "sqlite3_open is not a dynamic relocation, so LD_PRELOAD cannot replace it: {line}"
    );
}

#[test]
fn two_processes_insert_and_raw_sqlite_sees_the_row() {
    let (bin, shim) = artifacts();
    let dir = Tmp::new("roundtrip");
    let storage = dir.0.join("storage.sqlite");
    let stub = dir.0.join("db.sqlite");
    let server = Server::spawn(&bin, &storage, &stub);

    let sql = [
        "CREATE TABLE IF NOT EXISTS launch(v TEXT NOT NULL)",
        "INSERT INTO launch(v) VALUES ('sqlite-broker-launch-ok')",
        "SELECT v FROM launch WHERE rowid = last_insert_rowid()",
    ];
    let first = run_call(&bin, &shim, &stub, &sql);
    assert_eq!(
        first
            .lines()
            .filter(|line| *line == "cell:sqlite-broker-launch-ok")
            .count(),
        1,
        "first call did not select its own row\n{first}"
    );
    let raw_first = raw_sqlite(&storage, "SELECT v FROM launch ORDER BY rowid");
    assert!(
        raw_first.contains("sqlite-broker-launch-ok"),
        "unmodified sqlite3 did not see the first row\n{raw_first}"
    );

    let second = run_call(&bin, &shim, &stub, &sql);
    assert_eq!(
        second
            .lines()
            .filter(|line| *line == "cell:sqlite-broker-launch-ok")
            .count(),
        1,
        "second call did not select its own row\n{second}"
    );
    let raw_second = raw_sqlite(
        &storage,
        "SELECT v, COUNT(*) FROM launch GROUP BY v ORDER BY v",
    );
    assert_eq!(
        raw_second.trim(),
        "sqlite-broker-launch-ok|2",
        "shared file does not contain both commits\n{raw_second}"
    );
    println!("roundtrip_ok text=sqlite-broker-launch-ok");
    drop(server);
}

#[test]
fn three_clients_queue_onto_one_database() {
    let (bin, shim) = artifacts();
    let dir = Tmp::new("overlap");
    let storage = dir.0.join("storage.sqlite");
    let stub = dir.0.join("db.sqlite");
    let mut server = Server::spawn(&bin, &storage, &stub);

    let created = run_call(
        &bin,
        &shim,
        &stub,
        &["CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT NOT NULL)"],
    );
    assert!(created.contains("ok"), "{created}");

    let mut holder = Client::spawn(&bin, &shim, &stub);
    assert!(
        holder
            .out
            .wait_has(|line| line == "READY", Duration::from_secs(5)),
        "holder open failed\nstdout:\n{}\nstderr:\n{}",
        holder.out.snapshot(),
        holder.err.snapshot()
    );
    holder.send("EXEC BEGIN");
    holder.send("EXEC INSERT INTO t(v) VALUES ('row-a')");
    holder.send("HOLD");
    assert!(
        holder
            .out
            .wait_has(|line| line == "HOLDING", Duration::from_secs(5)),
        "holder did not reach HOLDING\nstdout:\n{}\nstderr:\n{}",
        holder.out.snapshot(),
        holder.err.snapshot()
    );

    let mut left = Client::spawn(&bin, &shim, &stub);
    let mut right = Client::spawn(&bin, &shim, &stub);
    left.send("EXEC INSERT INTO t(v) VALUES ('row-b')");
    left.send("QUERY SELECT v FROM t ORDER BY v");
    right.send("EXEC INSERT INTO t(v) VALUES ('row-c')");
    right.send("QUERY SELECT v FROM t WHERE v='row-c'");

    let started = Instant::now();
    loop {
        let left_out = left.out.snapshot();
        let right_out = right.out.snapshot();
        if left_out.contains("START") {
            assert_not_finished("left", &left_out);
        }
        if right_out.contains("START") {
            assert_not_finished("right", &right_out);
        }
        let log = server.err.snapshot();
        if left_out.contains("START")
            && right_out.contains("START")
            && log.matches("sqlite_broker_event=queued").count() >= 2
        {
            break;
        }
        if started.elapsed() > Duration::from_secs(8) {
            panic!(
                "clients were not queued behind the open transaction\nleft:\n{left_out}\nright:\n{right_out}\nserver:\n{log}\nleft-err:\n{}\nright-err:\n{}",
                left.err.snapshot(),
                right.err.snapshot()
            );
        }
        thread::sleep(Duration::from_millis(30));
    }

    let left_out = left.out.snapshot();
    let right_out = right.out.snapshot();
    assert_not_finished("left", &left_out);
    assert_not_finished("right", &right_out);
    let held_log = server.err.snapshot();
    assert!(
        held_log.matches("sqlite_broker_event=queued").count() >= 2,
        "{held_log}"
    );
    assert!(
        !held_log.contains("sqlite_broker_event=release"),
        "transaction released while the holder was still waiting\n{held_log}"
    );
    assert!(!held_log.contains("'row-b'"), "{held_log}");
    assert!(!held_log.contains("'row-c'"), "{held_log}");
    assert!(held_log.contains("'row-a'"), "{held_log}");

    let maps = fs::read_to_string(format!("/proc/{}/maps", left.child.id())).unwrap_or_default();
    assert!(
        maps.contains("libsqlite_broker.so"),
        "shim was not loaded into the client\n{maps}"
    );
    assert!(
        maps.contains("libsqlite3.so"),
        "client does not load libsqlite3.so\n{maps}"
    );

    let during = raw_sqlite(&storage, "SELECT COUNT(*) FROM t");
    assert_eq!(
        during.trim(),
        "0",
        "uncommitted or queued rows were visible in the shared file\n{during}"
    );

    holder.send("GO");
    holder.send("EXEC COMMIT");
    holder.send("QUERY SELECT v FROM t WHERE v='row-a'");

    assert!(
        holder
            .out
            .wait_has(|line| line == "END", Duration::from_secs(8)),
        "holder select did not finish\n{}\nserver:\n{}",
        holder.out.snapshot(),
        server.err.snapshot()
    );
    assert!(
        left.out
            .wait_has(|line| line == "END", Duration::from_secs(8)),
        "left client did not finish\nstdout:\n{}\nstderr:\n{}\nserver:\n{}",
        left.out.snapshot(),
        left.err.snapshot(),
        server.err.snapshot()
    );
    assert!(
        right
            .out
            .wait_has(|line| line == "END", Duration::from_secs(8)),
        "right client did not finish\nstdout:\n{}\nstderr:\n{}\nserver:\n{}",
        right.out.snapshot(),
        right.err.snapshot(),
        server.err.snapshot()
    );

    let log = server.err.snapshot();
    let release_at = log
        .find("sqlite_broker_event=release")
        .unwrap_or_else(|| panic!("missing release\n{log}"));
    let left_at = log
        .find("'row-b'")
        .unwrap_or_else(|| panic!("missing row-b exec\n{log}"));
    let right_at = log
        .find("'row-c'")
        .unwrap_or_else(|| panic!("missing row-c exec\n{log}"));
    assert!(
        release_at < left_at && release_at < right_at,
        "other clients ran inside the open transaction\n{log}"
    );

    let holder_out = holder.out.snapshot();
    let left_done = left.out.snapshot();
    let right_done = right.out.snapshot();
    for (name, out) in [
        ("holder", &holder_out),
        ("left", &left_done),
        ("right", &right_done),
    ] {
        let lower = out.to_ascii_lowercase();
        assert!(!lower.contains("busy"), "{name} saw SQLITE_BUSY:\n{out}");
        assert!(!lower.contains("locked"), "{name} saw a lock error:\n{out}");
        assert!(!out.contains("ERR"), "{name} returned an error:\n{out}");
    }
    assert!(
        rows_of(&holder_out).contains(&"row-a"),
        "holder did not read its committed row\n{holder_out}"
    );
    let left_rows = rows_of(&left_done);
    assert!(
        left_rows.contains(&"row-a") && left_rows.contains(&"row-b"),
        "left client did not see the shared rows\n{left_done}"
    );
    assert_eq!(left_rows.iter().filter(|row| **row == "row-a").count(), 1);
    assert_eq!(left_rows.iter().filter(|row| **row == "row-b").count(), 1);
    assert!(
        rows_of(&right_done).contains(&"row-c"),
        "right client did not see its row\n{right_done}"
    );

    let grouped = raw_sqlite(&storage, "SELECT v, COUNT(*) FROM t GROUP BY v ORDER BY v");
    assert_eq!(
        grouped.trim(),
        "row-a|1\nrow-b|1\nrow-c|1",
        "shared file does not contain each committed write once\n{grouped}"
    );

    let stub_bytes = fs::read(&stub).unwrap();
    assert!(
        stub_bytes.starts_with(b"SQLITEBROKER1\n"),
        "replacement path was rewritten as a private database: {stub_bytes:?}"
    );
    let files = sqlite_files(&dir.0);
    assert_eq!(
        files,
        vec![storage.clone()],
        "more than one SQLite file exists: {files:?}"
    );

    holder.close_stdin();
    left.close_stdin();
    right.close_stdin();
    let holder_status = wait_deadline(&mut holder.child, Duration::from_secs(5));
    let left_status = wait_deadline(&mut left.child, Duration::from_secs(5));
    let right_status = wait_deadline(&mut right.child, Duration::from_secs(5));
    assert!(holder_status.success(), "{holder_status}");
    assert!(left_status.success(), "{left_status}");
    assert!(right_status.success(), "{right_status}");
    println!("overlap_ok rows=3 queued_before_release");
    drop(server);
}

#[test]
fn stub_publishes_the_advertise_address() {
    let (bin, _) = artifacts();
    let dir = Tmp::new("advertise");
    let storage = dir.0.join("storage.sqlite");
    let stub = dir.0.join("db.sqlite");
    let mut child = Command::new(&bin)
        .args([
            "serve",
            "--storage",
            storage.to_str().unwrap(),
            "--stub",
            stub.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--advertise",
            "broker:7432",
        ])
        .env_remove("LD_PRELOAD")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve");
    let mut out = Lines::from_reader(child.stdout.take().unwrap());
    let ready = out.wait_has(|line| line.starts_with("ready "), Duration::from_secs(5));
    let _ = child.kill();
    let _ = child.wait();
    assert!(ready, "{}", out.snapshot());
    let text = fs::read_to_string(&stub).unwrap();
    assert_eq!(text, "SQLITEBROKER1\nbroker:7432\n");
}
