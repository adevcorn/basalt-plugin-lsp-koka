//! lsp-koka — Koka language-server bridge for Basalt.
//!
//! Wraps `koka --language-server --lsstdio` through the host LSP process imports:
//! `basalt_spawn_lsp`, `basalt_lsp_write`, `basalt_lsp_read`,
//! `basalt_lsp_stop`, and `basalt_sleep_ms`.

#![allow(static_mut_refs)]

use std::collections::HashMap;

use basalt_plugin_sdk::prelude::*;
use serde_json::{json, Value};

basalt_plugin_meta! {
    name:              "lsp-koka",
    version:           "0.1.0",
    hook_flags:        CAP_DIAGNOSTICS | CAP_HOVER,
    provides:          "lsp:koka",
    requires:          "",
    file_globs:        "**/*.kk\n**/*.kki",
    activates_on:      "koka.toml\n**/koka.toml\nkoka.yaml\n**/koka.yaml\n**/*.kk",
    activation_events: "",
}

#[cfg(target_arch = "wasm32")]
extern "C" {
    fn basalt_spawn_lsp(cmd_ptr: i32, cmd_len: i32, root_ptr: i32, root_len: i32) -> i32;
    fn basalt_lsp_write(handle: i32, buf_ptr: i32, buf_len: i32) -> i32;
    fn basalt_lsp_read(handle: i32, out_ptr: i32, out_cap: i32) -> i32;
    fn basalt_lsp_stop(handle: i32);
    fn basalt_sleep_ms(ms: i32);
    fn basalt_read_file(path_ptr: i32, path_len: i32, out_ptr: i32, out_cap: i32) -> i32;
}

#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
unsafe extern "C" fn basalt_spawn_lsp(_cmd: i32, _cmd_len: i32, _root: i32, _root_len: i32) -> i32 {
    -1
}

#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
unsafe extern "C" fn basalt_lsp_write(_handle: i32, _buf: i32, _len: i32) -> i32 {
    -1
}

#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
unsafe extern "C" fn basalt_lsp_read(_handle: i32, _out: i32, _cap: i32) -> i32 {
    -1
}

#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
unsafe extern "C" fn basalt_lsp_stop(_handle: i32) {}

#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
unsafe extern "C" fn basalt_sleep_ms(_ms: i32) {}

#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
unsafe extern "C" fn basalt_read_file(_path: i32, _path_len: i32, _out: i32, _cap: i32) -> i32 {
    -1
}

const READ_BUF_SIZE: usize = 64 * 1024;
const FILE_PROBE_SIZE: usize = 1024;

static mut READ_BUF: [u8; READ_BUF_SIZE] = [0u8; READ_BUF_SIZE];
static mut FILE_PROBE_BUF: [u8; FILE_PROBE_SIZE] = [0u8; FILE_PROBE_SIZE];

struct LspClient {
    handle: i32,
    buffer: Vec<u8>,
    next_id: i64,
    versions: HashMap<String, i32>,
}

static mut CLIENT: Option<LspClient> = None;

fn ensure_client(path: &str) -> bool {
    unsafe {
        if CLIENT.is_some() {
            return true;
        }
    }

    let root = project_root(path);
    let cmd = b"koka\0--language-server\0--lsstdio";
    let handle = unsafe {
        basalt_spawn_lsp(
            cmd.as_ptr() as i32,
            cmd.len() as i32,
            root.as_ptr() as i32,
            root.len() as i32,
        )
    };
    if handle < 0 {
        return false;
    }

    let mut client = LspClient {
        handle,
        buffer: Vec::new(),
        next_id: 2,
        versions: HashMap::new(),
    };

    let root_uri = path_to_uri(&root);
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": null,
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": { "relatedInformation": true },
                    "hover": { "contentFormat": ["markdown", "plaintext"] }
                }
            }
        }
    });
    send_msg(&mut client, &init);
    let _ = read_until_id(&mut client, 1, 5000);
    send_msg(
        &mut client,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    unsafe {
        CLIENT = Some(client);
    }
    true
}

#[basalt_plugin]
fn diagnose(src: &[u8], path: &str) -> Vec<Diagnostic> {
    let Ok(text) = core::str::from_utf8(src) else {
        return Vec::new();
    };
    if !ensure_client(path) {
        return Vec::new();
    }

    let client = unsafe { CLIENT.as_mut().unwrap() };
    open_or_update(client, path, text, "koka");

    let target_uri = path_to_uri(path);
    let mut diags = Vec::new();
    for _ in 0..40 {
        pump_read(client, 50);
        while let Some(msg) = pop_frame(&mut client.buffer) {
            if msg.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics")
            {
                if let Some(params) = msg.get("params") {
                    let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
                    if uri.eq_ignore_ascii_case(&target_uri) {
                        diags = parse_diagnostics(params, text);
                        return diags;
                    }
                }
            }
        }
    }
    diags
}

#[basalt_plugin]
fn hover(src: &[u8], path: &str, byte_offset: u32) -> String {
    let Ok(text) = core::str::from_utf8(src) else {
        return String::new();
    };
    if !ensure_client(path) {
        return String::new();
    }

    let client = unsafe { CLIENT.as_mut().unwrap() };
    open_or_update(client, path, text, "koka");

    let (line, character) = byte_offset_to_lsp(text, byte_offset as usize);
    let id = client.next_id;
    client.next_id += 1;

    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "textDocument/hover",
        "params": {
            "textDocument": { "uri": path_to_uri(path) },
            "position": { "line": line, "character": character }
        }
    });
    send_msg(client, &req);

    if let Some(resp) = read_until_id(client, id, 2500) {
        if let Some(result) = resp.get("result") {
            return extract_hover_markdown(result);
        }
    }
    String::new()
}

fn open_or_update(client: &mut LspClient, path: &str, text: &str, lang_id: &str) {
    let uri = path_to_uri(path);
    let version = match client.versions.get_mut(&uri) {
        Some(v) => {
            *v += 1;
            let req = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": { "uri": uri, "version": *v },
                    "contentChanges": [{ "text": text }]
                }
            });
            send_msg(client, &req);
            return;
        }
        None => 1,
    };
    client.versions.insert(uri.clone(), version);
    let req = json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": uri,
                "languageId": lang_id,
                "version": version,
                "text": text
            }
        }
    });
    send_msg(client, &req);
}

// ── JSON-RPC framing ────────────────────────────────────────────────────────

fn frame_message(msg: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(msg).unwrap_or_default();
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut framed = Vec::with_capacity(header.len() + body.len());
    framed.extend_from_slice(header.as_bytes());
    framed.extend_from_slice(&body);
    framed
}

fn send_msg(client: &mut LspClient, msg: &Value) {
    let framed = frame_message(msg);
    unsafe {
        let _ = basalt_lsp_write(client.handle, framed.as_ptr() as i32, framed.len() as i32);
    }
}

fn pump_read(client: &mut LspClient, sleep_ms: i32) {
    let buf: &mut [u8; READ_BUF_SIZE] = unsafe { &mut *core::ptr::addr_of_mut!(READ_BUF) };
    let n = unsafe { basalt_lsp_read(client.handle, buf.as_mut_ptr() as i32, READ_BUF_SIZE as i32) };
    if n > 0 {
        client.buffer.extend_from_slice(&buf[..n as usize]);
    } else if sleep_ms > 0 {
        unsafe { basalt_sleep_ms(sleep_ms) };
    }
}

fn pop_frame(buf: &mut Vec<u8>) -> Option<Value> {
    let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let header_str = core::str::from_utf8(&buf[..header_end]).ok()?;
    let mut content_len = None;
    for line in header_str.split("\r\n") {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_len = v.trim().parse::<usize>().ok();
            }
        }
    }
    let len = content_len?;
    let body_start = header_end + 4;
    if buf.len() < body_start + len {
        return None;
    }
    let val = serde_json::from_slice::<Value>(&buf[body_start..body_start + len]).ok();
    buf.drain(..body_start + len);
    val
}

fn read_until_id(client: &mut LspClient, target_id: i64, timeout_ms: i32) -> Option<Value> {
    let steps = (timeout_ms / 25).max(1);
    for _ in 0..steps {
        pump_read(client, 25);
        while let Some(msg) = pop_frame(&mut client.buffer) {
            if msg.get("id").and_then(Value::as_i64) == Some(target_id) {
                return Some(msg);
            }
        }
    }
    None
}

// ── LSP conversions ─────────────────────────────────────────────────────────

fn parse_diagnostics(params: &Value, text: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let Some(arr) = params.get("diagnostics").and_then(Value::as_array) else {
        return out;
    };
    for d in arr {
        let Some(range) = d.get("range") else { continue };
        let start = range.get("start");
        let end = range.get("end");
        let s_line = start.and_then(|p| p.get("line")).and_then(Value::as_u64).unwrap_or(0) as usize;
        let s_char = start.and_then(|p| p.get("character")).and_then(Value::as_u64).unwrap_or(0) as usize;
        let e_line = end.and_then(|p| p.get("line")).and_then(Value::as_u64).unwrap_or(s_line as u64) as usize;
        let e_char = end.and_then(|p| p.get("character")).and_then(Value::as_u64).unwrap_or(s_char as u64) as usize;

        let start_off = lsp_to_byte_offset(text, s_line, s_char) as u32;
        let end_off = lsp_to_byte_offset(text, e_line, e_char) as u32;
        let sev = match d.get("severity").and_then(Value::as_u64).unwrap_or(1) {
            1 => Severity::Error,
            2 => Severity::Warning,
            3 => Severity::Info,
            _ => Severity::Hint,
        };
        let message = d.get("message").and_then(Value::as_str).unwrap_or("").to_string();
        if !message.is_empty() {
            out.push(Diagnostic {
                offset: start_off,
                length: end_off.saturating_sub(start_off).max(1),
                severity: sev,
                message,
            });
        }
    }
    out
}

fn extract_hover_markdown(result: &Value) -> String {
    let Some(contents) = result.get("contents") else {
        return String::new();
    };
    if let Some(s) = contents.as_str() {
        return s.to_string();
    }
    if let Some(obj) = contents.as_object() {
        if let Some(val) = obj.get("value").and_then(Value::as_str) {
            return val.to_string();
        }
    }
    if let Some(arr) = contents.as_array() {
        let parts: Vec<&str> = arr
            .iter()
            .filter_map(|item| {
                item.as_str()
                    .or_else(|| item.get("value").and_then(Value::as_str))
            })
            .collect();
        return parts.join("\n\n");
    }
    String::new()
}

pub fn lsp_to_byte_offset(text: &str, line: usize, utf16_col: usize) -> usize {
    let mut cur_line = 0usize;
    let mut line_start = 0usize;
    for (idx, b) in text.bytes().enumerate() {
        if cur_line == line {
            line_start = idx;
            break;
        }
        if b == b'\n' {
            cur_line += 1;
            line_start = idx + 1;
        }
    }
    if cur_line < line {
        return text.len();
    }
    let rest = &text[line_start..];
    let mut u16_count = 0usize;
    for (rel, ch) in rest.char_indices() {
        if ch == '\n' || u16_count >= utf16_col {
            return line_start + rel;
        }
        u16_count += ch.len_utf16();
    }
    text.len()
}

pub fn byte_offset_to_lsp(text: &str, byte_offset: usize) -> (usize, usize) {
    let clamped = byte_offset.min(text.len());
    let prefix = &text[..clamped];
    let line = prefix.bytes().filter(|&b| b == b'\n').count();
    let line_start = prefix.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let col: usize = prefix[line_start..].chars().map(|c| c.len_utf16()).sum();
    (line, col)
}

fn path_to_uri(path: &str) -> String {
    let norm = path.replace('\\', "/");
    if norm.starts_with("file://") {
        norm
    } else if norm.starts_with('/') {
        format!("file://{norm}")
    } else {
        format!("file:///{norm}")
    }
}

fn project_root(path: &str) -> String {
    let norm = path.replace('\\', "/");
    let mut cur = match norm.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => return ".".to_string(),
    };
    loop {
        if probe_file(&format!("{cur}/koka.toml")) || probe_file(&format!("{cur}/koka.yaml")) {
            return cur;
        }
        match cur.rsplit_once('/') {
            Some((parent, _)) if !parent.is_empty() => cur = parent.to_string(),
            _ => break,
        }
    }
    norm.rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_else(|| ".".to_string())
}

fn probe_file(path: &str) -> bool {
    let buf: &mut [u8; FILE_PROBE_SIZE] = unsafe { &mut *core::ptr::addr_of_mut!(FILE_PROBE_BUF) };
    let n = unsafe {
        basalt_read_file(
            path.as_ptr() as i32,
            path.len() as i32,
            buf.as_mut_ptr() as i32,
            FILE_PROBE_SIZE as i32,
        )
    };
    n >= 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_lsp_position_and_byte_offset() {
        let src = "fun hi() : console () {\n  println(\"ok\")\n}\n";
        let off = src.find("println").unwrap();
        let (line, col) = byte_offset_to_lsp(src, off);
        assert_eq!((line, col), (1, 2));
        assert_eq!(lsp_to_byte_offset(src, line, col), off);
    }

    #[test]
    fn frames_and_parses_jsonrpc_message() {
        let msg = json!({"jsonrpc": "2.0", "id": 1, "result": {}});
        let mut buf = frame_message(&msg);
        let parsed = pop_frame(&mut buf).unwrap();
        assert_eq!(parsed.get("id").and_then(Value::as_i64), Some(1));
        assert!(buf.is_empty());
    }

    #[test]
    fn flattens_hover_markup() {
        let res = json!({ "contents": { "kind": "markdown", "value": "```koka\nfun add(a: int, b: int): total int\n```" } });
        assert!(extract_hover_markdown(&res).contains("total int"));
    }
}
