use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=src");
    let lib = find_sqlite();
    println!("cargo:rerun-if-changed={}", lib.display());
    let implemented = implemented_symbols();
    let symbols = exported_functions(&lib);
    let forward: Vec<_> = symbols
        .into_iter()
        .filter(|name| !implemented.contains(name))
        .collect();
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let mut rust = String::from(
        r#"extern "C" {
"#,
    );
    let mut asm = String::new();
    let mut fill = String::new();
    for name in &forward {
        let slot = format!("{name}__real");
        rust.push_str(&format!("    static mut {slot}: *mut std::ffi::c_void;\n"));
        asm.push_str(&format!(
            ".bss\n.align 8\n.globl {slot}\n.hidden {slot}\n{slot}:\n.zero 8\n.text\n.globl {name}\n.type {name}, @function\n{name}:\n.byte 0xf3, 0x0f, 0x1e, 0xfa\njmp *{slot}(%rip)\n"
        ));
        fill.push_str(&format!(
            "    {slot} = libc::dlsym(lib, c\"{name}\".as_ptr());\n"
        ));
        println!("cargo:rustc-cdylib-link-arg=-Wl,-u,{name}");
    }
    rust.push_str("}\n\n");
    rust.push_str("std::arch::global_asm!(\n");
    rust.push_str(&escape_asm(&format!(".text\n{asm}")));
    rust.push_str(",\n    options(att_syntax)\n);\n\n");
    rust.push_str("unsafe extern \"C\" fn init_real_forwarders() {\n");
    rust.push_str("    let lib = open_real();\n");
    rust.push_str("    if lib.is_null() {\n");
    rust.push_str("        return;\n");
    rust.push_str("    }\n");
    rust.push_str("    unsafe {\n");
    rust.push_str(&fill);
    rust.push_str("    }\n");
    rust.push_str("}\n");
    fs::write(out_dir.join("forward.rs"), rust).unwrap();
    let map = out_dir.join("export.map");
    fs::write(
        &map,
        "{\n  global:\n    sqlite3_*;\n  local:\n    *;\n};\n",
    )
    .unwrap();
    println!(
        "cargo:rustc-cdylib-link-arg=-Wl,--version-script={}",
        map.display()
    );
    let _ = forward;
}

fn escape_asm(asm: &str) -> String {
    let mut out = String::from("r#\"");
    out.push_str(asm);
    out.push_str("\"#");
    out
}

fn implemented_symbols() -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    let src = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("src");
    for entry in fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap();
        for token in text.split("fn sqlite3_") {
            let name: String = token
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
                .collect();
            if !name.is_empty() && token.starts_with(&name) {
                set.insert(format!("sqlite3_{name}"));
            }
        }
    }
    set
}

fn exported_functions(lib: &PathBuf) -> Vec<String> {
    let output = Command::new("nm")
        .args(["-D", "--defined-only", lib.to_str().unwrap()])
        .output()
        .expect("nm libsqlite3");
    if !output.status.success() {
        panic!(
            "nm {}: {}",
            lib.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut names = Vec::new();
    for line in text.lines() {
        let cols: Vec<_> = line.split_whitespace().collect();
        if cols.len() < 3 {
            continue;
        }
        let kind = cols[cols.len() - 2];
        let name = cols[cols.len() - 1];
        if !matches!(kind, "T" | "t" | "W" | "w") || !name.starts_with("sqlite3_") {
            continue;
        }
        if name.contains('@') {
            continue;
        }
        names.push(name.to_string());
    }
    names.sort();
    names.dedup();
    names
}

fn find_sqlite() -> PathBuf {
    for path in [
        "/usr/lib/x86_64-linux-gnu/libsqlite3.so.0",
        "/lib/x86_64-linux-gnu/libsqlite3.so.0",
        "/usr/lib/aarch64-linux-gnu/libsqlite3.so.0",
        "/usr/lib64/libsqlite3.so.0",
        "/usr/lib/libsqlite3.so.0",
        "/lib64/libsqlite3.so.0",
        "/lib/libsqlite3.so.0",
    ] {
        if PathBuf::from(path).is_file() {
            return PathBuf::from(path);
        }
    }
    panic!("libsqlite3.so.0 not found; install libsqlite3");
}
