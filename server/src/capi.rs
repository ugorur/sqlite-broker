//! Client side of the drop-in. Every call below is a dynamic relocation to
//! libsqlite3. With LD_PRELOAD=libsqlite_broker.so those relocations hit the shim,
//! which still enters through sqlite3_open.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::path::Path;
use std::ptr;

const SQLITE_OK: i32 = 0;
const SQLITE_ROW: i32 = 100;
const SQLITE_DONE: i32 = 101;

#[link(name = "sqlite3")]
unsafe extern "C" {
    fn sqlite3_open(filename: *const c_char, ppdb: *mut *mut c_void) -> i32;
    fn sqlite3_close(db: *mut c_void) -> i32;
    fn sqlite3_exec(
        db: *mut c_void,
        sql: *const c_char,
        callback: *mut c_void,
        arg: *mut c_void,
        errmsg: *mut *mut c_char,
    ) -> i32;
    fn sqlite3_prepare_v2(
        db: *mut c_void,
        sql: *const c_char,
        nbytes: i32,
        ppstmt: *mut *mut c_void,
        pztail: *mut *const c_char,
    ) -> i32;
    fn sqlite3_step(stmt: *mut c_void) -> i32;
    fn sqlite3_finalize(stmt: *mut c_void) -> i32;
    fn sqlite3_column_count(stmt: *mut c_void) -> i32;
    fn sqlite3_column_text(stmt: *mut c_void, i: i32) -> *const u8;
    fn sqlite3_changes(db: *mut c_void) -> i32;
    fn sqlite3_last_insert_rowid(db: *mut c_void) -> i64;
    fn sqlite3_errmsg(db: *mut c_void) -> *const c_char;
}

pub struct Db(*mut c_void);

impl Db {
    pub fn open(path: &Path) -> Result<Self, String> {
        let text = path.to_str().ok_or("database path is not utf-8")?;
        let c_path = CString::new(text).map_err(|err| err.to_string())?;
        let mut db = ptr::null_mut();
        let rc = unsafe { sqlite3_open(c_path.as_ptr(), &mut db) };
        if rc != SQLITE_OK {
            let message = if db.is_null() {
                format!("sqlite3_open failed ({rc})")
            } else {
                let message = errmsg(db);
                unsafe { sqlite3_close(db) };
                message
            };
            return Err(message);
        }
        Ok(Db(db))
    }

    pub fn exec(&self, sql: &str) -> Result<(), String> {
        let c_sql = CString::new(sql).map_err(|err| err.to_string())?;
        let rc = unsafe {
            sqlite3_exec(
                self.0,
                c_sql.as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if rc != SQLITE_OK {
            Err(format!("sqlite3_exec ({rc}): {}", errmsg(self.0)))
        } else {
            Ok(())
        }
    }

    pub fn query(&self, sql: &str) -> Result<Vec<Vec<String>>, String> {
        let c_sql = CString::new(sql).map_err(|err| err.to_string())?;
        let mut stmt = ptr::null_mut();
        let rc =
            unsafe { sqlite3_prepare_v2(self.0, c_sql.as_ptr(), -1, &mut stmt, ptr::null_mut()) };
        if rc != SQLITE_OK {
            return Err(format!("sqlite3_prepare_v2 ({rc}): {}", errmsg(self.0)));
        }
        let mut rows = Vec::new();
        loop {
            let rc = unsafe { sqlite3_step(stmt) };
            if rc == SQLITE_ROW {
                let n = unsafe { sqlite3_column_count(stmt) };
                let mut row = Vec::with_capacity(n as usize);
                for i in 0..n {
                    let ptr = unsafe { sqlite3_column_text(stmt, i) };
                    if ptr.is_null() {
                        row.push(String::new());
                    } else {
                        let text = unsafe { CStr::from_ptr(ptr as *const c_char) }
                            .to_string_lossy()
                            .into_owned();
                        row.push(text);
                    }
                }
                rows.push(row);
            } else if rc == SQLITE_DONE {
                break;
            } else {
                let message = format!("sqlite3_step ({rc}): {}", errmsg(self.0));
                unsafe { sqlite3_finalize(stmt) };
                return Err(message);
            }
        }
        unsafe { sqlite3_finalize(stmt) };
        Ok(rows)
    }

    pub fn changes(&self) -> i32 {
        unsafe { sqlite3_changes(self.0) }
    }

    pub fn last_insert_rowid(&self) -> i64 {
        unsafe { sqlite3_last_insert_rowid(self.0) }
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { sqlite3_close(self.0) };
            self.0 = ptr::null_mut();
        }
    }
}

fn errmsg(db: *mut c_void) -> String {
    unsafe {
        let ptr = sqlite3_errmsg(db);
        if ptr.is_null() {
            "sqlite error".to_string()
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }
}
