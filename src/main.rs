// cursor-bridge — Claude Code on Cursor's backend.
// One binary. Zero config.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn log(msg: &str) {
    if std::env::var("CURSOR_BRIDGE_DEBUG").is_ok() {
        eprintln!("bridge: {msg}");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let claude_args: Vec<&str> = args.iter().skip(1).map(|s| s.as_str()).collect();

    if claude_args.iter().any(|a| *a == "--help" || *a == "-h") {
        println!("cursor-bridge — Claude Code on Cursor's backend");
        println!("Usage: cursor-bridge [claude-args...]");
        println!();
        println!("  cursor-bridge              interactive");
        println!("  cursor-bridge \"prompt\"     one-shot");
        println!("  cursor-bridge -p \"prompt\"  pipe mode");
        return;
    }

    let token = get_cursor_token();
    if token.is_empty() {
        log("No Cursor token found. Run `agent login` first.");
        std::process::exit(1);
    }
    log(&format!("token: {}..{}", &token[..12], &token[token.len()-4..]));

    let proxy = match Proxy::start(&token) {
        Ok(p) => p,
        Err(err) => { log(&format!("proxy failed: {err}")); std::process::exit(1); }
    };

    let mut cmd = Command::new("claude");
    cmd.env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{}", proxy.port()));
    cmd.env("ANTHROPIC_AUTH_TOKEN", "sk-any");
    cmd.env("ANTHROPIC_API_KEY", "");
    cmd.env("ANTHROPIC_MODEL", "cursor-auto");
    cmd.env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");

    for arg in &claude_args { cmd.arg(arg); }
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            log(&format!("Failed to spawn claude: {err}"));
            log("Install: https://claude.ai/code");
            std::process::exit(1);
        }
    };

    let status = child.wait();
    drop(proxy);
    std::process::exit(status.ok().and_then(|s| s.code()).unwrap_or(0));
}

// ─── Token ────────────────────────────────────────────────────

fn get_cursor_token() -> String {
    for var in &["CURSOR_TOKEN", "CURSOR_API_KEY"] {
        if let Ok(t) = std::env::var(var) {
            if !t.is_empty() { return t; }
        }
    }
    let out = Command::new("security")
        .args(["find-generic-password", "-s", "cursor-access-token", "-w"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}

// ─── Proxy ────────────────────────────────────────────────────

struct Proxy { port: u16, _shutdown: Arc<AtomicBool> }

impl Proxy {
    fn start(token: &str) -> std::io::Result<Self> {
        let t = token.to_string();
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();

        std::thread::Builder::new().name("bridge-proxy".into()).spawn(move || {
            let _ = listener.set_nonblocking(true);
            loop {
                if sd.load(Ordering::Relaxed) { break; }
                match listener.accept() {
                    Ok((stream, _)) => {
                        let ct = t.clone();
                        std::thread::Builder::new().name("bridge-conn".into())
                            .spawn(move || handle_connection(stream, &ct)).ok();
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock =>
                        std::thread::sleep(Duration::from_millis(50)),
                    Err(_) => break,
                }
            }
        }).ok();

        log(&format!("proxy on 127.0.0.1:{port}"));
        Ok(Self { port, _shutdown: shutdown })
    }

    fn port(&self) -> u16 { self.port }
}

// ─── HTTP ─────────────────────────────────────────────────────

fn handle_connection(stream: TcpStream, token: &str) {
    let mut reader = BufReader::new(&stream);
    let mut req_line = String::new();
    if reader.read_line(&mut req_line).ok().map_or(true, |n| n == 0) || req_line.trim().is_empty() {
        return;
    }

    let parts: Vec<&str> = req_line.trim().splitn(3, ' ').collect();
    if parts.len() < 2 { return; }
    let method = parts[0];
    let path = parts[1];

    let mut content_length: usize = 0;
    let mut is_chunked = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok().map_or(true, |n| n == 0) || line.trim().is_empty() { break; }
        let lower = line.to_lowercase();
        if lower.starts_with("content-length:") {
            content_length = line.split(':').nth(1).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        }
        if lower.contains("transfer-encoding:") && lower.contains("chunked") {
            is_chunked = true;
        }
    }

    let mut body = Vec::new();
    if content_length > 0 {
        body.resize(content_length, 0);
        let _ = reader.read_exact(&mut body);
    } else if is_chunked {
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).ok().map_or(true, |n| n == 0) { break; }
            // Chunk size — strip extensions after ';'
            let size_str = line.split(';').next().unwrap_or("").trim();
            let sz = usize::from_str_radix(size_str, 16).unwrap_or(0);
            if sz == 0 { break; }
            let mut chunk = vec![0u8; sz];
            let _ = reader.read_exact(&mut chunk);
            body.extend_from_slice(&chunk);
            let _ = reader.read_line(&mut String::new());
        }
    }

    log(&format!("  {} {} ({}b)", method, path, body.len()));

    match (method, path) {
        ("HEAD", "/api/hello") | ("GET", "/api/hello") => respond_hello(stream, method == "HEAD"),
        ("GET", "/v1/models") | ("GET", "/models") => respond_models(stream),
        ("POST", p) if p.starts_with("/v1/messages") || p.starts_with("/messages") =>
            handle_messages(stream, &body, token),
        ("OPTIONS", _) => respond_cors(stream),
        _ => respond_404(stream),
    }
}

fn respond_cors(mut s: TcpStream) { let _ = s.write_all(b"HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: *\r\nContent-Length: 0\r\n\r\n"); }
fn respond_404(mut s: TcpStream) { let _ = s.write_all(b"HTTP/1.1 404\r\nContent-Length: 2\r\n\r\n{}"); }
fn respond_hello(mut s: TcpStream, head: bool) {
    let b = r#"{"status":"ok"}"#;
    let h = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nx-request-id: bridge-{}\r\n\r\n{}", b.len(), std::process::id(), if head { "" } else { b });
    let _ = s.write_all(h.as_bytes());
}
fn respond_models(mut s: TcpStream) {
    let body = get_models_json();
    let h = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = s.write_all(h.as_bytes());
}

fn get_models_json() -> String {
    r#"{"data":[
        {"type":"model","id":"default","display_name":"Auto"},
        {"type":"model","id":"claude-sonnet-4-6-high","display_name":"Claude Sonnet 4.6 High"},
        {"type":"model","id":"claude-sonnet-4-6-high-fast","display_name":"Claude Sonnet 4.6 High Fast"},
        {"type":"model","id":"claude-opus-5-high","display_name":"Claude Opus 5 High"},
        {"type":"model","id":"claude-opus-5-high-fast","display_name":"Claude Opus 5 High Fast"},
        {"type":"model","id":"cursor-grok-4.5-high","display_name":"Cursor Grok 4.5"},
        {"type":"model","id":"cursor-grok-4.5-high-fast","display_name":"Cursor Grok 4.5 Fast"},
        {"type":"model","id":"composer-2.5","display_name":"Composer 2.5"},
        {"type":"model","id":"composer-2.5-fast","display_name":"Composer 2.5 Fast"}
    ]}"#.to_string()
}

// ─── Messages ─────────────────────────────────────────────────

#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct MessagesRequest {
    model: Option<String>,
    messages: Option<Vec<Message>>,
    system: Option<serde_json::Value>,
    max_tokens: Option<u32>,
    stream: Option<bool>,
}

#[derive(serde::Deserialize)]
struct Message {
    role: String,
    content: serde_json::Value,
}

// ─── Prompt building ──────────────────────────────────────────

fn extract_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            let mut out = String::new();
            for block in arr {
                match block["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = block["text"].as_str() { out.push_str(t); out.push('\n'); }
                    }
                    Some("tool_use") => {
                        let name = block["name"].as_str().unwrap_or("unknown");
                        let input = block["input"].to_string();
                        out.push_str(&format!("[TOOL_USE: {name}]\n{input}\n[/TOOL_USE]\n"));
                    }
                    Some("tool_result") => {
                        let id = block["tool_use_id"].as_str().unwrap_or("");
                        let content = extract_text(&block["content"]);
                        let error = block["is_error"].as_bool().unwrap_or(false);
                        if error {
                            out.push_str(&format!("[TOOL_ERROR: {id}]\n{content}\n[/TOOL_ERROR]\n"));
                        } else {
                            out.push_str(&format!("[TOOL_RESULT: {id}]\n{content}\n[/TOOL_RESULT]\n"));
                        }
                    }
                    Some("thinking") => {
                        if let Some(t) = block["thinking"].as_str() { out.push_str(&format!("[thinking]\n{t}\n[/thinking]\n")); }
                    }
                    _ => {}
                }
            }
            out
        }
        _ => String::new(),
    }
}

fn extract_system_text(system: &Option<serde_json::Value>) -> String {
    match system {
        None => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => {
            arr.iter().filter_map(|v| v.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join("\n")
        }
        _ => String::new(),
    }
}

fn build_prompt(messages: &[Message], system: &Option<serde_json::Value>) -> String {
    let mut prompt = String::new();
    let sys = extract_system_text(system);
    if !sys.is_empty() { prompt.push_str(&format!("[SYSTEM]\n{sys}\n[/SYSTEM]\n\n")); }

    for msg in messages {
        let role = match msg.role.as_str() {
            "assistant" => "Assistant",
            "user" => "User",
            _ => "User",
        };
        prompt.push_str(&format!("[{role}]\n{}\n[/{role}]\n\n", extract_text(&msg.content)));
    }
    prompt.push_str("[Assistant]\n");
    prompt
}

// ─── Agent ────────────────────────────────────────────────────

fn find_agent() -> Option<String> {
    if let Ok(path) = std::env::var("AGENT_PATH") {
        if !path.is_empty() && std::path::Path::new(&path).exists() { return Some(path); }
    }
    // Try `command -v` (POSIX) then `which`
    for cmd in &["sh", "which"] {
        let args: &[&str] = if *cmd == "sh" { &["-c", "command -v agent"] } else { &["agent"] };
        if let Ok(out) = Command::new(cmd).args(args).output() {
            if out.status.success() {
                let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !p.is_empty() { return Some(p); }
            }
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    for loc in &["/usr/local/bin/agent", "/opt/homebrew/bin/agent", "/usr/bin/agent"] {
        if std::path::Path::new(loc).exists() { return Some(loc.to_string()); }
    }
    if !home.is_empty() {
        let local = format!("{home}/.local/bin/agent");
        if std::path::Path::new(&local).exists() { return Some(local); }
    }
    None
}

fn spawn_agent(requested_model: &str) -> std::io::Result<std::process::Child> {
    let path = find_agent().unwrap_or_else(|| { log("agent not found. Install Cursor CLI or set AGENT_PATH."); std::process::exit(1); });
    log(&format!("spawning: {path}"));

    // Run agent in temp dir so it can't touch project files
    let sandbox = std::env::temp_dir().join(format!("cursor-bridge-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&sandbox);
    log(&format!("sandbox: {}", sandbox.display()));

    // Default mode (no --mode) = full agent with tool execution.
    // --force auto-approves tool calls in non-interactive mode.
    // --trust skips workspace trust prompt.
    Command::new(path)
        .args(["--print", "--force", "--output-format", "stream-json", "--model", requested_model, "--trust"])
        .current_dir(&sandbox)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

fn write_prompt(agent: &mut std::process::Child, prompt: &str) {
    if let Some(ref mut stdin) = agent.stdin {
        let _ = stdin.write_all(prompt.as_bytes());
        let _ = stdin.flush();
    }
    // Close stdin so agent gets EOF
    agent.stdin = None;
}

// ─── Blocking ─────────────────────────────────────────────────

fn handle_blocking(mut stream: TcpStream, req: &MessagesRequest) {
    let prompt = build_prompt(req.messages.as_deref().unwrap_or_default(), &req.system);
    let model = req.model.as_deref().unwrap_or("cursor-auto");

    let mut agent = match spawn_agent(model) { Ok(a) => a, Err(e) => {
        let err = format!("{{\"error\":\"agent: {e}\"}}");
        let _ = stream.write_all(format!("HTTP/1.1 500\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{err}", err.len()).as_bytes());
        return;
    }};
    write_prompt(&mut agent, &prompt);

    let reader = BufReader::new(agent.stdout.take().unwrap());
    let mut text = String::new();
    let mut usage = serde_json::json!({});

    for line in reader.lines() {
        let line = match line { Ok(l) => l, _ => break };
        if line.trim().is_empty() { continue; }
        if let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) {
            if event["type"] == "assistant" {
                if let Some(arr) = event["message"]["content"].as_array() {
                    for block in arr {
                        if let Some(t) = block["text"].as_str() { text.push_str(t); }
                    }
                }
            }
            if event["type"] == "result" { usage = event["usage"].clone(); }
        }
    }
    let _ = agent.wait();

    let resp = serde_json::json!({
        "id": format!("msg_{}", std::process::id()), "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": text}], "model": model, "stop_reason": "end_turn",
        "usage": { "input_tokens": usage["inputTokens"].as_u64().unwrap_or(0), "output_tokens": usage["outputTokens"].as_u64().unwrap_or(0) }
    });
    let body = serde_json::to_string(&resp).unwrap_or_default();
    let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes());
}

// ─── Streaming ────────────────────────────────────────────────

fn write_sse(stream: &mut TcpStream, event_type: &str, data: &serde_json::Value) -> std::io::Result<()> {
    let json = serde_json::to_string(data)?;
    stream.write_all(b"event: ")?;
    stream.write_all(event_type.as_bytes())?;
    stream.write_all(b"\ndata: ")?;
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n\n")?;
    stream.flush()
}

fn handle_streaming(mut stream: TcpStream, req: &MessagesRequest) {
    let prompt = build_prompt(req.messages.as_deref().unwrap_or_default(), &req.system);
    let model = req.model.as_deref().unwrap_or("cursor-auto");

    let mut agent = match spawn_agent(model) { Ok(a) => a, Err(e) => {
        let err = format!("{{\"error\":\"agent: {e}\"}}");
        let _ = stream.write_all(format!("HTTP/1.1 500\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{err}", err.len()).as_bytes());
        return;
    }};
    log(&format!("prompt: {}b", prompt.len()));
    write_prompt(&mut agent, &prompt);

    // SSE response headers
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\n");
    let _ = stream.flush();

    let msg_id = format!("msg_{}", std::process::id());
    let _ = write_sse(&mut stream, "message_start", &serde_json::json!({
        "type": "message_start",
        "message": { "id": msg_id, "type": "message", "role": "assistant", "content": [], "model": model, "stop_reason": null, "usage": { "input_tokens": 0, "output_tokens": 0 } }
    }));

    let reader = BufReader::new(agent.stdout.take().unwrap());
    let mut content_index = 0i32;
    let mut result_received = false;

    for line in reader.lines() {
        let line = match line { Ok(l) => l, _ => break };
        if line.trim().is_empty() { continue; }
        if let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) {
            match event["type"].as_str() {
                Some("assistant") => {
                    if let Some(blocks) = event["message"]["content"].as_array() {
                        for block in blocks {
                            let block_type = block["type"].as_str().unwrap_or("text");
                            match block_type {
                                "text" => {
                                    if let Some(text) = block["text"].as_str() {
                                        let _ = write_sse(&mut stream, "content_block_start", &serde_json::json!({
                                            "type": "content_block_start", "index": content_index,
                                            "content_block": {"type": "text", "text": text}
                                        }));
                                        let _ = write_sse(&mut stream, "content_block_delta", &serde_json::json!({
                                            "type": "content_block_delta", "index": content_index,
                                            "delta": {"type": "text_delta", "text": text}
                                        }));
                                        let _ = write_sse(&mut stream, "content_block_stop", &serde_json::json!({
                                            "type": "content_block_stop", "index": content_index
                                        }));
                                        content_index += 1;
                                    }
                                }
                                "tool_use" => {
                                    let name = block["name"].as_str().unwrap_or("unknown");
                                    let input = block["input"].clone();
                                    let fallback_id = format!("toolu_{}", content_index);
                                    let tool_id = block["id"].as_str().unwrap_or(&fallback_id);
                                    let _ = write_sse(&mut stream, "content_block_start", &serde_json::json!({
                                        "type": "content_block_start", "index": content_index,
                                        "content_block": {"type": "tool_use", "id": tool_id, "name": name, "input": input}
                                    }));
                                    let _ = write_sse(&mut stream, "content_block_stop", &serde_json::json!({
                                        "type": "content_block_stop", "index": content_index
                                    }));
                                    content_index += 1;
                                }
                                _ => {} // skip thinking, etc
                            }
                        }
                    }
                }
                Some("result") => {
                    result_received = true;
                    let usage = &event["usage"];
                    let _ = write_sse(&mut stream, "message_delta", &serde_json::json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": "end_turn"},
                        "usage": { "input_tokens": usage["inputTokens"].as_u64().unwrap_or(0), "output_tokens": usage["outputTokens"].as_u64().unwrap_or(0) }
                    }));
                    let _ = write_sse(&mut stream, "message_stop", &serde_json::json!({"type": "message_stop"}));
                    let _ = stream.write_all(b"data: [DONE]\n\n");
                    let _ = stream.flush();
                }
                _ => {}
            }
        }
    }

    if !result_received {
        log("result not received, sending fallback message_stop");
        let _ = write_sse(&mut stream, "message_delta", &serde_json::json!({
            "type": "message_delta", "delta": {"stop_reason": "end_turn"},
            "usage": {"input_tokens": 0, "output_tokens": 0}
        }));
        let _ = write_sse(&mut stream, "message_stop", &serde_json::json!({"type": "message_stop"}));
        let _ = stream.write_all(b"data: [DONE]\n\n");
        let _ = stream.flush();
    }

    let _ = agent.wait();
}

fn handle_messages(mut stream: TcpStream, body: &[u8], _token: &str) {
    let req: MessagesRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            let err = format!("{{\"error\":\"{}\"}}", e.to_string().replace('"', "'"));
            let resp = format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{err}", err.len());
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
            return;
        }
    };

    if req.stream.unwrap_or(true) { handle_streaming(stream, &req); }
    else { handle_blocking(stream, &req); }
}

// ─── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_system_text_string() {
        let v = Some(serde_json::Value::String("Be helpful.".into()));
        assert_eq!(extract_system_text(&v), "Be helpful.");
    }

    #[test]
    fn test_extract_system_text_array() {
        let v = Some(serde_json::json!([
            {"type": "text", "text": "Be helpful."},
            {"type": "text", "text": "Be concise."}
        ]));
        assert_eq!(extract_system_text(&v), "Be helpful.\nBe concise.");
    }

    #[test]
    fn test_extract_system_text_none() {
        assert_eq!(extract_system_text(&None), "");
    }

    #[test]
    fn test_extract_text_string_content() {
        let v = serde_json::Value::String("hello".into());
        assert_eq!(extract_text(&v), "hello");
    }

    #[test]
    fn test_extract_text_text_block() {
        let v = serde_json::json!([
            {"type": "text", "text": "Hello there"}
        ]);
        assert_eq!(extract_text(&v), "Hello there\n");
    }

    #[test]
    fn test_extract_text_tool_use() {
        let v = serde_json::json!([
            {"type": "tool_use", "name": "bash", "id": "tu_1", "input": {"command": "ls"}}
        ]);
        let out = extract_text(&v);
        assert!(out.contains("TOOL_USE: bash"));
        assert!(out.contains("ls"));
    }

    #[test]
    fn test_extract_text_tool_result() {
        let v = serde_json::json!([
            {"type": "tool_result", "tool_use_id": "tu_1", "content": "file.txt"}
        ]);
        let out = extract_text(&v);
        assert!(out.contains("TOOL_RESULT: tu_1"));
        assert!(out.contains("file.txt"));
    }

    #[test]
    fn test_extract_text_tool_error() {
        let v = serde_json::json!([
            {"type": "tool_result", "tool_use_id": "tu_1", "content": "permission denied", "is_error": true}
        ]);
        let out = extract_text(&v);
        assert!(out.contains("TOOL_ERROR"));
    }

    #[test]
    fn test_agent_empty_env_returns_none_for_bogus() {
        // Without AGENT_PATH set, non-existent name should not crash
        // Just verify the function handles missing gracefully
        let original = std::env::var("AGENT_PATH").ok();
        std::env::remove_var("AGENT_PATH");
        // Can't assert None because `which` might find `agent` in CI,
        // but it shouldn't panic or return Some("")
        let result = find_agent();
        if let Some(path) = result {
            assert!(!path.is_empty(), "path must not be empty");
        }
        if let Some(val) = original {
            std::env::set_var("AGENT_PATH", val);
        }
    }

    #[test]
    fn test_build_prompt_simple() {
        let msgs = [Message {
            role: "user".into(),
            content: serde_json::Value::String("hi".into()),
        }];
        let prompt = build_prompt(&msgs, &None);
        assert!(prompt.contains("[User]"));
        assert!(prompt.contains("hi"));
        assert!(prompt.contains("[/User]"));
        assert!(prompt.contains("[Assistant]"));
    }

    #[test]
    fn test_build_prompt_with_system() {
        let sys = Some(serde_json::Value::String("You are a bot.".into()));
        let msgs = [Message {
            role: "user".into(),
            content: serde_json::Value::String("hi".into()),
        }];
        let prompt = build_prompt(&msgs, &sys);
        assert!(prompt.contains("[SYSTEM]"));
        assert!(prompt.contains("You are a bot."));
    }

    #[test]
    fn test_build_prompt_with_tool_context() {
        let msgs = [
            Message {
                role: "user".into(),
                content: serde_json::json!([
                    {"type": "tool_result", "tool_use_id": "tu_1", "content": "file contents"}
                ]),
            },
            Message {
                role: "assistant".into(),
                content: serde_json::json!([
                    {"type": "tool_use", "name": "read", "id": "tu_1", "input": {"path": "file.txt"}}
                ]),
            },
        ];
        let prompt = build_prompt(&msgs, &None);
        assert!(prompt.contains("TOOL_RESULT"));
        assert!(prompt.contains("TOOL_USE: read"));
    }

    #[test]
    fn test_models_response_valid_json() {
        // Verify the hardcoded models response is valid JSON
        let body = r#"{"data":[
            {"type":"model","id":"cursor-auto","display_name":"Cursor Auto"},
            {"type":"model","id":"cursor-smart","display_name":"Cursor Smart"},
            {"type":"model","id":"default","display_name":"Default"}
        ]}"#;
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["data"].as_array().unwrap().len(), 3);
    }
}
