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
    Blob { v: String },
}

impl Cell {
    pub fn render(&self) -> String {
        match self {
            Cell::Null => String::new(),
            Cell::Int { v } => v.to_string(),
            Cell::Real { v } => v.to_string(),
            Cell::Text { v } => v.clone(),
            Cell::Blob { v } => decode_base64(v)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default(),
        }
    }

    pub fn blob_bytes(&self) -> Option<Vec<u8>> {
        match self {
            Cell::Blob { v } => decode_base64(v).ok(),
            _ => None,
        }
    }
}

pub fn encode_base64(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut chunks = data.chunks_exact(3);
    for chunk in chunks.by_ref() {
        let n = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | chunk[2] as u32;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push(TABLE[(n & 63) as usize] as char);
    }
    let rest = chunks.remainder();
    if rest.len() == 1 {
        let n = (rest[0] as u32) << 16;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rest.len() == 2 {
        let n = ((rest[0] as u32) << 16) | ((rest[1] as u32) << 8);
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push('=');
    }
    out
}

pub fn decode_base64(text: &str) -> Result<Vec<u8>, ()> {
    fn val(byte: u8) -> Result<u8, ()> {
        match byte {
            b'A'..=b'Z' => Ok(byte - b'A'),
            b'a'..=b'z' => Ok(byte - b'a' + 26),
            b'0'..=b'9' => Ok(byte - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(()),
        }
    }
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks_exact(4) {
        if chunk[2] == b'=' && chunk[3] != b'=' {
            return Err(());
        }
        let pad = usize::from(chunk[2] == b'=') + usize::from(chunk[3] == b'=');
        let n = ((val(chunk[0])? as u32) << 18)
            | ((val(chunk[1])? as u32) << 12)
            | if chunk[2] == b'=' {
                0
            } else {
                (val(chunk[2])? as u32) << 6
            }
            | if chunk[3] == b'=' {
                0
            } else {
                val(chunk[3])? as u32
            };
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
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
    /// Compile the first statement only. Does not run it.
    Prepare { sql: String },
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
    /// Byte offset of the uncompiled tail after [`Request::Prepare`].
    #[serde(default)]
    pub tail: usize,
    #[serde(default)]
    pub param_count: i32,
    /// Empty string means a nameless `?` parameter. One entry per parameter.
    #[serde(default)]
    pub param_names: Vec<String>,
    /// Empty string means `sqlite3_column_decltype` returned NULL.
    #[serde(default)]
    pub decltypes: Vec<String>,
    #[serde(default)]
    pub total_changes: i32,
    /// [`Request::Prepare`] found comments or whitespace and no statement.
    #[serde(default)]
    pub empty: bool,
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
            Request::Prepare { .. } => panic!("decoded prepare"),
        }
    }

    #[test]
    fn base64_roundtrip_includes_nul() {
        let raw = b"\x00\x01hi";
        let encoded = encode_base64(raw);
        assert_eq!(decode_base64(&encoded).unwrap(), raw);
        assert_eq!(decode_base64("").unwrap(), b"");
    }
}
