//! LD_PRELOAD replacement for libsqlite3. A regular file whose text is
//! `SQLITEBROKER1\\n<host:port>\\n` is not opened as a database. sqlite3_open connects
//! to the broker that owns the real file, and later SQL calls on that handle
//! run there. Any other path is forwarded to libsqlite3.so.0.

use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::fs;
use std::net::TcpStream;
use std::os::raw::{c_char, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::slice;
use std::sync::{LazyLock, Mutex, OnceLock};

use sqlite_broker_proto::{Cell, Hello, Param, Request, Response};

mod api;

include!(concat!(env!("OUT_DIR"), "/forward.rs"));

#[used]
#[link_section = ".init_array"]
static INIT_FORWARDERS: unsafe extern "C" fn() = init_real_forwarders;

const SQLITE_OK: i32 = 0;
const SQLITE_ERROR: i32 = 1;
const SQLITE_ABORT: i32 = 4;
const SQLITE_BUSY: i32 = 5;
const SQLITE_TOOBIG: i32 = 18;
const SQLITE_CANTOPEN: i32 = 14;
const SQLITE_MISUSE: i32 = 21;
const SQLITE_ROW: i32 = 100;
const SQLITE_DONE: i32 = 101;
const SQLITE_INTEGER: i32 = 1;
const SQLITE_FLOAT: i32 = 2;
const SQLITE_TEXT: i32 = 3;
const SQLITE_BLOB: i32 = 4;
const SQLITE_NULL: i32 = 5;

static DBS: LazyLock<Mutex<HashSet<usize>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
static STMTS: LazyLock<Mutex<HashSet<usize>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
static ALLOCATED: LazyLock<Mutex<HashSet<usize>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

struct ProxyDb {
    stream: Mutex<TcpStream>,
    filename: CString,
    errmsg: Mutex<CString>,
    changes: Mutex<i32>,
    total_changes: Mutex<i32>,
    last_rowid: Mutex<i64>,
    autocommit: Mutex<i32>,
    errcode: Mutex<i32>,
    stmts: Mutex<Vec<usize>>,
    closed: Mutex<bool>,
}

struct ProxyStmt {
    db: *const ProxyDb,
    sql: String,
    sql_c: CString,
    params: Vec<Param>,
    columns: Vec<CString>,
    decltypes: Vec<Option<CString>>,
    param_names: Vec<Option<CString>>,
    param_count: i32,
    rows: Vec<Vec<Cell>>,
    texts: Vec<CString>,
    blobs: Vec<Vec<u8>>,
    pos: usize,
    started: bool,
    failed: bool,
    last_step: i32,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

fn is_this_shim(handle: *mut c_void) -> bool {
    if handle.is_null() {
        return false;
    }
    let symbol = unsafe { libc::dlsym(handle, c"sqlite3_open".as_ptr()) };
    !symbol.is_null() && symbol == sqlite3_open as *mut c_void
}

fn try_open(path: *const c_char) -> *mut c_void {
    if path.is_null() {
        return ptr::null_mut();
    }
    let handle = unsafe { libc::dlopen(path, libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() || is_this_shim(handle) {
        if !handle.is_null() {
            unsafe { libc::dlclose(handle) };
        }
        return ptr::null_mut();
    }
    handle
}

fn open_real() -> *mut c_void {
    unsafe {
        let from_env = libc::getenv(c"SQLITE_BROKER_LIBSQLITE".as_ptr());
        let opened = try_open(from_env);
        if !opened.is_null() {
            return opened;
        }
        for path in [
            c"/usr/lib/x86_64-linux-gnu/libsqlite3.so.0".as_ptr(),
            c"/lib/x86_64-linux-gnu/libsqlite3.so.0".as_ptr(),
            c"/usr/lib/aarch64-linux-gnu/libsqlite3.so.0".as_ptr(),
            c"/lib/aarch64-linux-gnu/libsqlite3.so.0".as_ptr(),
            c"/usr/lib64/libsqlite3.so.0".as_ptr(),
            c"/lib64/libsqlite3.so.0".as_ptr(),
            c"/usr/lib/libsqlite3.so.0".as_ptr(),
            c"/lib/libsqlite3.so.0".as_ptr(),
        ] {
            let opened = try_open(path);
            if !opened.is_null() {
                return opened;
            }
        }
        try_open(c"libsqlite3.so.0".as_ptr())
    }
}

fn real_lib() -> *mut c_void {
    static LIB: OnceLock<usize> = OnceLock::new();
    let handle = *LIB.get_or_init(|| open_real() as usize);
    handle as *mut c_void
}

fn sym<T>(name: &[u8]) -> Option<T> {
    let lib = real_lib();
    if lib.is_null() {
        return None;
    }
    let ptr = unsafe { libc::dlsym(lib, name.as_ptr() as *const c_char) };
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute_copy(&ptr) })
    }
}

fn guard(f: impl FnOnce() -> i32) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(rc) => rc,
        Err(_) => SQLITE_ERROR,
    }
}

fn cstring_lossy(text: &str) -> CString {
    let mut bytes = text.as_bytes().to_vec();
    for byte in &mut bytes {
        if *byte == 0 {
            *byte = b' ';
        }
    }
    CString::new(bytes).unwrap_or_else(|_| CString::new("error").unwrap())
}

fn malloc_cstr(text: &str) -> *mut c_char {
    let bytes = text.as_bytes();
    let len = bytes.len() + 1;
    let ptr = unsafe { libc::malloc(len) } as *mut u8;
    if ptr.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        *ptr.add(bytes.len()) = 0;
    }
    lock(&*ALLOCATED).insert(ptr as usize);
    ptr as *mut c_char
}

unsafe fn sql_from(sql: *const c_char, nbytes: i32) -> String {
    if sql.is_null() {
        return String::new();
    }
    if nbytes < 0 {
        unsafe { CStr::from_ptr(sql) }
            .to_string_lossy()
            .into_owned()
    } else {
        let bytes = unsafe { slice::from_raw_parts(sql as *const u8, nbytes as usize) };
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    }
}

fn is_db(db: *mut c_void) -> bool {
    !db.is_null() && lock(&*DBS).contains(&(db as usize))
}

fn as_db<'a>(db: *mut c_void) -> Option<&'a ProxyDb> {
    if is_db(db) {
        Some(unsafe { &*(db as *const ProxyDb) })
    } else {
        None
    }
}

fn as_stmt_mut<'a>(stmt: *mut c_void) -> Option<&'a mut ProxyStmt> {
    if stmt.is_null() || !lock(&*STMTS).contains(&(stmt as usize)) {
        None
    } else {
        Some(unsafe { &mut *(stmt as *mut ProxyStmt) })
    }
}

fn as_stmt<'a>(stmt: *mut c_void) -> Option<&'a ProxyStmt> {
    if stmt.is_null() || !lock(&*STMTS).contains(&(stmt as usize)) {
        None
    } else {
        Some(unsafe { &*(stmt as *const ProxyStmt) })
    }
}

impl ProxyDb {
    fn connect(addr: &str, path: &str) -> Result<Self, String> {
        let mut stream =
            TcpStream::connect(addr).map_err(|err| format!("connect {addr}: {err}"))?;
        stream.set_nodelay(true).ok();
        let hello: Hello =
            sqlite_broker_proto::read_msg(&mut stream).map_err(|err| err.to_string())?;
        if !hello.ok {
            return Err(hello
                .error
                .unwrap_or_else(|| "broker rejected session".to_string()));
        }
        Ok(Self {
            stream: Mutex::new(stream),
            filename: cstring_lossy(path),
            errmsg: Mutex::new(CString::new("not an error").unwrap()),
            changes: Mutex::new(0),
            total_changes: Mutex::new(0),
            last_rowid: Mutex::new(0),
            autocommit: Mutex::new(1),
            errcode: Mutex::new(0),
            stmts: Mutex::new(Vec::new()),
            closed: Mutex::new(false),
        })
    }

    fn apply(&self, resp: &Response) {
        *lock(&self.changes) = resp.changes;
        *lock(&self.total_changes) = resp.total_changes;
        *lock(&self.last_rowid) = resp.last_insert_rowid;
        *lock(&self.autocommit) = if resp.autocommit { 1 } else { 0 };
        if resp.ok {
            *lock(&self.errcode) = 0;
            *lock(&self.errmsg) = CString::new("not an error").unwrap();
        } else {
            let code = if resp.code == 0 {
                SQLITE_ERROR
            } else {
                resp.code
            };
            *lock(&self.errcode) = code;
            *lock(&self.errmsg) = cstring_lossy(&resp.message);
        }
    }

    fn fail(&self, message: &str) {
        *lock(&self.errcode) = SQLITE_ERROR;
        *lock(&self.errmsg) = cstring_lossy(message);
    }

    fn rpc(&self, req: &Request) -> Result<Response, String> {
        if *lock(&self.closed) {
            self.fail("connection is closed");
            return Err("connection is closed".to_string());
        }
        let rpc = {
            let mut stream = lock(&self.stream);
            match sqlite_broker_proto::write_msg(&mut *stream, req) {
                Ok(()) => {
                    let parsed: std::io::Result<Response> =
                        sqlite_broker_proto::read_msg(&mut *stream);
                    parsed.map_err(|err| err.to_string())
                }
                Err(err) => Err(err.to_string()),
            }
        };
        let resp = match rpc {
            Ok(resp) => resp,
            Err(err) => {
                self.fail(&err);
                return Err(err);
            }
        };
        self.apply(&resp);
        if resp.ok {
            Ok(resp)
        } else {
            Err(if resp.message.is_empty() {
                "sql error".to_string()
            } else {
                resp.message
            })
        }
    }

    fn exec_sql(&self, sql: &str, params: Vec<Param>) -> Result<Response, String> {
        self.rpc(&Request::Exec {
            sql: sql.to_string(),
            params,
        })
    }

    fn prepare_meta(&self, sql: &str) -> Result<Response, String> {
        self.rpc(&Request::Prepare {
            sql: sql.to_string(),
        })
    }

    fn shutdown(&self) {
        let mut stream = lock(&self.stream);
        let _ = sqlite_broker_proto::write_msg(&mut *stream, &Request::Close);
        let _: std::io::Result<Response> = sqlite_broker_proto::read_msg(&mut *stream);
    }

    fn link_stmt(&self, stmt: usize) {
        lock(&self.stmts).push(stmt);
    }

    fn unlink_stmt(&self, stmt: usize) {
        lock(&self.stmts).retain(|item| *item != stmt);
    }
}

fn stub_addr(path: &str) -> Option<String> {
    if path.is_empty() || path == ":memory:" {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    sqlite_broker_proto::parse_stub(&bytes)
}

type OpenV2Fn = unsafe extern "C" fn(*const c_char, *mut *mut c_void, i32, *const c_char) -> i32;
type CloseFn = unsafe extern "C" fn(*mut c_void) -> i32;
type ExecFn = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    *mut c_void,
    *mut c_void,
    *mut *mut c_char,
) -> i32;
type PrepareFn = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    i32,
    *mut *mut c_void,
    *mut *const c_char,
) -> i32;
type StepFn = unsafe extern "C" fn(*mut c_void) -> i32;
type ColCountFn = unsafe extern "C" fn(*mut c_void) -> i32;
type ColTextFn = unsafe extern "C" fn(*mut c_void, i32) -> *const u8;
type ChangesFn = unsafe extern "C" fn(*mut c_void) -> i32;
type RowidFn = unsafe extern "C" fn(*mut c_void) -> i64;
type ErrmsgFn = unsafe extern "C" fn(*mut c_void) -> *const c_char;
type ErrcodeFn = unsafe extern "C" fn(*mut c_void) -> i32;
type FreeFn = unsafe extern "C" fn(*mut c_void);
type BindTextFn = unsafe extern "C" fn(*mut c_void, i32, *const c_char, i32, *const c_void) -> i32;
type BindInt64Fn = unsafe extern "C" fn(*mut c_void, i32, i64) -> i32;
type BindNullFn = unsafe extern "C" fn(*mut c_void, i32) -> i32;
type AutoFn = unsafe extern "C" fn(*mut c_void) -> i32;

#[no_mangle]
pub unsafe extern "C" fn sqlite3_open(filename: *const c_char, ppdb: *mut *mut c_void) -> i32 {
    guard(|| unsafe { sqlite3_open_v2(filename, ppdb, 0x2 | 0x4, ptr::null()) })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_open_v2(
    filename: *const c_char,
    ppdb: *mut *mut c_void,
    flags: i32,
    vfs: *const c_char,
) -> i32 {
    guard(|| unsafe { open_v2(filename, ppdb, flags, vfs) })
}

unsafe fn open_v2(
    filename: *const c_char,
    ppdb: *mut *mut c_void,
    flags: i32,
    vfs: *const c_char,
) -> i32 {
    if ppdb.is_null() {
        return SQLITE_MISUSE;
    }
    unsafe { *ppdb = ptr::null_mut() };
    if filename.is_null() {
        return SQLITE_MISUSE;
    }
    let path = unsafe { CStr::from_ptr(filename) }.to_string_lossy();
    if let Some(addr) = stub_addr(&path) {
        match ProxyDb::connect(&addr, &path) {
            Ok(proxy) => {
                let raw = Box::into_raw(Box::new(proxy));
                lock(&*DBS).insert(raw as usize);
                unsafe { *ppdb = raw as *mut c_void };
                SQLITE_OK
            }
            Err(err) => {
                eprintln!("sqlite-broker: open failed: {err}");
                SQLITE_CANTOPEN
            }
        }
    } else if let Some(func) = sym::<OpenV2Fn>(b"sqlite3_open_v2\0") {
        unsafe { func(filename, ppdb, flags, vfs) }
    } else {
        SQLITE_ERROR
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_close(db: *mut c_void) -> i32 {
    guard(|| close_db(db, false))
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_close_v2(db: *mut c_void) -> i32 {
    guard(|| close_db(db, true))
}

fn close_db(db: *mut c_void, v2: bool) -> i32 {
    if db.is_null() {
        return SQLITE_OK;
    }
    if !is_db(db) {
        let symbol = if v2 {
            b"sqlite3_close_v2\0".as_slice()
        } else {
            b"sqlite3_close\0".as_slice()
        };
        return if let Some(func) = sym::<CloseFn>(symbol) {
            unsafe { func(db) }
        } else {
            SQLITE_ERROR
        };
    }
    let proxy = unsafe { &*(db as *const ProxyDb) };
    if !v2 && !lock(&proxy.stmts).is_empty() {
        return SQLITE_BUSY;
    }
    *lock(&proxy.closed) = true;
    if lock(&proxy.stmts).is_empty() {
        destroy_db(db);
    }
    SQLITE_OK
}

fn destroy_db(db: *mut c_void) {
    if !lock(&*DBS).remove(&(db as usize)) {
        return;
    }
    let proxy = unsafe { &*(db as *const ProxyDb) };
    proxy.shutdown();
    unsafe { drop(Box::from_raw(db as *mut ProxyDb)) };
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_exec(
    db: *mut c_void,
    sql: *const c_char,
    callback: *mut c_void,
    arg: *mut c_void,
    errmsg_out: *mut *mut c_char,
) -> i32 {
    guard(|| exec_sql(db, sql, callback, arg, errmsg_out))
}

fn exec_sql(
    db: *mut c_void,
    sql: *const c_char,
    callback: *mut c_void,
    arg: *mut c_void,
    errmsg_out: *mut *mut c_char,
) -> i32 {
    let Some(proxy) = as_db(db) else {
        return if let Some(func) = sym::<ExecFn>(b"sqlite3_exec\0") {
            unsafe { func(db, sql, callback, arg, errmsg_out) }
        } else {
            SQLITE_ERROR
        };
    };
    if sql.is_null() {
        return SQLITE_MISUSE;
    }
    let text = unsafe { sql_from(sql, -1) };
    match proxy.exec_sql(&text, Vec::new()) {
        Ok(resp) => {
            if !errmsg_out.is_null() {
                unsafe { *errmsg_out = ptr::null_mut() };
            }
            if !callback.is_null() {
                let callback: unsafe extern "C" fn(
                    *mut c_void,
                    i32,
                    *mut *mut c_char,
                    *mut *mut c_char,
                ) -> i32 = unsafe { std::mem::transmute(callback) };
                for row in &resp.rows {
                    let values: Vec<CString> = row
                        .iter()
                        .map(|cell| cstring_lossy(&cell.render()))
                        .collect();
                    let mut names: Vec<CString> = resp
                        .columns
                        .iter()
                        .map(|name| cstring_lossy(name))
                        .collect();
                    if names.len() < values.len() {
                        names.resize_with(values.len(), || CString::new("").unwrap());
                    }
                    let mut value_ptrs: Vec<*mut c_char> = values
                        .iter()
                        .map(|item| item.as_ptr() as *mut c_char)
                        .collect();
                    let mut name_ptrs: Vec<*mut c_char> = names
                        .iter()
                        .map(|item| item.as_ptr() as *mut c_char)
                        .collect();
                    let rc = unsafe {
                        callback(
                            arg,
                            values.len() as i32,
                            value_ptrs.as_mut_ptr(),
                            name_ptrs.as_mut_ptr(),
                        )
                    };
                    if rc != 0 {
                        proxy.apply(&Response {
                            ok: false,
                            message: "callback aborted".to_string(),
                            code: SQLITE_ABORT,
                            columns: Vec::new(),
                            rows: Vec::new(),
                            changes: resp.changes,
                            last_insert_rowid: resp.last_insert_rowid,
                            autocommit: resp.autocommit,
                            tail: 0,
                            param_count: 0,
                            param_names: Vec::new(),
                            decltypes: Vec::new(),
                            total_changes: resp.total_changes,
                            empty: false,
                        });
                        if !errmsg_out.is_null() {
                            unsafe { *errmsg_out = malloc_cstr("callback aborted") };
                        }
                        return SQLITE_ABORT;
                    }
                }
            }
            SQLITE_OK
        }
        Err(message) => {
            if !errmsg_out.is_null() {
                unsafe { *errmsg_out = malloc_cstr(&message) };
            }
            let code = *lock(&proxy.errcode);
            if code == 0 {
                SQLITE_ERROR
            } else {
                code
            }
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_prepare_v2(
    db: *mut c_void,
    sql: *const c_char,
    nbytes: i32,
    ppstmt: *mut *mut c_void,
    pztail: *mut *const c_char,
) -> i32 {
    guard(|| prepare_v2(db, sql, nbytes, ppstmt, pztail))
}

fn prepare_v2(
    db: *mut c_void,
    sql: *const c_char,
    nbytes: i32,
    ppstmt: *mut *mut c_void,
    pztail: *mut *const c_char,
) -> i32 {
    if ppstmt.is_null() {
        return SQLITE_MISUSE;
    }
    unsafe { *ppstmt = ptr::null_mut() };
    if !is_db(db) {
        return if let Some(func) = sym::<PrepareFn>(b"sqlite3_prepare_v2\0") {
            unsafe { func(db, sql, nbytes, ppstmt, pztail) }
        } else {
            SQLITE_ERROR
        };
    }
    if sql.is_null() {
        return SQLITE_MISUSE;
    }
    if !pztail.is_null() && nbytes < 0 {
        let len = unsafe { CStr::from_ptr(sql) }.to_bytes().len();
        unsafe { *pztail = sql.add(len) };
    }
    let text = unsafe { sql_from(sql, nbytes) };
    let proxy = unsafe { &*(db as *const ProxyDb) };
    let resp = match proxy.prepare_meta(&text) {
        Ok(resp) => resp,
        Err(_) => return *lock(&proxy.errcode),
    };
    let tail = resp.tail.min(text.len());
    if !pztail.is_null() {
        unsafe { *pztail = sql.add(tail) };
    }
    if resp.empty {
        return SQLITE_OK;
    }
    let stmt_sql = if tail == 0 {
        text
    } else {
        text[..tail].to_string()
    };
    let stmt = Box::new(ProxyStmt {
        db: db as *const ProxyDb,
        sql_c: cstring_lossy(&stmt_sql),
        sql: stmt_sql,
        params: Vec::new(),
        columns: resp
            .columns
            .iter()
            .map(|name| cstring_lossy(name))
            .collect(),
        decltypes: resp
            .decltypes
            .iter()
            .map(|name| {
                if name.is_empty() {
                    None
                } else {
                    Some(cstring_lossy(name))
                }
            })
            .collect(),
        param_names: resp
            .param_names
            .iter()
            .map(|name| {
                if name.is_empty() {
                    None
                } else {
                    Some(cstring_lossy(name))
                }
            })
            .collect(),
        param_count: resp.param_count,
        rows: Vec::new(),
        texts: Vec::new(),
        blobs: Vec::new(),
        pos: 0,
        started: false,
        failed: false,
        last_step: 0,
    });
    let raw = Box::into_raw(stmt);
    lock(&*STMTS).insert(raw as usize);
    proxy.link_stmt(raw as usize);
    unsafe { *ppstmt = raw as *mut c_void };
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_step(stmt: *mut c_void) -> i32 {
    guard(|| step_stmt(stmt))
}

fn step_stmt(stmt_ptr: *mut c_void) -> i32 {
    let Some(stmt) = as_stmt_mut(stmt_ptr) else {
        return if let Some(func) = sym::<StepFn>(b"sqlite3_step\0") {
            unsafe { func(stmt_ptr) }
        } else {
            SQLITE_ERROR
        };
    };
    if !stmt.started {
        let sql = stmt.sql.clone();
        let params = stmt.params.clone();
        let db = stmt.db;
        match unsafe { &*db }.exec_sql(&sql, params) {
            Ok(resp) => {
                if !resp.columns.is_empty() {
                    stmt.columns = resp
                        .columns
                        .iter()
                        .map(|name| cstring_lossy(name))
                        .collect();
                }
                stmt.rows = resp.rows;
                stmt.pos = 0;
                stmt.failed = false;
                stmt.started = true;
            }
            Err(_) => {
                stmt.failed = true;
                stmt.started = true;
                let code = *lock(&unsafe { &*db }.errcode);
                let code = if code == 0 { SQLITE_ERROR } else { code };
                stmt.last_step = code;
                return code;
            }
        }
    }
    if stmt.failed {
        return if stmt.last_step == 0 {
            SQLITE_ERROR
        } else {
            stmt.last_step
        };
    }
    if stmt.pos < stmt.rows.len() {
        stmt.pos += 1;
        stmt.cache_row();
        stmt.last_step = SQLITE_ROW;
        SQLITE_ROW
    } else {
        stmt.last_step = SQLITE_DONE;
        SQLITE_DONE
    }
}

impl ProxyStmt {
    fn cache_row(&mut self) {
        self.texts.clear();
        self.blobs.clear();
        let Some(row) = self.rows.get(self.pos - 1) else {
            return;
        };
        for cell in row {
            match cell {
                Cell::Blob { .. } => {
                    let bytes = cell.blob_bytes().unwrap_or_default();
                    self.texts.push(cstring_lossy(&String::from_utf8_lossy(&bytes)));
                    self.blobs.push(bytes);
                }
                other => {
                    self.texts.push(cstring_lossy(&other.render()));
                    self.blobs.push(Vec::new());
                }
            }
        }
    }

    fn cell_at(&self, index: i32) -> Option<&Cell> {
        if self.last_step != SQLITE_ROW || self.pos == 0 || index < 0 {
            return None;
        }
        self.rows
            .get(self.pos - 1)
            .and_then(|row| row.get(index as usize))
    }

    fn text_ptr(&self, index: i32) -> *const u8 {
        if index < 0 {
            return ptr::null();
        }
        self.texts
            .get(index as usize)
            .map(|text| text.as_ptr() as *const u8)
            .unwrap_or(ptr::null())
    }

    fn param_index(&self, query: &str) -> i32 {
        let matches = |name: &CString, wanted: &str| name.to_string_lossy() == wanted;
        for (i, name) in self.param_names.iter().enumerate() {
            if let Some(name) = name {
                if matches(name, query) {
                    return (i + 1) as i32;
                }
            }
        }
        if query.starts_with([':', '@', '$', '?']) {
            return 0;
        }
        for prefix in [":", "@", "$"] {
            let alt = format!("{prefix}{query}");
            for (i, name) in self.param_names.iter().enumerate() {
                if let Some(name) = name {
                    if matches(name, &alt) {
                        return (i + 1) as i32;
                    }
                }
            }
        }
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_finalize(stmt: *mut c_void) -> i32 {
    guard(|| finalize_stmt(stmt))
}

fn finalize_stmt(stmt: *mut c_void) -> i32 {
    if stmt.is_null() {
        return SQLITE_OK;
    }
    if !lock(&*STMTS).remove(&(stmt as usize)) {
        return if let Some(func) = sym::<StepFn>(b"sqlite3_finalize\0") {
            unsafe { func(stmt) }
        } else {
            SQLITE_ERROR
        };
    }
    let db = unsafe { (*(stmt as *const ProxyStmt)).db };
    let proxy = unsafe { &*db };
    proxy.unlink_stmt(stmt as usize);
    unsafe { drop(Box::from_raw(stmt as *mut ProxyStmt)) };
    if *lock(&proxy.closed) && lock(&proxy.stmts).is_empty() {
        destroy_db(db as *mut c_void);
    }
    SQLITE_OK
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_reset(stmt: *mut c_void) -> i32 {
    guard(|| {
        let Some(stmt) = as_stmt_mut(stmt) else {
            return if let Some(func) = sym::<StepFn>(b"sqlite3_reset\0") {
                unsafe { func(stmt) }
            } else {
                SQLITE_ERROR
            };
        };
        stmt.started = false;
        stmt.failed = false;
        stmt.rows.clear();
        stmt.texts.clear();
        stmt.blobs.clear();
        stmt.pos = 0;
        stmt.last_step = 0;
        SQLITE_OK
    })
}

fn upsert_param(stmt: &mut ProxyStmt, index: i32, value: Cell) {
    if let Some(existing) = stmt.params.iter_mut().find(|param| param.index == index) {
        existing.value = value;
    } else {
        stmt.params.push(Param { index, value });
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_text(
    stmt: *mut c_void,
    index: i32,
    value: *const c_char,
    n: i32,
    _destructor: *const c_void,
) -> i32 {
    guard(|| {
        let Some(stmt) = as_stmt_mut(stmt) else {
            return if let Some(func) = sym::<BindTextFn>(b"sqlite3_bind_text\0") {
                unsafe { func(stmt, index, value, n, _destructor) }
            } else {
                SQLITE_ERROR
            };
        };
        if stmt.started || index <= 0 {
            return SQLITE_MISUSE;
        }
        if value.is_null() {
            upsert_param(stmt, index, Cell::Null);
        } else {
            let text = unsafe { sql_from(value, n) };
            upsert_param(stmt, index, Cell::Text { v: text });
        }
        release_destructor(_destructor, value as *mut c_void);
        SQLITE_OK
    })
}

fn release_destructor(dtor: *const c_void, ptr: *mut c_void) {
    if dtor.is_null() || dtor as isize == -1 || ptr.is_null() {
        return;
    }
    let func: unsafe extern "C" fn(*mut c_void) = unsafe { std::mem::transmute(dtor) };
    unsafe { func(ptr) };
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_double(stmt: *mut c_void, index: i32, value: f64) -> i32 {
    guard(|| {
        let Some(stmt) = as_stmt_mut(stmt) else {
            return if let Some(func) = sym::<unsafe extern "C" fn(*mut c_void, i32, f64) -> i32>(
                b"sqlite3_bind_double\0",
            ) {
                unsafe { func(stmt, index, value) }
            } else {
                SQLITE_ERROR
            };
        };
        if stmt.started || index <= 0 {
            return SQLITE_MISUSE;
        }
        upsert_param(stmt, index, Cell::Real { v: value });
        SQLITE_OK
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_blob(
    stmt: *mut c_void,
    index: i32,
    value: *const c_void,
    n: i32,
    dtor: *const c_void,
) -> i32 {
    guard(|| {
        let Some(stmt) = as_stmt_mut(stmt) else {
            return if let Some(func) = sym::<
                unsafe extern "C" fn(*mut c_void, i32, *const c_void, i32, *const c_void) -> i32,
            >(b"sqlite3_bind_blob\0")
            {
                unsafe { func(stmt, index, value, n, dtor) }
            } else {
                SQLITE_ERROR
            };
        };
        if stmt.started || index <= 0 {
            return SQLITE_MISUSE;
        }
        if value.is_null() && n > 0 {
            return SQLITE_MISUSE;
        }
        if n < 0 {
            release_destructor(dtor, value as *mut c_void);
            return SQLITE_TOOBIG;
        }
        let bytes = if n == 0 || value.is_null() {
            Vec::new()
        } else {
            unsafe { slice::from_raw_parts(value as *const u8, n as usize).to_vec() }
        };
        upsert_param(
            stmt,
            index,
            Cell::Blob {
                v: sqlite_broker_proto::encode_base64(&bytes),
            },
        );
        release_destructor(dtor, value as *mut c_void);
        SQLITE_OK
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_int64(stmt: *mut c_void, index: i32, value: i64) -> i32 {
    guard(|| {
        let Some(stmt) = as_stmt_mut(stmt) else {
            return if let Some(func) = sym::<BindInt64Fn>(b"sqlite3_bind_int64\0") {
                unsafe { func(stmt, index, value) }
            } else {
                SQLITE_ERROR
            };
        };
        if stmt.started || index <= 0 {
            return SQLITE_MISUSE;
        }
        upsert_param(stmt, index, Cell::Int { v: value });
        SQLITE_OK
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_int(stmt: *mut c_void, index: i32, value: i32) -> i32 {
    unsafe { sqlite3_bind_int64(stmt, index, i64::from(value)) }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_null(stmt: *mut c_void, index: i32) -> i32 {
    guard(|| {
        let Some(stmt) = as_stmt_mut(stmt) else {
            return if let Some(func) = sym::<BindNullFn>(b"sqlite3_bind_null\0") {
                unsafe { func(stmt, index) }
            } else {
                SQLITE_ERROR
            };
        };
        if stmt.started || index <= 0 {
            return SQLITE_MISUSE;
        }
        upsert_param(stmt, index, Cell::Null);
        SQLITE_OK
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_count(stmt: *mut c_void) -> i32 {
    guard(|| {
        if let Some(stmt) = as_stmt(stmt) {
            stmt.columns.len() as i32
        } else if let Some(func) = sym::<ColCountFn>(b"sqlite3_column_count\0") {
            unsafe { func(stmt) }
        } else {
            0
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_text(stmt: *mut c_void, i: i32) -> *const u8 {
    match catch_unwind(AssertUnwindSafe(|| {
        if let Some(stmt) = as_stmt(stmt) {
            if matches!(stmt.cell_at(i), Some(Cell::Null) | None) {
                return ptr::null();
            }
            stmt.text_ptr(i)
        } else if let Some(func) = sym::<ColTextFn>(b"sqlite3_column_text\0") {
            unsafe { func(stmt, i) }
        } else {
            ptr::null()
        }
    })) {
        Ok(ptr) => ptr,
        Err(_) => ptr::null(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_name(stmt: *mut c_void, i: i32) -> *const c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        if let Some(stmt) = as_stmt(stmt) {
            if i < 0 {
                return ptr::null();
            }
            stmt.columns
                .get(i as usize)
                .map(|name| name.as_ptr())
                .unwrap_or(ptr::null())
        } else if let Some(func) =
            sym::<unsafe extern "C" fn(*mut c_void, i32) -> *const c_char>(b"sqlite3_column_name\0")
        {
            unsafe { func(stmt, i) }
        } else {
            ptr::null()
        }
    })) {
        Ok(ptr) => ptr,
        Err(_) => ptr::null(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_changes(db: *mut c_void) -> i32 {
    guard(|| {
        if let Some(proxy) = as_db(db) {
            *lock(&proxy.changes)
        } else if let Some(func) = sym::<ChangesFn>(b"sqlite3_changes\0") {
            unsafe { func(db) }
        } else {
            0
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_last_insert_rowid(db: *mut c_void) -> i64 {
    match catch_unwind(AssertUnwindSafe(|| {
        if let Some(proxy) = as_db(db) {
            *lock(&proxy.last_rowid)
        } else if let Some(func) = sym::<RowidFn>(b"sqlite3_last_insert_rowid\0") {
            unsafe { func(db) }
        } else {
            0
        }
    })) {
        Ok(value) => value,
        Err(_) => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_errmsg(db: *mut c_void) -> *const c_char {
    match catch_unwind(AssertUnwindSafe(|| {
        if let Some(proxy) = as_db(db) {
            lock(&proxy.errmsg).as_ptr()
        } else if let Some(func) = sym::<ErrmsgFn>(b"sqlite3_errmsg\0") {
            unsafe { func(db) }
        } else {
            c"sqlite error".as_ptr()
        }
    })) {
        Ok(ptr) => ptr,
        Err(_) => c"sqlite error".as_ptr(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_errcode(db: *mut c_void) -> i32 {
    guard(|| {
        if let Some(proxy) = as_db(db) {
            *lock(&proxy.errcode)
        } else if let Some(func) = sym::<ErrcodeFn>(b"sqlite3_errcode\0") {
            unsafe { func(db) }
        } else {
            SQLITE_ERROR
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_get_autocommit(db: *mut c_void) -> i32 {
    guard(|| {
        if let Some(proxy) = as_db(db) {
            *lock(&proxy.autocommit)
        } else if let Some(func) = sym::<AutoFn>(b"sqlite3_get_autocommit\0") {
            unsafe { func(db) }
        } else {
            1
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_free(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    if lock(&*ALLOCATED).remove(&(ptr as usize)) {
        unsafe { libc::free(ptr) };
        return;
    }
    if let Some(func) = sym::<FreeFn>(b"sqlite3_free\0") {
        unsafe { func(ptr) };
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn stub_parser_sees_magic() {
        assert!(sqlite_broker_proto::parse_stub(b"SQLITEBROKER1\n127.0.0.1:1\n").is_some());
    }
}
