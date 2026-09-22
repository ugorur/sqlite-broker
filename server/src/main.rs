mod broker;
mod capi;
mod hammer;

use std::env;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process;

const USAGE: &str = "\
usage:
  sqlite-broker serve --storage FILE --stub FILE [--listen ADDR]
  sqlite-broker call --db FILE --sql SQL [--sql SQL ...]
  sqlite-broker session --db FILE
  sqlite-broker hammer --db FILE --worker N [--ops 1000] [--seed N]
  sqlite-broker storm [--apps 10] [--ops 1000] [--dir PATH]";

fn main() {
    if let Err(err) = dispatch() {
        eprintln!("sqlite-broker: {err}");
        process::exit(1);
    }
}

fn dispatch() -> Result<(), String> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("serve") => serve_cli(args),
        Some("call") => call_cli(args),
        Some("session") => session_cli(args),
        Some("hammer") => hammer::hammer_cli(args),
        Some("storm") => hammer::storm_cli(args),
        Some("-h" | "--help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(format!("unknown command {other}\n{USAGE}")),
        None => Err(format!("missing command\n{USAGE}")),
    }
}

fn take(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("missing value for {flag}"))
}

fn serve_cli(args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut storage = None;
    let mut stub = None;
    let mut listen = "127.0.0.1:0".to_string();
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--storage" => storage = Some(PathBuf::from(take(&mut args, "--storage")?)),
            "--stub" => stub = Some(PathBuf::from(take(&mut args, "--stub")?)),
            "--listen" => listen = take(&mut args, "--listen")?,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let storage = storage.ok_or("missing --storage")?;
    let stub = stub.ok_or("missing --stub")?;
    broker::serve(storage, stub, &listen)
}

fn call_cli(args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut db_path = None;
    let mut sqls = Vec::new();
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--db" => db_path = Some(PathBuf::from(take(&mut args, "--db")?)),
            "--sql" => sqls.push(take(&mut args, "--sql")?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let db_path = db_path.ok_or("missing --db")?;
    if sqls.is_empty() {
        return Err("missing --sql".to_string());
    }
    let db = capi::Db::open(&db_path)?;
    for sql in &sqls {
        if is_query(sql) {
            for row in db.query(sql)? {
                emit_line(&format!("cell:{}", row.join(",")))?;
            }
        } else {
            db.exec(sql)?;
            emit_line("ok")?;
        }
    }
    Ok(())
}

fn session_cli(args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut db_path = None;
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--db" => db_path = Some(PathBuf::from(take(&mut args, "--db")?)),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let db_path = db_path.ok_or("missing --db")?;
    let db = capi::Db::open(&db_path)?;
    emit_line("READY")?;
    let stdin = io::stdin();
    let locked = stdin.lock();
    let mut lines = locked.lines();
    while let Some(line) = lines.next() {
        let line = line.map_err(|err| err.to_string())?;
        if line.is_empty() {
            continue;
        }
        if line == "HOLD" {
            emit_line("HOLDING")?;
            let go = lines
                .next()
                .ok_or("stdin closed while holding")?
                .map_err(|err| err.to_string())?;
            if go.trim() != "GO" {
                return Err(format!("expected GO, got {go}"));
            }
            continue;
        }
        if let Some(sql) = line.strip_prefix("EXEC ") {
            emit_line("START")?;
            if let Err(err) = db.exec(sql) {
                emit_line(&format!("ERR {err}"))?;
                process::exit(1);
            }
            emit_line(&format!(
                "OK changes={} rowid={}",
                db.changes(),
                db.last_insert_rowid()
            ))?;
            continue;
        }
        if let Some(sql) = line.strip_prefix("QUERY ") {
            emit_line("START")?;
            let rows = match db.query(sql) {
                Ok(rows) => rows,
                Err(err) => {
                    emit_line(&format!("ERR {err}"))?;
                    process::exit(1);
                }
            };
            for row in rows {
                emit_line(&format!("ROW {}", row.join("\t")))?;
            }
            emit_line("END")?;
            continue;
        }
        return Err(format!("bad command {line}"));
    }
    Ok(())
}

fn is_query(sql: &str) -> bool {
    let mut rest = sql.trim_start();
    loop {
        if let Some(stripped) = rest.strip_prefix("--") {
            if let Some((_, after)) = stripped.split_once('\n') {
                rest = after.trim_start();
                continue;
            }
            return false;
        }
        break;
    }
    let head = rest.get(..16).unwrap_or(rest).to_ascii_uppercase();
    head.starts_with("SELECT") || head.starts_with("WITH") || head.starts_with("EXPLAIN")
}

fn emit_line(line: &str) -> Result<(), String> {
    let mut out = io::stdout();
    writeln!(out, "{line}").map_err(|err| err.to_string())?;
    out.flush().map_err(|err| err.to_string())?;
    Ok(())
}
