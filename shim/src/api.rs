//! SQLite C API used by CPython's sqlite3 module and PHP's pdo_sqlite.
//! Symbols that take a database or statement pointer have to live here: the
//! real libsqlite3 would treat a proxy pointer as its own struct.

use std::ffi::CStr;
use std::os::raw::{c_char, c_void};
use std::ptr;

use super::{
    as_db, as_stmt, as_stmt_mut, guard, lock, malloc_cstr, sym, SQLITE_BLOB, SQLITE_ERROR,
    SQLITE_FLOAT, SQLITE_INTEGER, SQLITE_MISUSE, SQLITE_NULL, SQLITE_OK, SQLITE_ROW, SQLITE_TEXT,
    SQLITE_TOOBIG,
};
use sqlite_broker_proto::Cell;

fn keyword(sql: &str) -> Option<String> {
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' | 0x0c => i += 1,
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            _ => {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                if start == i {
                    return None;
                }
                return Some(sql[start..i].to_ascii_uppercase());
            }
        }
    }
    None
}

fn limit_value(id: i32) -> i32 {
    match id {
        0 | 1 => 1_000_000_000,
        2 => 2000,
        3 | 10 => 1000,
        4 => 500,
        5 => 25_000,
        6 => 100,
        7 => 10,
        8 => 50_000,
        9 => 32_766,
        _ => 1_000_000,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_parameter_count(stmt: *mut c_void) -> i32 {
    guard(|| {
        if let Some(stmt) = as_stmt(stmt) {
            stmt.param_count
        } else if let Some(func) = sym::<unsafe extern "C" fn(*mut c_void) -> i32>(
            b"sqlite3_bind_parameter_count\0",
        ) {
            unsafe { func(stmt) }
        } else {
            0
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_parameter_name(stmt: *mut c_void, index: i32) -> *const c_char {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(stmt) = as_stmt(stmt) {
            if index <= 0 {
                return ptr::null();
            }
            stmt.param_names
                .get((index - 1) as usize)
                .and_then(|name| name.as_ref())
                .map(|name| name.as_ptr())
                .unwrap_or(ptr::null())
        } else if let Some(func) =
            sym::<unsafe extern "C" fn(*mut c_void, i32) -> *const c_char>(
                b"sqlite3_bind_parameter_name\0",
            )
        {
            unsafe { func(stmt, index) }
        } else {
            ptr::null()
        }
    })) {
        Ok(ptr) => ptr,
        Err(_) => ptr::null(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_parameter_index(
    stmt: *mut c_void,
    name: *const c_char,
) -> i32 {
    guard(|| {
        let Some(stmt) = as_stmt(stmt) else {
            return if let Some(func) =
                sym::<unsafe extern "C" fn(*mut c_void, *const c_char) -> i32>(
                    b"sqlite3_bind_parameter_index\0",
                ) {
                unsafe { func(stmt, name) }
            } else {
                0
            };
        };
        if name.is_null() {
            return 0;
        }
        let query = unsafe { CStr::from_ptr(name) }.to_string_lossy();
        stmt.param_index(&query)
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_clear_bindings(stmt: *mut c_void) -> i32 {
    guard(|| {
        let Some(stmt) = as_stmt_mut(stmt) else {
            return if let Some(func) =
                sym::<unsafe extern "C" fn(*mut c_void) -> i32>(b"sqlite3_clear_bindings\0")
            {
                unsafe { func(stmt) }
            } else {
                SQLITE_ERROR
            };
        };
        stmt.params.clear();
        SQLITE_OK
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_bind_zeroblob(stmt: *mut c_void, index: i32, n: i32) -> i32 {
    guard(|| {
        let Some(stmt) = as_stmt_mut(stmt) else {
            return if let Some(func) =
                sym::<unsafe extern "C" fn(*mut c_void, i32, i32) -> i32>(b"sqlite3_bind_zeroblob\0")
            {
                unsafe { func(stmt, index, n) }
            } else {
                SQLITE_ERROR
            };
        };
        if stmt.started || index <= 0 {
            return SQLITE_MISUSE;
        }
        if n < 0 || n > 8 * 1024 * 1024 {
            return SQLITE_TOOBIG;
        }
        super::upsert_param(
            stmt,
            index,
            Cell::Blob {
                v: sqlite_broker_proto::encode_base64(&vec![0u8; n as usize]),
            },
        );
        SQLITE_OK
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_type(stmt: *mut c_void, i: i32) -> i32 {
    guard(|| {
        if let Some(stmt) = as_stmt(stmt) {
            match stmt.cell_at(i) {
                Some(Cell::Null) | None => SQLITE_NULL,
                Some(Cell::Int { .. }) => SQLITE_INTEGER,
                Some(Cell::Real { .. }) => SQLITE_FLOAT,
                Some(Cell::Text { .. }) => SQLITE_TEXT,
                Some(Cell::Blob { .. }) => SQLITE_BLOB,
            }
        } else if let Some(func) =
            sym::<unsafe extern "C" fn(*mut c_void, i32) -> i32>(b"sqlite3_column_type\0")
        {
            unsafe { func(stmt, i) }
        } else {
            SQLITE_NULL
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_int64(stmt: *mut c_void, i: i32) -> i64 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(stmt) = as_stmt(stmt) {
            match stmt.cell_at(i) {
                Some(Cell::Int { v }) => *v,
                Some(Cell::Real { v }) => *v as i64,
                Some(Cell::Text { v }) => v.parse().unwrap_or(0),
                _ => 0,
            }
        } else if let Some(func) =
            sym::<unsafe extern "C" fn(*mut c_void, i32) -> i64>(b"sqlite3_column_int64\0")
        {
            unsafe { func(stmt, i) }
        } else {
            0
        }
    })) {
        Ok(value) => value,
        Err(_) => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_int(stmt: *mut c_void, i: i32) -> i32 {
    unsafe { sqlite3_column_int64(stmt, i) as i32 }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_double(stmt: *mut c_void, i: i32) -> f64 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(stmt) = as_stmt(stmt) {
            match stmt.cell_at(i) {
                Some(Cell::Real { v }) => *v,
                Some(Cell::Int { v }) => *v as f64,
                Some(Cell::Text { v }) => v.parse().unwrap_or(0.0),
                _ => 0.0,
            }
        } else if let Some(func) =
            sym::<unsafe extern "C" fn(*mut c_void, i32) -> f64>(b"sqlite3_column_double\0")
        {
            unsafe { func(stmt, i) }
        } else {
            0.0
        }
    })) {
        Ok(value) => value,
        Err(_) => 0.0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_bytes(stmt: *mut c_void, i: i32) -> i32 {
    guard(|| {
        if let Some(stmt) = as_stmt(stmt) {
            match stmt.cell_at(i) {
                Some(Cell::Blob { .. }) => stmt
                    .blobs
                    .get(i as usize)
                    .map(|bytes| bytes.len() as i32)
                    .unwrap_or(0),
                Some(Cell::Null) | None => 0,
                Some(_) => stmt
                    .texts
                    .get(i as usize)
                    .map(|text| text.as_bytes().len() as i32)
                    .unwrap_or(0),
            }
        } else if let Some(func) =
            sym::<unsafe extern "C" fn(*mut c_void, i32) -> i32>(b"sqlite3_column_bytes\0")
        {
            unsafe { func(stmt, i) }
        } else {
            0
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_blob(stmt: *mut c_void, i: i32) -> *const c_void {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(stmt) = as_stmt(stmt) {
            match stmt.cell_at(i) {
                Some(Cell::Blob { .. }) => stmt
                    .blobs
                    .get(i as usize)
                    .filter(|bytes| !bytes.is_empty())
                    .map(|bytes| bytes.as_ptr() as *const c_void)
                    .unwrap_or(ptr::null()),
                Some(Cell::Null) | None => ptr::null(),
                Some(_) => stmt.text_ptr(i) as *const c_void,
            }
        } else if let Some(func) = sym::<unsafe extern "C" fn(*mut c_void, i32) -> *const c_void>(
            b"sqlite3_column_blob\0",
        ) {
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
pub unsafe extern "C" fn sqlite3_column_decltype(stmt: *mut c_void, i: i32) -> *const c_char {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(stmt) = as_stmt(stmt) {
            stmt.decltypes
                .get(i as usize)
                .and_then(|name| name.as_ref())
                .map(|name| name.as_ptr())
                .unwrap_or(ptr::null())
        } else if let Some(func) =
            sym::<unsafe extern "C" fn(*mut c_void, i32) -> *const c_char>(
                b"sqlite3_column_decltype\0",
            )
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
pub unsafe extern "C" fn sqlite3_column_table_name(stmt: *mut c_void, i: i32) -> *const c_char {
    column_name_forward(stmt, i, b"sqlite3_column_table_name\0")
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_database_name(stmt: *mut c_void, i: i32) -> *const c_char {
    column_name_forward(stmt, i, b"sqlite3_column_database_name\0")
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_column_origin_name(stmt: *mut c_void, i: i32) -> *const c_char {
    column_name_forward(stmt, i, b"sqlite3_column_origin_name\0")
}

fn column_name_forward(stmt: *mut c_void, i: i32, symbol: &[u8]) -> *const c_char {
    if as_stmt(stmt).is_some() {
        return ptr::null();
    }
    if let Some(func) =
        sym::<unsafe extern "C" fn(*mut c_void, i32) -> *const c_char>(symbol)
    {
        unsafe { func(stmt, i) }
    } else {
        ptr::null()
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_data_count(stmt: *mut c_void) -> i32 {
    guard(|| {
        if let Some(stmt) = as_stmt(stmt) {
            if stmt.last_step == SQLITE_ROW {
                stmt.columns.len() as i32
            } else {
                0
            }
        } else if let Some(func) =
            sym::<unsafe extern "C" fn(*mut c_void) -> i32>(b"sqlite3_data_count\0")
        {
            unsafe { func(stmt) }
        } else {
            0
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_stmt_busy(stmt: *mut c_void) -> i32 {
    guard(|| {
        if let Some(stmt) = as_stmt(stmt) {
            i32::from(stmt.last_step == SQLITE_ROW)
        } else if let Some(func) =
            sym::<unsafe extern "C" fn(*mut c_void) -> i32>(b"sqlite3_stmt_busy\0")
        {
            unsafe { func(stmt) }
        } else {
            0
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_stmt_readonly(stmt: *mut c_void) -> i32 {
    guard(|| {
        if let Some(stmt) = as_stmt(stmt) {
            let word = keyword(&stmt.sql);
            i32::from(matches!(
                word.as_deref(),
                Some("SELECT" | "EXPLAIN" | "PRAGMA" | "VALUES")
            ))
        } else if let Some(func) =
            sym::<unsafe extern "C" fn(*mut c_void) -> i32>(b"sqlite3_stmt_readonly\0")
        {
            unsafe { func(stmt) }
        } else {
            0
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_db_handle(stmt: *mut c_void) -> *mut c_void {
    if let Some(stmt) = as_stmt(stmt) {
        stmt.db as *mut c_void
    } else if let Some(func) =
        sym::<unsafe extern "C" fn(*mut c_void) -> *mut c_void>(b"sqlite3_db_handle\0")
    {
        unsafe { func(stmt) }
    } else {
        ptr::null_mut()
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_sql(stmt: *mut c_void) -> *const c_char {
    if let Some(stmt) = as_stmt(stmt) {
        stmt.sql_c.as_ptr()
    } else if let Some(func) =
        sym::<unsafe extern "C" fn(*mut c_void) -> *const c_char>(b"sqlite3_sql\0")
    {
        unsafe { func(stmt) }
    } else {
        ptr::null()
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_expanded_sql(stmt: *mut c_void) -> *mut c_char {
    if let Some(stmt) = as_stmt(stmt) {
        malloc_cstr(&stmt.sql)
    } else if let Some(func) =
        sym::<unsafe extern "C" fn(*mut c_void) -> *mut c_char>(b"sqlite3_expanded_sql\0")
    {
        unsafe { func(stmt) }
    } else {
        ptr::null_mut()
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_next_stmt(db: *mut c_void, stmt: *mut c_void) -> *mut c_void {
    if let Some(proxy) = as_db(db) {
        let stmts = lock(&proxy.stmts);
        if stmt.is_null() {
            return stmts.first().copied().unwrap_or(0) as *mut c_void;
        }
        match stmts.iter().position(|item| *item == stmt as usize) {
            Some(pos) => stmts.get(pos + 1).copied().unwrap_or(0) as *mut c_void,
            None => ptr::null_mut(),
        }
    } else if let Some(func) = sym::<unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void>(
        b"sqlite3_next_stmt\0",
    ) {
        unsafe { func(db, stmt) }
    } else {
        ptr::null_mut()
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_total_changes(db: *mut c_void) -> i32 {
    guard(|| {
        if let Some(proxy) = as_db(db) {
            *lock(&proxy.total_changes)
        } else if let Some(func) =
            sym::<unsafe extern "C" fn(*mut c_void) -> i32>(b"sqlite3_total_changes\0")
        {
            unsafe { func(db) }
        } else {
            0
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_changes64(db: *mut c_void) -> i64 {
    unsafe { super::sqlite3_changes(db) as i64 }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_total_changes64(db: *mut c_void) -> i64 {
    unsafe { sqlite3_total_changes(db) as i64 }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_extended_errcode(db: *mut c_void) -> i32 {
    unsafe { super::sqlite3_errcode(db) }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_busy_timeout(db: *mut c_void, ms: i32) -> i32 {
    guard(|| {
        if as_db(db).is_some() {
            SQLITE_OK
        } else if let Some(func) =
            sym::<unsafe extern "C" fn(*mut c_void, i32) -> i32>(b"sqlite3_busy_timeout\0")
        {
            unsafe { func(db, ms) }
        } else {
            SQLITE_ERROR
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_busy_handler(
    db: *mut c_void,
    handler: *mut c_void,
    arg: *mut c_void,
) -> i32 {
    guard(|| {
        if as_db(db).is_some() {
            SQLITE_OK
        } else if let Some(func) = sym::<
            unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32,
        >(b"sqlite3_busy_handler\0")
        {
            unsafe { func(db, handler, arg) }
        } else {
            SQLITE_ERROR
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_extended_result_codes(db: *mut c_void, on: i32) -> i32 {
    guard(|| {
        if as_db(db).is_some() {
            SQLITE_OK
        } else if let Some(func) =
            sym::<unsafe extern "C" fn(*mut c_void, i32) -> i32>(b"sqlite3_extended_result_codes\0")
        {
            unsafe { func(db, on) }
        } else {
            SQLITE_ERROR
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_limit(db: *mut c_void, id: i32, new_val: i32) -> i32 {
    guard(|| {
        if as_db(db).is_some() {
            let _ = new_val;
            limit_value(id)
        } else if let Some(func) =
            sym::<unsafe extern "C" fn(*mut c_void, i32, i32) -> i32>(b"sqlite3_limit\0")
        {
            unsafe { func(db, id, new_val) }
        } else {
            limit_value(id)
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_db_config(db: *mut c_void, op: i32, a: i32, b: *mut i32) -> i32 {
    guard(|| {
        if as_db(db).is_some() {
            if !b.is_null() {
                unsafe { *b = a };
            }
            SQLITE_OK
        } else if let Some(func) = sym::<unsafe extern "C" fn(*mut c_void, i32, i32, *mut i32) -> i32>(
            b"sqlite3_db_config\0",
        ) {
            unsafe { func(db, op, a, b) }
        } else {
            SQLITE_ERROR
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_db_filename(db: *mut c_void, name: *const c_char) -> *const c_char {
    if let Some(proxy) = as_db(db) {
        let schema = if name.is_null() {
            ""
        } else {
            unsafe { CStr::from_ptr(name) }.to_str().unwrap_or("")
        };
        if schema.is_empty() || schema == "main" {
            proxy.filename.as_ptr()
        } else {
            ptr::null()
        }
    } else if let Some(func) =
        sym::<unsafe extern "C" fn(*mut c_void, *const c_char) -> *const c_char>(
            b"sqlite3_db_filename\0",
        )
    {
        unsafe { func(db, name) }
    } else {
        ptr::null()
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_db_readonly(db: *mut c_void, name: *const c_char) -> i32 {
    guard(|| {
        if as_db(db).is_some() {
            0
        } else if let Some(func) =
            sym::<unsafe extern "C" fn(*mut c_void, *const c_char) -> i32>(b"sqlite3_db_readonly\0")
        {
            unsafe { func(db, name) }
        } else {
            -1
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_set_authorizer(
    db: *mut c_void,
    callback: *mut c_void,
    arg: *mut c_void,
) -> i32 {
    guard(|| {
        if as_db(db).is_some() {
            let _ = (callback, arg);
            SQLITE_OK
        } else if let Some(func) = sym::<
            unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32,
        >(b"sqlite3_set_authorizer\0")
        {
            unsafe { func(db, callback, arg) }
        } else {
            SQLITE_ERROR
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_trace_v2(
    db: *mut c_void,
    mask: u32,
    callback: *mut c_void,
    arg: *mut c_void,
) -> i32 {
    guard(|| {
        if as_db(db).is_some() {
            let _ = (mask, callback, arg);
            SQLITE_OK
        } else if let Some(func) = sym::<
            unsafe extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> i32,
        >(b"sqlite3_trace_v2\0")
        {
            unsafe { func(db, mask, callback, arg) }
        } else {
            SQLITE_ERROR
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_progress_handler(
    db: *mut c_void,
    n: i32,
    callback: *mut c_void,
    arg: *mut c_void,
) {
    if as_db(db).is_some() {
        return;
    }
    if let Some(func) =
        sym::<unsafe extern "C" fn(*mut c_void, i32, *mut c_void, *mut c_void)>(
            b"sqlite3_progress_handler\0",
        )
    {
        unsafe { func(db, n, callback, arg) };
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_interrupt(db: *mut c_void) {
    if as_db(db).is_some() {
        return;
    }
    if let Some(func) = sym::<unsafe extern "C" fn(*mut c_void)>(b"sqlite3_interrupt\0") {
        unsafe { func(db) };
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_function(
    db: *mut c_void,
    name: *const c_char,
    nargs: i32,
    flags: i32,
    app: *mut c_void,
    func: *mut c_void,
    step: *mut c_void,
    final_fn: *mut c_void,
) -> i32 {
    guard(|| {
        if as_db(db).is_some() {
            let _ = (name, nargs, flags, app, func, step, final_fn);
            return SQLITE_OK;
        }
        type Fn = unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            i32,
            i32,
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
        ) -> i32;
        if let Some(real) = sym::<Fn>(b"sqlite3_create_function\0") {
            unsafe { real(db, name, nargs, flags, app, func, step, final_fn) }
        } else {
            SQLITE_ERROR
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_function_v2(
    db: *mut c_void,
    name: *const c_char,
    nargs: i32,
    flags: i32,
    app: *mut c_void,
    func: *mut c_void,
    step: *mut c_void,
    final_fn: *mut c_void,
    destroy: *mut c_void,
) -> i32 {
    guard(|| {
        if as_db(db).is_some() {
            let _ = (name, nargs, flags, app, func, step, final_fn, destroy);
            return SQLITE_OK;
        }
        type Fn = unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            i32,
            i32,
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
        ) -> i32;
        if let Some(real) = sym::<Fn>(b"sqlite3_create_function_v2\0") {
            unsafe { real(db, name, nargs, flags, app, func, step, final_fn, destroy) }
        } else {
            SQLITE_ERROR
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_window_function(
    db: *mut c_void,
    name: *const c_char,
    nargs: i32,
    flags: i32,
    app: *mut c_void,
    step: *mut c_void,
    final_fn: *mut c_void,
    value: *mut c_void,
    inverse: *mut c_void,
    destroy: *mut c_void,
) -> i32 {
    guard(|| {
        if as_db(db).is_some() {
            let _ = (name, nargs, flags, app, step, final_fn, value, inverse, destroy);
            return SQLITE_OK;
        }
        type Fn = unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            i32,
            i32,
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
        ) -> i32;
        if let Some(real) = sym::<Fn>(b"sqlite3_create_window_function\0") {
            unsafe {
                real(
                    db, name, nargs, flags, app, step, final_fn, value, inverse, destroy,
                )
            }
        } else {
            SQLITE_ERROR
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_collation(
    db: *mut c_void,
    name: *const c_char,
    enc: i32,
    arg: *mut c_void,
    cmp: *mut c_void,
) -> i32 {
    guard(|| {
        if as_db(db).is_some() {
            let _ = (name, enc, arg, cmp);
            return SQLITE_OK;
        }
        type Fn = unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            i32,
            *mut c_void,
            *mut c_void,
        ) -> i32;
        if let Some(real) = sym::<Fn>(b"sqlite3_create_collation\0") {
            unsafe { real(db, name, enc, arg, cmp) }
        } else {
            SQLITE_ERROR
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_create_collation_v2(
    db: *mut c_void,
    name: *const c_char,
    enc: i32,
    arg: *mut c_void,
    cmp: *mut c_void,
    destroy: *mut c_void,
) -> i32 {
    guard(|| {
        if as_db(db).is_some() {
            let _ = (name, enc, arg, cmp, destroy);
            return SQLITE_OK;
        }
        type Fn = unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            i32,
            *mut c_void,
            *mut c_void,
            *mut c_void,
        ) -> i32;
        if let Some(real) = sym::<Fn>(b"sqlite3_create_collation_v2\0") {
            unsafe { real(db, name, enc, arg, cmp, destroy) }
        } else {
            SQLITE_ERROR
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_enable_load_extension(db: *mut c_void, on: i32) -> i32 {
    guard(|| {
        if as_db(db).is_some() {
            let _ = on;
            SQLITE_OK
        } else if let Some(func) = sym::<unsafe extern "C" fn(*mut c_void, i32) -> i32>(
            b"sqlite3_enable_load_extension\0",
        ) {
            unsafe { func(db, on) }
        } else {
            SQLITE_ERROR
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_load_extension(
    db: *mut c_void,
    file: *const c_char,
    proc_name: *const c_char,
    err: *mut *mut c_char,
) -> i32 {
    guard(|| {
        if let Some(proxy) = as_db(db) {
            proxy.fail("sqlite-broker does not load extensions");
            if !err.is_null() {
                unsafe { *err = malloc_cstr("sqlite-broker does not load extensions") };
            }
            return SQLITE_ERROR;
        }
        type Fn = unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            *const c_char,
            *mut *mut c_char,
        ) -> i32;
        if let Some(real) = sym::<Fn>(b"sqlite3_load_extension\0") {
            unsafe { real(db, file, proc_name, err) }
        } else {
            SQLITE_ERROR
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_blob_open(
    db: *mut c_void,
    schema: *const c_char,
    table: *const c_char,
    column: *const c_char,
    row: i64,
    flags: i32,
    out: *mut *mut c_void,
) -> i32 {
    guard(|| {
        if let Some(proxy) = as_db(db) {
            proxy.fail("sqlite-broker does not support incremental blobs");
            if !out.is_null() {
                unsafe { *out = ptr::null_mut() };
            }
            return SQLITE_ERROR;
        }
        type Fn = unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            *const c_char,
            *const c_char,
            i64,
            i32,
            *mut *mut c_void,
        ) -> i32;
        if let Some(real) = sym::<Fn>(b"sqlite3_blob_open\0") {
            unsafe { real(db, schema, table, column, row, flags, out) }
        } else {
            SQLITE_ERROR
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_backup_init(
    dest: *mut c_void,
    dest_name: *const c_char,
    source: *mut c_void,
    source_name: *const c_char,
) -> *mut c_void {
    if as_db(dest).is_some() || as_db(source).is_some() {
        if let Some(proxy) = as_db(dest).or_else(|| as_db(source)) {
            proxy.fail("sqlite-broker does not support backup");
        }
        return ptr::null_mut();
    }
    type Fn = unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *mut c_void,
        *const c_char,
    ) -> *mut c_void;
    if let Some(real) = sym::<Fn>(b"sqlite3_backup_init\0") {
        unsafe { real(dest, dest_name, source, source_name) }
    } else {
        ptr::null_mut()
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_serialize(
    db: *mut c_void,
    schema: *const c_char,
    size: *mut i64,
    flags: u32,
) -> *mut u8 {
    if as_db(db).is_some() {
        if !size.is_null() {
            unsafe { *size = 0 };
        }
        return ptr::null_mut();
    }
    type Fn = unsafe extern "C" fn(*mut c_void, *const c_char, *mut i64, u32) -> *mut u8;
    if let Some(real) = sym::<Fn>(b"sqlite3_serialize\0") {
        unsafe { real(db, schema, size, flags) }
    } else {
        ptr::null_mut()
    }
}

#[no_mangle]
pub unsafe extern "C" fn sqlite3_deserialize(
    db: *mut c_void,
    schema: *const c_char,
    data: *mut u8,
    size: i64,
    buf: i64,
    flags: u32,
) -> i32 {
    guard(|| {
        if let Some(proxy) = as_db(db) {
            proxy.fail("sqlite-broker does not support deserialize");
            return SQLITE_ERROR;
        }
        type Fn = unsafe extern "C" fn(*mut c_void, *const c_char, *mut u8, i64, i64, u32) -> i32;
        if let Some(real) = sym::<Fn>(b"sqlite3_deserialize\0") {
            unsafe { real(db, schema, data, size, buf, flags) }
        } else {
            SQLITE_ERROR
        }
    })
}
