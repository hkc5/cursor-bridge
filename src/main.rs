// cursor-bridge — Claude Code on Cursor's backend
// One binary. Claude Code on Cursor's backend. Zero config.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn e(msg: &str) {
    eprintln!("ccp: {msg}");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let claude_args: Vec<&str> = args.iter().skip(1).map(|s| s.as_str()).collect();
    let is_pipe = claude_args.iter().any(|a| *a == "-p" || *a == "--print");

    if claude_args.iter().any(|a| *a == "--help" || *a == "-h") {
        println!("cursor-bridge — Claude Code on Cursor's backend");
        println!("Usage: cursor-bridge [claude-args...]");
        println!();
        println!("One binary. Zero config. Just like `claude`, but runs on your Cursor subscription.");
        println!();
        println!("  cursor-bridge              interactive");
        println!("  cursor-bridge \"prompt\"     one-shot");
        println!("  cursor-bridge -p \"prompt\"  pipe mode");
        return;
    }

    // 1. Read Cursor token
    let token = get_cursor_token();
    if token.is_empty() {
        e("No Cursor token found. Run `agent login` first.");
        std::process::exit(1);
    }
    e(&format!("token found: {}..{}", &token[..12], &token[token.len()-4..]));

    // 2. Start proxy on random port
    let proxy = match Proxy::start(&token) {
        Ok(p) => p,
        Err(err) => {
            e(&format!("Failed to start proxy: {err}"));
            std::process::exit(1);
        }
    };

    let port = proxy.port();

    // 3. Spawn claude with env overrides
    let mut cmd = Command::new("claude");
    cmd.env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"));
    cmd.env("ANTHROPIC_AUTH_TOKEN", "sk-any");
    cmd.env("ANTHROPIC_API_KEY", "");
    cmd.env("ANTHROPIC_MODEL", "cursor-auto");
    cmd.env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");

    for arg in &claude_args {
        cmd.arg(arg);
    }

    if is_pipe || !claude_args.is_empty() {
        if !claude_args.contains(&"--dangerously-skip-permissions") {
            cmd.arg("--dangerously-skip-permissions");
        }
    }

    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            e(&format!("Failed to spawn claude: {err}"));
            e("Make sure Claude Code is installed: https://claude.ai/code");
            std::process::exit(1);
        }
    };

    e(&format!("claude spawned (pid {}), waiting...", child.id()));

    let status = child.wait();
    drop(proxy);

    match status {
        Ok(s) => std::process::exit(s.code().unwrap_or(0)),
        Err(err) => {
            e(&format!("claude process error: {err}"));
            std::process::exit(1);
        }
    }
}

// ─── Token ─────────────────────────────────────────────────────────────

fn get_cursor_token() -> String {
    if let Ok(t) = std::env::var("CURSOR_TOKEN") {
        if !t.is_empty() {
            return t;
        }
    }
    if let Ok(t) = std::env::var("CURSOR_API_KEY") {
        if !t.is_empty() {
            return t;
        }
    }
    let output = Command::new("security")
        .args(["find-generic-password", "-s", "cursor-access-token", "-w"])
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}

// ─── Proxy ─────────────────────────────────────────────────────────────

struct Proxy {
    port: u16,
    _shutdown: Arc<AtomicBool>,
}

impl Proxy {
    fn start(token: &str) -> std::io::Result<Self> {
        let token = token.to_string();
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();

        std::thread::Builder::new()
            .name("ccp-proxy".into())
            .spawn(move || {
                let _ = listener.set_nonblocking(true);
                loop {
                    if sd.load(Ordering::Relaxed) {
                        break;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let t = token.clone();
                            std::thread::Builder::new()
                                .name("ccp-conn".into())
                                .spawn(move || handle_connection(stream, &t))
                                .ok();
                        }
                        Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        Err(_) => break,
                    }
                }
            })
            .ok();

        e(&format!("proxy on 127.0.0.1:{port}"));
        Ok(Self { port, _shutdown: shutdown })
    }

    fn port(&self) -> u16 {
        self.port
    }
}

// ─── HTTP ──────────────────────────────────────────────────────────────

fn handle_connection(stream: TcpStream, token: &str) {
    let mut reader = BufReader::new(&stream);
    let mut req_line = String::new();
    if reader.read_line(&mut req_line).ok().map_or(true, |n| n == 0) || req_line.trim().is_empty() {
        return;
    }

    let parts: Vec<&str> = req_line.trim().splitn(3, ' ').collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0];
    let path = parts[1];

    let mut content_length: usize = 0;
    let mut is_chunked = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok().map_or(true, |n| n == 0) || line.trim().is_empty() {
            break;
        }
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
            if reader.read_line(&mut line).ok().map_or(true, |n| n == 0) {
                break;
            }
            let sz = usize::from_str_radix(line.trim(), 16).unwrap_or(0);
            if sz == 0 {
                break;
            }
            let mut chunk = vec![0u8; sz];
            let _ = reader.read_exact(&mut chunk);
            body.extend_from_slice(&chunk);
            let _ = reader.read_line(&mut String::new());
        }
    }

    e(&format!("  {} {} ({} bytes)", method, path, body.len()));

    match (method, path) {
        ("HEAD", "/api/hello") | ("GET", "/api/hello") => respond_hello(stream, method == "HEAD"),
        ("GET", "/v1/models") | ("GET", "/models") => respond_models(stream),
        ("POST", p) if p.starts_with("/v1/messages") || p.starts_with("/messages") => {
            handle_messages(stream, &body, token);
        }
        ("OPTIONS", _) => respond_cors(stream),
        _ => respond_404(stream),
    }
}

fn respond_cors(mut stream: TcpStream) {
    let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: *\r\nContent-Length: 0\r\n\r\n");
}

fn respond_404(mut stream: TcpStream) {
    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\n\r\n{}");
}

fn respond_hello(mut stream: TcpStream, head: bool) {
    let body = r#"{"status":"ok"}"#;
    let hdr = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nx-request-id: ccp-{}\r\n\r\n{}",
        body.len(),
        std::process::id(),
        if head { "" } else { body }
    );
    let _ = stream.write_all(hdr.as_bytes());
}

fn respond_models(mut stream: TcpStream) {
    let models = r#"{"data":[
        {"type":"model","id":"cursor-auto","display_name":"Cursor Auto"},
        {"type":"model","id":"cursor-smart","display_name":"Cursor Smart"},
        {"type":"model","id":"default","display_name":"Default"}
    ]}"#;
    let hdr = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        models.len(),
        models
    );
    let _ = stream.write_all(hdr.as_bytes());
}

// ─── Messages ──────────────────────────────────────────────────────────

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
    let sys_text = extract_system_text(system);
    if !sys_text.is_empty() {
        prompt.push_str("[SYSTEM]\n");
        prompt.push_str(&sys_text);
        prompt.push_str("\n[/SYSTEM]\n\n");
    }
    for msg in messages {
        let role = match msg.role.as_str() {
            "assistant" => "Assistant",
            _ => "User",
        };
        prompt.push('[');
        prompt.push_str(role);
        prompt.push_str("]\n");
        match &msg.content {
            serde_json::Value::String(s) => prompt.push_str(s),
            serde_json::Value::Array(arr) => {
                for block in arr {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                        prompt.push_str(text);
                        prompt.push('\n');
                    }
                }
            }
            _ => {}
        }
        prompt.push_str("\n[/");
        prompt.push_str(role);
        prompt.push_str("]\n\n");
    }
    prompt.push_str("[Assistant]\n");
    prompt
}

fn handle_messages(stream: TcpStream, body: &[u8], _token: &str) {
    let req: MessagesRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            let err = format!("{{\"error\":\"{}\"}}", e.to_string().replace('"', "'"));
            let _resp = format!("HTTP/1.1 400\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", err.len(), err);
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return;
        }
    };

    let stream_mode = req.stream.unwrap_or(true);
    if stream_mode {
        handle_streaming(stream, &req);
    } else {
        handle_blocking(stream, &req);
    }
}

fn run_agent(prompt: &str) -> std::io::Result<std::process::Child> {
    let agent_path = std::env::var("AGENT_PATH").unwrap_or_else(|_| "/Users/hakancan/.local/bin/agent".into());
    e(&format!("spawning: {agent_path}"));
    // Pass prompt as last arg so agent reads it from argv, not stdin
    Command::new(agent_path)
        .args(["--mode", "ask", "--print", "--output-format", "stream-json", "--model", "auto", "--trust"])
        .arg(prompt)  // prompt as positional arg
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

fn handle_blocking(mut stream: TcpStream, req: &MessagesRequest) {
    let prompt = build_prompt(req.messages.as_deref().unwrap_or_default(), &req.system);
    let model = req.model.as_deref().unwrap_or("cursor-auto");

    let mut agent = match run_agent(&prompt) {
        Ok(a) => a,
        Err(e) => {
            let err = format!("{{\"error\":\"agent: {e}\"}}");
            let resp = format!("HTTP/1.1 500\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", err.len(), err);
            let _ = stream.write_all(resp.as_bytes());
            return;
        }
    };

    if let Some(stdin) = agent.stdin.as_mut() {
        let _ = stdin.write_all(prompt.as_bytes());
        let _ = stdin.flush();
    }

    let stdout = agent.stdout.take().unwrap();
    let reader = BufReader::new(stdout);
    let mut text = String::new();
    let mut usage = serde_json::json!({});

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            _ => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) {
            if event["type"] == "assistant" {
                if let Some(arr) = event["message"]["content"].as_array() {
                    for block in arr {
                        if let Some(t) = block["text"].as_str() {
                            text.push_str(t);
                        }
                    }
                }
            }
            if event["type"] == "result" {
                usage = event["usage"].clone();
            }
        }
    }

    let _ = agent.wait();

    let resp = serde_json::json!({
        "id": format!("msg_{}", std::process::id()),
        "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": text}],
        "model": model, "stop_reason": "end_turn",
        "usage": {
            "input_tokens": usage["inputTokens"].as_u64().unwrap_or(0),
            "output_tokens": usage["outputTokens"].as_u64().unwrap_or(0),
        }
    });

    let body = serde_json::to_string(&resp).unwrap_or_default();
    let hdr = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = stream.write_all(hdr.as_bytes());
}

fn handle_streaming(mut stream: TcpStream, req: &MessagesRequest) {
    let prompt = build_prompt(req.messages.as_deref().unwrap_or_default(), &req.system);
    let model = req.model.as_deref().unwrap_or("cursor-auto");

    let mut agent = match run_agent(&prompt) {
        Ok(a) => a,
        Err(e) => {
            let err = format!("{{\"error\":\"agent: {e}\"}}");
            let resp = format!("HTTP/1.1 500\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", err.len(), err);
            let _ = stream.write_all(resp.as_bytes());
            return;
        }
    };

    e(&format!("prompt: {} bytes", prompt.len()));

    // Send SSE response headers
    let hdr = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\n";
    if stream.write_all(hdr.as_bytes()).is_err() {
        let _ = agent.wait();
        return;
    }
    let _ = stream.flush();
    e("SSE headers sent");
    let _ = stream.flush();

    let msg_id = format!("msg_{}", std::process::id());

    // message_start
    let start = serde_json::json!({
        "type": "message_start",
        "message": {
            "id": msg_id, "type": "message", "role": "assistant",
            "content": [], "model": model,
            "stop_reason": null,
            "usage": {"input_tokens": 0, "output_tokens": 0}
        }
    });
    if write_sse(&mut stream, &start).is_err() {
        let _ = agent.wait();
        return;
    }

    let stdout = agent.stdout.take().unwrap();
    let reader = BufReader::new(stdout);
    let mut handled = false;
    let mut started = false;
    let mut line_count = 0;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            _ => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        line_count += 1;
        if let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) {
            match event["type"].as_str() {
                Some("assistant") => {
                    if let Some(arr) = event["message"]["content"].as_array() {
                        for block in arr {
                            if let Some(text) = block["text"].as_str() {
                                if !started {
                                    started = true;
                                    let cb = serde_json::json!({
                                        "type": "content_block_start", "index": 0,
                                        "content_block": {"type": "text", "text": text}
                                    });
                                    if write_sse(&mut stream, &cb).is_err() {
                                        return;
                                    }
                                }
                                let delta = serde_json::json!({
                                    "type": "content_block_delta", "index": 0,
                                    "delta": {"type": "text_delta", "text": text}
                                });
                                if write_sse(&mut stream, &delta).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
                Some("result") => {
                    // content_block_stop
                    if started {
                        let _ = write_sse(&mut stream, &serde_json::json!({"type": "content_block_stop", "index": 0}));
                    }
                    handled = true;
                    let usage = &event["usage"];
                    let done = serde_json::json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": "end_turn"},
                        "usage": {
                            "input_tokens": usage["inputTokens"].as_u64().unwrap_or(0),
                            "output_tokens": usage["outputTokens"].as_u64().unwrap_or(0),
                        }
                    });
                    let _ = write_sse(&mut stream, &done);
                    let _ = write_sse(&mut stream, &serde_json::json!({"type": "message_stop"}));
                    let _ = stream.write_all(b"data: [DONE]\n\n");
                    let _ = stream.flush();
                }
                _ => {}
            }
        }
    }

    e(&format!("agent output: {line_count} lines, started={started}, handled={handled}"));

    // Fallback if result event wasn't received (e.g., agent error)
    if !handled {
        e("result not handled, sending fallback message_stop");
        if started {
            let _ = write_sse(&mut stream, &serde_json::json!({"type": "content_block_stop", "index": 0}));
        }
        let _ = write_sse(&mut stream, &serde_json::json!({
            "type": "message_delta", "delta": {"stop_reason": "end_turn"},
            "usage": {"input_tokens": 0, "output_tokens": 0}
        }));
        let _ = write_sse(&mut stream, &serde_json::json!({"type": "message_stop"}));
        let _ = stream.write_all(b"data: [DONE]\n\n");
        let _ = stream.flush();
    }

    let _ = agent.wait();
}

fn write_sse(stream: &mut TcpStream, data: &serde_json::Value) -> std::io::Result<()> {
    let json = serde_json::to_string(data)?;
    let event_type = data["type"].as_str().unwrap_or("");
    stream.write_all(b"event: ")?;
    stream.write_all(event_type.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.write_all(b"data: ")?;
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n\n")?;
    stream.flush()
}
