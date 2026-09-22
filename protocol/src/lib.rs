//! Wire format between the in-process SQLite shim and the single writer.

use std::io::{Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const STUB_MAGIC: &str = "SQLITEBROKER1";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Cell {
    Null,
    Int { v: i64 },
    Real { v: f64 },
    Text { v: String },
}

impl Cell {
    pub fn render(&self) -> String {
        match self {
            Cell::Null => String::new(),
            Cell::Int { v } => v.to_string(),
            Cell::Real { v } => v.to_string(),
            Cell::Text { v } => v.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Param {
    pub index: i32,
    pub value: Cell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub ok: bool,
    pub session: u64,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Exec {
        sql: String,
        #[serde(default)]
        params: Vec<Param>,
    },
    Close,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub code: i32,
    #[serde(default)]
    pub columns: Vec<String>,
    #[serde(default)]
    pub rows: Vec<Vec<Cell>>,
    #[serde(default)]
    pub changes: i32,
    #[serde(default)]
    pub last_insert_rowid: i64,
    pub autocommit: bool,
}

pub fn format_stub(addr: &str) -> String {
    format!("{STUB_MAGIC}\n{addr}\n")
}

pub fn parse_stub(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.lines();
    if lines.next()? != STUB_MAGIC {
        return None;
    }
    let addr = lines.next()?.trim();
    if addr.is_empty() || addr.contains(char::is_whitespace) {
        return None;
    }
    Some(addr.to_string())
}

pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    if body.len() > 16 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    w.write_all(&(body.len() as u32).to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()?;
    Ok(())
}

pub fn read_msg<R: Read, T: DeserializeOwned>(r: &mut R) -> std::io::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 16 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn stub_roundtrip_rejects_sqlite_header() {
        let encoded = format_stub("127.0.0.1:9");
        assert_eq!(
            parse_stub(encoded.as_bytes()).as_deref(),
            Some("127.0.0.1:9")
        );
        assert!(parse_stub(b"SQLite format 3\0").is_none());
    }

    #[test]
    fn frame_roundtrip() {
        let msg = Request::Exec {
            sql: "SELECT 1".to_string(),
            params: vec![],
        };
        let mut cursor = Cursor::new(Vec::new());
        write_msg(&mut cursor, &msg).unwrap();
        cursor.set_position(0);
        let got: Request = read_msg(&mut cursor).unwrap();
        match got {
            Request::Exec { sql, params } => {
                assert_eq!(sql, "SELECT 1");
                assert!(params.is_empty());
            }
            Request::Close => panic!("decoded close"),
        }
    }
}
