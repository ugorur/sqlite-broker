//! One process owns the real SQLite file. Each client session has its own
//! connection. While a session is inside a transaction, every other session's
//! statement waits, so nothing lands inside that transaction.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use rusqlite::{ffi, Connection};
use sqlite_broker_proto::{Cell, Param, Request, Response};

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

struct Pending {
    id: u64,
    sql: String,
    params: Vec<Param>,
    reply: Sender<Response>,
}

enum Job {
    Open {
        id: u64,
        reply: Sender<Result<(), String>>,
    },
    Req {
        id: u64,
        req: Request,
        reply: Sender<Response>,
    },
    Drop {
        id: u64,
        reply: Sender<()>,
    },
}

struct Worker {
    storage: PathBuf,
    sessions: HashMap<u64, Connection>,
    /// Session whose connection is not in autocommit. Only that session runs.
    holder: Option<u64>,
    pending: VecDeque<Pending>,
}

pub fn serve(storage: PathBuf, stub: PathBuf, listen: &str) -> Result<(), String> {
    if let Some(parent) = storage.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
    }
    {
        let conn = open_storage(&storage)?;
        drop(conn);
    }

    let listener = TcpListener::bind(listen).map_err(|err| format!("listen {listen}: {err}"))?;
    let addr = listener
        .local_addr()
        .map_err(|err| format!("local_addr: {err}"))?;
    write_stub(&stub, &addr.to_string())?;

    let (tx, rx) = mpsc::channel();
    let accept_tx = tx.clone();
    thread::spawn(move || accept_loop(listener, accept_tx));
    drop(tx);

    println!(
        "ready listen={addr} stub={} storage={}",
        stub.display(),
        storage.display()
    );
    let _ = io::stdout().flush();

    worker_loop(rx, storage);
    Ok(())
}

fn write_stub(path: &Path, addr: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
    }
    let body = sqlite_broker_proto::format_stub(addr);
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, &body).map_err(|err| format!("write stub: {err}"))?;
    fs::rename(&tmp, path).map_err(|err| format!("rename stub: {err}"))?;
    Ok(())
}

fn open_storage(path: &Path) -> Result<Connection, String> {
    let mut last = None;
    for _ in 0..50 {
        match Connection::open(path) {
            Ok(conn) => {
                conn.busy_timeout(Duration::from_millis(0))
                    .map_err(|err| err.to_string())?;
                return Ok(conn);
            }
            Err(err) => {
                let msg = err.to_string().to_ascii_lowercase();
                if msg.contains("locked") || msg.contains("busy") {
                    last = Some(err.to_string());
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
                return Err(err.to_string());
            }
        }
    }
    Err(last.unwrap_or_else(|| "unable to open storage".to_string()))
}

fn worker_loop(rx: Receiver<Job>, storage: PathBuf) {
    let mut worker = Worker {
        storage,
        sessions: HashMap::new(),
        holder: None,
        pending: VecDeque::new(),
    };
    while let Ok(job) = rx.recv() {
        worker.handle(job);
    }
}

impl Worker {
    fn handle(&mut self, job: Job) {
        match job {
            Job::Open { id, reply } => {
                let opened = open_storage(&self.storage).map(|conn| {
                    self.sessions.insert(id, conn);
                });
                let _ = reply.send(opened);
                self.pump();
            }
            Job::Drop { id, reply } => {
                self.drop_session(id);
                let _ = reply.send(());
                self.pump();
            }
            Job::Req { id, req, reply } => match req {
                Request::Close => {
                    self.drop_session(id);
                    let _ = reply.send(ok_closed());
                    self.pump();
                }
                Request::Exec { sql, params } => {
                    if matches!(self.holder, Some(holder) if holder != id) {
                        event(&format!("sqlite_broker_event=queued session={id}"));
                        self.pending.push_back(Pending {
                            id,
                            sql,
                            params,
                            reply,
                        });
                        return;
                    }
                    self.run_exec(id, sql, params, reply);
                    self.pump();
                }
            },
        }
    }

    fn run_exec(&mut self, id: u64, sql: String, params: Vec<Param>, reply: Sender<Response>) {
        let response = match self.sessions.get(&id) {
            Some(conn) => execute(conn, &sql, &params),
            None => error_response(true, 1, "no session"),
        };
        let brief = sql_brief(&sql);
        event(&format!(
            "sqlite_broker_event=exec session={id} autocommit={} sql={brief}",
            u8::from(response.autocommit)
        ));
        let was_holder = self.holder == Some(id);
        if !response.autocommit {
            self.holder = Some(id);
        } else if was_holder {
            self.holder = None;
            event(&format!("sqlite_broker_event=release session={id}"));
        }
        let _ = reply.send(response);
    }

    fn pump(&mut self) {
        while self.holder.is_none() {
            let Some(pending) = self.pending.pop_front() else {
                return;
            };
            self.run_exec(pending.id, pending.sql, pending.params, pending.reply);
        }
    }

    fn drop_session(&mut self, id: u64) {
        self.sessions.remove(&id);
        let mut kept = VecDeque::new();
        while let Some(pending) = self.pending.pop_front() {
            if pending.id == id {
                let _ = pending
                    .reply
                    .send(error_response(true, 1, "session closed"));
            } else {
                kept.push_back(pending);
            }
        }
        self.pending = kept;
        if self.holder == Some(id) {
            self.holder = None;
            event(&format!("sqlite_broker_event=release session={id}"));
        }
    }
}

fn event(msg: &str) {
    eprintln!("{msg}");
    let _ = io::stderr().flush();
}

fn sql_brief(sql: &str) -> String {
    let one = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.len() > 120 {
        format!("{}...", &one[..120])
    } else {
        one
    }
}

fn execute(conn: &Connection, sql: &str, params: &[Param]) -> Response {
    let result = unsafe { execute_inner(conn, sql, params) };
    let autocommit = conn.is_autocommit();
    match result {
        Ok((columns, rows)) => Response {
            ok: true,
            message: String::new(),
            code: 0,
            columns,
            rows,
            changes: unsafe { ffi::sqlite3_changes(conn.handle()) },
            last_insert_rowid: unsafe { ffi::sqlite3_last_insert_rowid(conn.handle()) },
            autocommit,
        },
        Err((code, message)) => Response {
            ok: false,
            message,
            code,
            columns: Vec::new(),
            rows: Vec::new(),
            changes: 0,
            last_insert_rowid: unsafe { ffi::sqlite3_last_insert_rowid(conn.handle()) },
            autocommit,
        },
    }
}

unsafe fn execute_inner(
    conn: &Connection,
    sql: &str,
    params: &[Param],
) -> Result<(Vec<String>, Vec<Vec<Cell>>), (i32, String)> {
    let c_sql = std::ffi::CString::new(sql).map_err(|err| (1, err.to_string()))?;
    let db = conn.handle();
    let mut sql_ptr = c_sql.as_ptr();
    let start = sql_ptr as usize;
    let mut columns = Vec::new();
    let mut rows = Vec::new();
    let limit = c_sql.as_bytes().len() + 1;

    loop {
        let mut stmt = std::ptr::null_mut();
        let mut tail: *const std::os::raw::c_char = std::ptr::null();
        let rc = unsafe { ffi::sqlite3_prepare_v2(db, sql_ptr, -1, &mut stmt, &mut tail) };
        if rc != ffi::SQLITE_OK {
            return Err(sqlite_error(db));
        }
        if stmt.is_null() {
            break;
        }
        if !params.is_empty() {
            if let Err(err) = bind_all(stmt, params) {
                unsafe { ffi::sqlite3_finalize(stmt) };
                return Err((1, err));
            }
        }
        let ncols = unsafe { ffi::sqlite3_column_count(stmt) };
        if columns.is_empty() && ncols > 0 {
            for i in 0..ncols {
                let name_ptr = unsafe { ffi::sqlite3_column_name(stmt, i) };
                let name = if name_ptr.is_null() {
                    format!("col{i}")
                } else {
                    unsafe { std::ffi::CStr::from_ptr(name_ptr) }
                        .to_string_lossy()
                        .into_owned()
                };
                columns.push(name);
            }
        }
        loop {
            let rc = unsafe { ffi::sqlite3_step(stmt) };
            if rc == ffi::SQLITE_ROW {
                rows.push(read_row(stmt, ncols));
            } else if rc == ffi::SQLITE_DONE {
                break;
            } else {
                let err = sqlite_error(db);
                unsafe { ffi::sqlite3_finalize(stmt) };
                return Err(err);
            }
        }
        unsafe { ffi::sqlite3_finalize(stmt) };

        if tail.is_null() || unsafe { *tail } == 0 {
            break;
        }
        if !params.is_empty() {
            if tail_has_sql(tail) {
                return Err((1, "bound parameters require a single statement".to_string()));
            }
            break;
        }
        if (tail as usize) <= (sql_ptr as usize) || (tail as usize) - start > limit {
            break;
        }
        sql_ptr = tail;
    }
    Ok((columns, rows))
}

fn bind_all(stmt: *mut ffi::sqlite3_stmt, params: &[Param]) -> Result<(), String> {
    for param in params {
        let rc = match &param.value {
            Cell::Null => unsafe { ffi::sqlite3_bind_null(stmt, param.index) },
            Cell::Int { v } => unsafe { ffi::sqlite3_bind_int64(stmt, param.index, *v) },
            Cell::Real { v } => unsafe { ffi::sqlite3_bind_double(stmt, param.index, *v) },
            Cell::Text { v } => {
                let c = std::ffi::CString::new(v.as_str()).map_err(|err| err.to_string())?;
                unsafe {
                    ffi::sqlite3_bind_text(
                        stmt,
                        param.index,
                        c.as_ptr(),
                        -1,
                        ffi::SQLITE_TRANSIENT(),
                    )
                }
            }
        };
        if rc != ffi::SQLITE_OK {
            return Err(format!("bind {} failed ({rc})", param.index));
        }
    }
    Ok(())
}

fn read_row(stmt: *mut ffi::sqlite3_stmt, ncols: i32) -> Vec<Cell> {
    let mut row = Vec::with_capacity(ncols as usize);
    for i in 0..ncols {
        let kind = unsafe { ffi::sqlite3_column_type(stmt, i) };
        let cell = if kind == ffi::SQLITE_NULL {
            Cell::Null
        } else if kind == ffi::SQLITE_INTEGER {
            Cell::Int {
                v: unsafe { ffi::sqlite3_column_int64(stmt, i) },
            }
        } else if kind == ffi::SQLITE_FLOAT {
            Cell::Real {
                v: unsafe { ffi::sqlite3_column_double(stmt, i) },
            }
        } else {
            let ptr = unsafe { ffi::sqlite3_column_text(stmt, i) };
            if ptr.is_null() {
                Cell::Null
            } else {
                Cell::Text {
                    v: unsafe { std::ffi::CStr::from_ptr(ptr as *const std::os::raw::c_char) }
                        .to_string_lossy()
                        .into_owned(),
                }
            }
        };
        row.push(cell);
    }
    row
}

unsafe fn tail_has_sql(tail: *const std::os::raw::c_char) -> bool {
    if tail.is_null() {
        return false;
    }
    let text = unsafe { std::ffi::CStr::from_ptr(tail) }.to_string_lossy();
    !text.trim().is_empty()
}

fn sqlite_error(db: *mut ffi::sqlite3) -> (i32, String) {
    let code = unsafe { ffi::sqlite3_errcode(db) };
    let code = if code == 0 { 1 } else { code };
    let message = unsafe {
        let ptr = ffi::sqlite3_errmsg(db);
        if ptr.is_null() {
            "sqlite error".to_string()
        } else {
            std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    };
    (code, message)
}

fn ok_closed() -> Response {
    Response {
        ok: true,
        message: String::new(),
        code: 0,
        columns: Vec::new(),
        rows: Vec::new(),
        changes: 0,
        last_insert_rowid: 0,
        autocommit: true,
    }
}

fn error_response(autocommit: bool, code: i32, message: impl Into<String>) -> Response {
    Response {
        ok: false,
        message: message.into(),
        code,
        columns: Vec::new(),
        rows: Vec::new(),
        changes: 0,
        last_insert_rowid: 0,
        autocommit,
    }
}

fn accept_loop(listener: TcpListener, tx: Sender<Job>) {
    for conn in listener.incoming() {
        let Ok(conn) = conn else {
            continue;
        };
        let _ = conn.set_nodelay(true);
        let tx = tx.clone();
        thread::spawn(move || client_loop(conn, tx));
    }
}

fn client_loop(mut conn: TcpStream, tx: Sender<Job>) {
    let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let (open_tx, open_rx) = mpsc::channel();
    if tx.send(Job::Open { id, reply: open_tx }).is_err() {
        return;
    }
    match open_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            let _ = sqlite_broker_proto::write_msg(
                &mut conn,
                &sqlite_broker_proto::Hello {
                    ok: false,
                    session: id,
                    error: Some(err),
                },
            );
            return;
        }
        Err(_) => return,
    }
    if sqlite_broker_proto::write_msg(
        &mut conn,
        &sqlite_broker_proto::Hello {
            ok: true,
            session: id,
            error: None,
        },
    )
    .is_err()
    {
        drop_remote(&tx, id);
        return;
    }

    loop {
        let req = match sqlite_broker_proto::read_msg(&mut conn) {
            Ok(req) => req,
            Err(_) => {
                drop_remote(&tx, id);
                return;
            }
        };
        let is_close = matches!(req, Request::Close);
        let (reply_tx, reply_rx) = mpsc::channel();
        if tx
            .send(Job::Req {
                id,
                req,
                reply: reply_tx,
            })
            .is_err()
        {
            return;
        }
        let Ok(resp) = reply_rx.recv() else {
            return;
        };
        if sqlite_broker_proto::write_msg(&mut conn, &resp).is_err() {
            if !is_close {
                drop_remote(&tx, id);
            }
            return;
        }
        if is_close {
            return;
        }
    }
}

fn drop_remote(tx: &Sender<Job>, id: u64) {
    let (reply_tx, reply_rx) = mpsc::channel();
    if tx
        .send(Job::Drop {
            id,
            reply: reply_tx,
        })
        .is_err()
    {
        return;
    }
    let _ = reply_rx.recv();
}
