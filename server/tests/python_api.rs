//! CPython's sqlite3 module, linked to libsqlite3.so, drives the shim.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[test]
fn cpython_sqlite_roundtrip_blocks_behind_a_transaction() {
    let python = PathBuf::from("/usr/bin/python3");
    assert!(
        python.is_file(),
        "expected a dynamically linked CPython at {}",
        python.display()
    );
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_sqlite-broker"));
    let shim = PathBuf::from(env!("SQLITE_BROKER_SHIM"));
    let dir = std::env::temp_dir().join(format!(
        "sqlite-broker-py-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let storage = dir.join("app.sqlite");
    let stub = dir.join("db.sqlite");
    let script = dir.join("client.py");
    fs::write(&script, PYTHON).unwrap();

    let mut server = Command::new(&bin)
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
        .expect("spawn broker");
    let ready = wait_for_file(&stub, Duration::from_secs(5));
    assert!(ready, "stub was not written");

    let output = Command::new(&python)
        .arg(&script)
        .arg(&stub)
        .env("LD_PRELOAD", &shim)
        .env_remove("PYTHONSTARTUP")
        .output()
        .expect("run python");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = server.kill();
    let _ = server.wait();
    assert!(
        output.status.success(),
        "python failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("row a 1.5 0001ff"),
        "missing float/blob row\n{stdout}"
    );
    assert!(
        stdout.contains("row b 2.0 hi"),
        "missing named row\n{stdout}"
    );
    assert!(
        stdout.contains("queued_saw 4"),
        "second connection did not wait for the open transaction\n{stdout}\n{stderr}"
    );
    assert!(stdout.contains("script_ok s"), "executescript\n{stdout}");
    assert!(
        stdout.contains("two_statements_rejected"),
        "tail was not reported\n{stdout}"
    );

    let audit = Command::new("/usr/bin/sqlite3")
        .args([
            storage.to_str().unwrap(),
            "SELECT name || ' ' || score || ' ' || hex(blob) FROM note ORDER BY id;",
        ])
        .env_remove("LD_PRELOAD")
        .output()
        .expect("sqlite3 audit");
    let body = String::from_utf8_lossy(&audit.stdout);
    assert!(
        audit.status.success(),
        "audit failed: {}",
        String::from_utf8_lossy(&audit.stderr)
    );
    assert!(
        body.contains("a 1.5 0001FF") || body.contains("a 1.5 0001ff"),
        "storage file missing the python write:\n{body}"
    );
    assert!(body.contains("held"), "held row missing from storage:\n{body}");
    let _ = fs::remove_dir_all(&dir);
}

fn wait_for_file(path: &Path, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if fs::read(path).map(|bytes| !bytes.is_empty()).unwrap_or(false) {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

const PYTHON: &str = r#"
import sqlite3, sys, threading, time
path = sys.argv[1]
con = sqlite3.connect(path)
con.execute("create table note(id integer primary key, name text, score real, blob blob)")
con.execute("insert into note(name, score, blob) values (?, ?, ?)", ("a", 1.5, b"\x00\x01\xff"))
con.execute(
    "insert into note(name, score, blob) values (:name, :score, :blob)",
    {"name": "b", "score": 2, "blob": b"hi"},
)
con.executemany(
    "insert into note(name, score, blob) values (?, ?, ?)",
    [("batch", 4, b"zz")],
)
con.executescript("create table extra(v text); insert into extra(v) values ('s');")
con.commit()
for name, score, blob in con.execute("select name, score, blob from note order by id"):
    shown = blob.hex() if name == "a" else blob.decode()
    print(f"row {name} {score} {shown}")
print("script_ok", con.execute("select v from extra").fetchone()[0])
try:
    con.execute("select 1; select 2")
    print("two_statements_accepted")
except sqlite3.ProgrammingError:
    print("two_statements_rejected")

holder = sqlite3.connect(path)
holder.execute("insert into note(name, score, blob) values ('held', 9, ?)", (b"z",))
seen = []
def reader():
    other = sqlite3.connect(path)
    seen.append(other.execute("select count(*) from note").fetchone()[0])
thread = threading.Thread(target=reader)
thread.start()
time.sleep(0.4)
if not thread.is_alive():
    print("queued_saw", "raced", seen)
else:
    holder.commit()
    thread.join(timeout=5)
    print("queued_saw", seen[0] if seen else "missing")
"#;
