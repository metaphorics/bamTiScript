//! Stdio LSP 3.17 adapter over [`bamts_compiler::service::ServiceState`].
//!
//! Checking stays in the compiler service. This module frames JSON-RPC,
//! maps document sync onto open/update/close, and forwards queries.

use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use bamts_compiler::checker::SymbolKind;
use bamts_compiler::diagnostic::DiagnosticSeverity;
use bamts_compiler::service::{
    DiagnosticEntry, DocumentSnapshot, Location, ServiceError, ServiceState,
    filesystem::{FileSystemError, OsFileSystem},
};
use bamts_compiler::source::{SourceText, TextRange, Utf16Pos};
use serde_json::{Value, json};

const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// How the stdio loop finished.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Exit {
    /// Client sent `shutdown` then `exit`.
    Shutdown,
    /// Client sent `exit` without `shutdown`, or the stream ended uncleanly.
    Unrequested,
}

/// Runs one LSP session on `input`/`output` confined to `root`.
pub fn run(input: impl BufRead, output: impl Write, root: impl AsRef<Path>) -> io::Result<Exit> {
    let mut session = Session::new(root.as_ref())?;
    session.serve(input, output)
}

struct Session {
    state: ServiceState<OsFileSystem>,
    snapshots: BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
    initialized: bool,
    shutdown: bool,
}

impl Session {
    fn new(root: &Path) -> io::Result<Self> {
        let filesystem = OsFileSystem::new(root).map_err(fs_io)?;
        Ok(Self {
            state: ServiceState::new(filesystem),
            snapshots: BTreeMap::new(),
            initialized: false,
            shutdown: false,
        })
    }

    fn serve(&mut self, mut input: impl BufRead, mut output: impl Write) -> io::Result<Exit> {
        loop {
            let Some(raw) = read_message(&mut input)? else {
                return Ok(if self.shutdown {
                    Exit::Shutdown
                } else {
                    Exit::Unrequested
                });
            };
            let parsed: Value = match serde_json::from_slice(&raw) {
                Ok(value) => value,
                Err(_) => {
                    write_json(
                        &mut output,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": Value::Null,
                            "error": { "code": -32700, "message": "Parse error" }
                        }),
                    )?;
                    continue;
                }
            };
            if parsed.get("method").and_then(Value::as_str) == Some("exit") {
                return Ok(if self.shutdown {
                    Exit::Shutdown
                } else {
                    Exit::Unrequested
                });
            }
            if let Some(responses) = self.dispatch(&parsed) {
                for response in responses {
                    write_json(&mut output, &response)?;
                }
            }
        }
    }

    fn dispatch(&mut self, message: &Value) -> Option<Vec<Value>> {
        let method = match message.get("method").and_then(Value::as_str) {
            Some(method) => method,
            None => {
                return message
                    .get("id")
                    .map(|id| vec![error_response(id.clone(), -32600, "Invalid Request")]);
            }
        };
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        if method == "initialize" {
            self.initialized = true;
            let id = id.unwrap_or(Value::Null);
            return Some(vec![json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "capabilities": {
                        "positionEncoding": "utf-16",
                        "textDocumentSync": { "openClose": true, "change": 1 },
                        "completionProvider": { "triggerCharacters": ["."] },
                        "definitionProvider": true,
                        "referencesProvider": true,
                        "renameProvider": true
                    },
                    "serverInfo": { "name": "bamts-lsp", "version": "0.2.0" }
                }
            })]);
        }

        if !self.initialized {
            return id.map(|id| vec![error_response(id, -32002, "Server not initialized")]);
        }

        match method {
            "initialized" | "textDocument/didSave" => None,
            "shutdown" => {
                self.shutdown = true;
                id.map(|id| vec![json!({ "jsonrpc": "2.0", "id": id, "result": Value::Null })])
            }
            "textDocument/didOpen" => self.did_open(&params),
            "textDocument/didChange" => self.did_change(&params),
            "textDocument/didClose" => self.did_close(&params),
            "textDocument/completion" => id.map(|id| vec![self.completion(id, &params)]),
            "textDocument/definition" => id.map(|id| vec![self.definition(id, &params)]),
            "textDocument/references" => id.map(|id| vec![self.references(id, &params)]),
            "textDocument/rename" => id.map(|id| vec![self.rename(id, &params)]),
            _ => id.map(|id| vec![error_response(id, -32601, "Method not found")]),
        }
    }

    fn did_open(&mut self, params: &Value) -> Option<Vec<Value>> {
        let doc = params.get("textDocument")?;
        let uri = doc.get("uri").and_then(Value::as_str)?;
        let text = doc.get("text").and_then(Value::as_str)?;
        let version = doc.get("version").and_then(Value::as_u64).unwrap_or(1);
        let path = match uri_to_path(uri) {
            Ok(path) => path,
            Err(message) => return Some(vec![invalid_params_notification(message)]),
        };
        match self.state.open(&path, text, version) {
            Ok(snapshot) => {
                self.snapshots
                    .insert(snapshot.path().to_path_buf(), snapshot);
                Some(vec![self.publish_diagnostics(&path)])
            }
            Err(error) => Some(vec![show_message(&error)]),
        }
    }

    fn did_change(&mut self, params: &Value) -> Option<Vec<Value>> {
        let doc = params.get("textDocument")?;
        let uri = doc.get("uri").and_then(Value::as_str)?;
        let version = doc.get("version").and_then(Value::as_u64)?;
        let text = params
            .get("contentChanges")
            .and_then(Value::as_array)
            .and_then(|changes| changes.last())
            .and_then(|change| {
                if change.get("range").is_some() {
                    None
                } else {
                    change.get("text").and_then(Value::as_str)
                }
            })?;
        let path = uri_to_path(uri).ok()?;
        match self.state.update(&path, text, version) {
            Ok(snapshot) => {
                self.snapshots
                    .insert(snapshot.path().to_path_buf(), snapshot);
                Some(vec![self.publish_diagnostics(&path)])
            }
            Err(error) => Some(vec![show_message(&error)]),
        }
    }

    fn did_close(&mut self, params: &Value) -> Option<Vec<Value>> {
        let uri = params
            .get("textDocument")
            .and_then(|doc| doc.get("uri"))
            .and_then(Value::as_str)?;
        let path = uri_to_path(uri).ok()?;
        let _ = self.state.close(&path);
        self.snapshots.remove(&path);
        Some(vec![json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": { "uri": uri, "diagnostics": [] }
        })])
    }

    fn publish_diagnostics(&mut self, path: &Path) -> Value {
        let uri = path_to_uri(path);
        let diagnostics = self
            .state
            .diagnostics(path)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| self.lsp_diagnostic(&entry))
            .collect::<Vec<_>>();
        json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": { "uri": uri, "diagnostics": diagnostics }
        })
    }

    fn lsp_diagnostic(&self, entry: &DiagnosticEntry) -> Option<Value> {
        let snapshot = self.snapshots.get(&entry.path)?;
        let range = lsp_range(snapshot.source().source_text(), entry.range)?;
        let severity = match entry.severity {
            DiagnosticSeverity::Error => 1,
            DiagnosticSeverity::Warning => 2,
        };
        Some(json!({
            "range": range,
            "severity": severity,
            "code": entry.code,
            "source": "bamts",
            "message": entry.message
        }))
    }

    fn completion(&mut self, id: Value, params: &Value) -> Value {
        let Some((path, position)) = self.doc_position(params) else {
            return error_response(id, -32602, "Invalid params");
        };
        match self.state.completions(&path, position) {
            Ok(items) => {
                let snapshot = self.snapshots.get(&path);
                let result: Vec<Value> = items
                    .into_iter()
                    .map(|item| {
                        let mut value = json!({
                            "label": item.name,
                            "kind": completion_kind(item.kind)
                        });
                        if let Some(snapshot) = snapshot
                            && let Some(range) =
                                lsp_range(snapshot.source().source_text(), item.replacement)
                        {
                            value["textEdit"] = json!({
                                "range": range,
                                "newText": item.name
                            });
                        }
                        value
                    })
                    .collect();
                json!({ "jsonrpc": "2.0", "id": id, "result": result })
            }
            Err(error) => error_response(id, -32603, &error.to_string()),
        }
    }

    fn definition(&mut self, id: Value, params: &Value) -> Value {
        let Some((path, position)) = self.doc_position(params) else {
            return error_response(id, -32602, "Invalid params");
        };
        match self.state.definition(&path, position) {
            Ok(Some(location)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": self.lsp_location(&location)
            }),
            Ok(None) => json!({ "jsonrpc": "2.0", "id": id, "result": Value::Null }),
            Err(error) => error_response(id, -32603, &error.to_string()),
        }
    }

    fn references(&mut self, id: Value, params: &Value) -> Value {
        let Some((path, position)) = self.doc_position(params) else {
            return error_response(id, -32602, "Invalid params");
        };
        let include_declaration = params
            .get("context")
            .and_then(|context| context.get("includeDeclaration"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        match self.state.references(&path, position) {
            Ok(mut locations) => {
                if !include_declaration
                    && let Ok(Some(definition)) = self.state.definition(&path, position)
                {
                    locations.retain(|location| {
                        location.path != definition.path || location.range != definition.range
                    });
                }
                let result: Vec<Value> = locations
                    .iter()
                    .filter_map(|location| self.lsp_location(location))
                    .collect();
                json!({ "jsonrpc": "2.0", "id": id, "result": result })
            }
            Err(error) => error_response(id, -32603, &error.to_string()),
        }
    }

    fn rename(&mut self, id: Value, params: &Value) -> Value {
        let Some((path, position)) = self.doc_position(params) else {
            return error_response(id, -32602, "Invalid params");
        };
        let Some(new_name) = params.get("newName").and_then(Value::as_str) else {
            return error_response(id, -32602, "Invalid params");
        };
        match self.state.rename(&path, position, new_name) {
            Ok(result) => {
                let mut changes = serde_json::Map::new();
                for edit in result.edit.edits {
                    let Some(snapshot) = self.snapshots.get(&edit.path) else {
                        continue;
                    };
                    let Some(range) = lsp_range(snapshot.source().source_text(), edit.range) else {
                        continue;
                    };
                    let uri = path_to_uri(&edit.path);
                    changes
                        .entry(uri)
                        .or_insert_with(|| json!([]))
                        .as_array_mut()
                        .expect("changes values are arrays")
                        .push(json!({ "range": range, "newText": edit.replacement }));
                }
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "changes": changes }
                })
            }
            Err(error) => error_response(id, -32603, &error.to_string()),
        }
    }

    fn lsp_location(&self, location: &Location) -> Option<Value> {
        let snapshot = self.snapshots.get(&location.path)?;
        let range = lsp_range(snapshot.source().source_text(), location.range)?;
        Some(json!({
            "uri": path_to_uri(&location.path),
            "range": range
        }))
    }

    fn doc_position(&self, params: &Value) -> Option<(PathBuf, Utf16Pos)> {
        let uri = params
            .get("textDocument")
            .and_then(|doc| doc.get("uri"))
            .and_then(Value::as_str)?;
        let position = params.get("position")?;
        let path = uri_to_path(uri).ok()?;
        let line = u32::try_from(position.get("line").and_then(Value::as_u64)?).ok()?;
        let character = u32::try_from(position.get("character").and_then(Value::as_u64)?).ok()?;
        let snapshot = self.snapshots.get(&path)?;
        Some((
            path.clone(),
            lsp_position(snapshot.source().source_text().as_str(), line, character),
        ))
    }
}

fn lsp_range(source: &SourceText, range: TextRange) -> Option<Value> {
    let (start_line, start_character) = source.line_column(range.start()).ok()?;
    let (end_line, end_character) = source.line_column(range.end()).ok()?;
    Some(json!({
        "start": { "line": start_line, "character": start_character },
        "end": { "line": end_line, "character": end_character }
    }))
}

fn lsp_position(source: &str, line: u32, character: u32) -> Utf16Pos {
    let mut current_line = 0u32;
    let mut current_character = 0u32;
    let mut utf16 = 0usize;
    let mut chars = source.chars().peekable();
    while let Some(ch) = chars.next() {
        if current_line == line && current_character >= character {
            return Utf16Pos::new(utf16);
        }
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
                utf16 = utf16.saturating_add(2);
            } else {
                utf16 = utf16.saturating_add(1);
            }
            current_line = current_line.saturating_add(1);
            current_character = 0;
            continue;
        }
        if ch == '\n' || ch == '\u{2028}' || ch == '\u{2029}' {
            current_line = current_line.saturating_add(1);
            current_character = 0;
            utf16 = utf16.saturating_add(1);
            continue;
        }
        if current_line == line {
            current_character = current_character.saturating_add(ch.len_utf16() as u32);
        }
        utf16 = utf16.saturating_add(ch.len_utf16());
    }
    Utf16Pos::new(utf16)
}

fn completion_kind(kind: SymbolKind) -> u32 {
    match kind {
        SymbolKind::Variable(_) => 6,
        _ => 6,
    }
}

fn uri_to_path(uri: &str) -> Result<PathBuf, String> {
    let rest = uri
        .strip_prefix("file://")
        .ok_or_else(|| format!("unsupported URI scheme: {uri}"))?;
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    let decoded = percent_decode(path)?;
    Ok(PathBuf::from(decoded))
}

fn path_to_uri(path: &Path) -> String {
    let display = path.to_string_lossy();
    let mut encoded = String::from("file://");
    for ch in display.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '/' | '_' | '-' | '.' | '~' => encoded.push(ch),
            _ => {
                for byte in ch.encode_utf8(&mut [0; 4]).bytes() {
                    encoded.push_str(&format!("%{byte:02X}"));
                }
            }
        }
    }
    encoded
}

fn percent_decode(input: &str) -> Result<String, String> {
    let mut output = Vec::new();
    let bytes = input.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hex = bytes
                    .get(index + 1..index + 3)
                    .ok_or_else(|| "truncated percent-encoding".to_owned())?;
                let text =
                    std::str::from_utf8(hex).map_err(|_| "invalid percent-encoding".to_owned())?;
                let value = u8::from_str_radix(text, 16)
                    .map_err(|_| "invalid percent-encoding".to_owned())?;
                output.push(value);
                index += 3;
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(output).map_err(|_| "URI path is not UTF-8".to_owned())
}

fn read_message(input: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut content_length = None;
    loop {
        let mut header = String::new();
        if input.read_line(&mut header)? == 0 {
            return Ok(None);
        }
        let header = header.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("Content-Length") {
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
            );
        }
    }
    let Some(length) = content_length else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing Content-Length",
        ));
    };
    if length > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "LSP message exceeds 16 MiB",
        ));
    }
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    Ok(Some(body))
}

fn write_json(output: &mut impl Write, value: &Value) -> io::Result<()> {
    let encoded = serde_json::to_vec(value)?;
    write!(output, "Content-Length: {}\r\n\r\n", encoded.len())?;
    output.write_all(&encoded)?;
    output.flush()
}

fn error_response(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

fn invalid_params_notification(message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "window/showMessage",
        "params": { "type": 1, "message": message }
    })
}

fn show_message(error: &ServiceError) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "window/showMessage",
        "params": { "type": 1, "message": error.to_string() }
    })
}

fn fs_io(error: FileSystemError) -> io::Error {
    io::Error::other(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame(value: &Value) -> Vec<u8> {
        let encoded = serde_json::to_vec(value).expect("json");
        let mut framed = format!("Content-Length: {}\r\n\r\n", encoded.len()).into_bytes();
        framed.extend_from_slice(&encoded);
        framed
    }

    fn read_all(bytes: &[u8]) -> Vec<Value> {
        let mut cursor = Cursor::new(bytes);
        let mut messages = Vec::new();
        while let Ok(Some(raw)) = read_message(&mut cursor) {
            messages.push(serde_json::from_slice(&raw).expect("json"));
        }
        messages
    }

    fn initialize(root: &Path) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "capabilities": {},
                "rootUri": path_to_uri(root)
            }
        })
    }

    #[test]
    fn handshake_frames_lifecycle_and_exit() {
        let root = tempfile::tempdir().expect("temp");
        let mut input = Vec::new();
        input.extend(frame(&initialize(root.path())));
        input.extend(frame(&json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        })));
        input.extend(frame(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "shutdown",
            "params": null
        })));
        input.extend(frame(&json!({
            "jsonrpc": "2.0",
            "method": "exit"
        })));
        let mut output = Vec::new();
        let exit = run(Cursor::new(input), &mut output, root.path()).expect("run");
        assert_eq!(exit, Exit::Shutdown);
        let messages = read_all(&output);
        assert_eq!(
            messages[0]["result"]["capabilities"]["positionEncoding"],
            "utf-16"
        );
        assert_eq!(
            messages[0]["result"]["capabilities"]["textDocumentSync"]["change"],
            1
        );
        assert_eq!(messages[1]["id"], 2);
        assert!(messages[1]["result"].is_null());
    }

    #[test]
    fn document_sync_publishes_diagnostics_from_service() {
        let root = tempfile::tempdir().expect("temp");
        let file = root.path().join("case.ts");
        std::fs::write(&file, "const value: number = \"wrong\";\n").expect("seed");
        let uri = path_to_uri(&file);
        let mut input = Vec::new();
        input.extend(frame(&initialize(root.path())));
        input.extend(frame(&json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        })));
        input.extend(frame(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "typescript",
                    "version": 1,
                    "text": "const value: number = \"wrong\";\n"
                }
            }
        })));
        input.extend(frame(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": uri, "version": 2 },
                "contentChanges": [{ "text": "const value: number = 1;\n" }]
            }
        })));
        input.extend(frame(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didClose",
            "params": { "textDocument": { "uri": uri } }
        })));
        input.extend(frame(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "shutdown",
            "params": null
        })));
        input.extend(frame(&json!({ "jsonrpc": "2.0", "method": "exit" })));
        let mut output = Vec::new();
        run(Cursor::new(input), &mut output, root.path()).expect("run");
        let messages = read_all(&output);
        let publishes: Vec<&Value> = messages
            .iter()
            .filter(|message| {
                message.get("method").and_then(Value::as_str)
                    == Some("textDocument/publishDiagnostics")
            })
            .collect();
        assert!(
            publishes[0]["params"]["diagnostics"]
                .as_array()
                .expect("diagnostics")
                .iter()
                .any(|diagnostic| diagnostic["code"] == "BAMTS-C004"),
            "{publishes:?}"
        );
        assert!(
            publishes[2]["params"]["diagnostics"]
                .as_array()
                .expect("closed")
                .is_empty()
        );
    }

    #[test]
    fn queries_route_through_service_state() {
        let root = tempfile::tempdir().expect("temp");
        let file = root.path().join("query.ts");
        let source = "const answer = 1;\nconst copy = answer;\n";
        std::fs::write(&file, source).expect("seed");
        let uri = path_to_uri(&file);
        let mut input = Vec::new();
        input.extend(frame(&initialize(root.path())));
        input.extend(frame(&json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        })));
        input.extend(frame(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "typescript",
                    "version": 1,
                    "text": source
                }
            }
        })));
        input.extend(frame(&json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "textDocument/definition",
            "params": {
                "textDocument": { "uri": uri },
                "position": { "line": 1, "character": 14 }
            }
        })));
        input.extend(frame(&json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "textDocument/completion",
            "params": {
                "textDocument": { "uri": uri },
                "position": { "line": 1, "character": 14 }
            }
        })));
        input.extend(frame(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "shutdown",
            "params": null
        })));
        input.extend(frame(&json!({ "jsonrpc": "2.0", "method": "exit" })));
        let mut output = Vec::new();
        run(Cursor::new(input), &mut output, root.path()).expect("run");
        let messages = read_all(&output);
        let definition = messages
            .iter()
            .find(|message| message.get("id") == Some(&json!(10)))
            .expect("definition");
        assert_eq!(definition["result"]["uri"], uri);
        let completion = messages
            .iter()
            .find(|message| message.get("id") == Some(&json!(11)))
            .expect("completion");
        assert!(
            completion["result"]
                .as_array()
                .expect("items")
                .iter()
                .any(|item| item["label"] == "answer"),
            "{completion:?}"
        );
    }

    #[test]
    fn lsp_position_counts_utf16_units() {
        let source = "a😀b";
        assert_eq!(lsp_position(source, 0, 0), Utf16Pos::new(0));
        assert_eq!(lsp_position(source, 0, 1), Utf16Pos::new(1));
        assert_eq!(lsp_position(source, 0, 3), Utf16Pos::new(3));
    }
}
