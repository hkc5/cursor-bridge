// cursor-bridge — Claude Code on Cursor's backend.
// One binary. Zero config.

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn log(msg: &str) {
    let debug_enabled = std::env::var("CURSOR_BRIDGE_DEBUG")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);
    if debug_enabled {
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

    match find_agent() {
        Ok(Some(_)) => {}
        Ok(None) => {
            log("Cursor agent CLI not found. Install it, add it to PATH, then run `agent login`.");
            std::process::exit(1);
        }
        Err(err) => {
            log(&format!("Invalid Cursor agent configuration: {err}"));
            std::process::exit(1);
        }
    }

    let claude = match find_claude() {
        Ok(Some(command)) => command,
        Ok(None) => {
            log("Claude Code CLI not found. Install it and add `claude` to PATH.");
            std::process::exit(1);
        }
        Err(err) => {
            log(&format!("Invalid Claude Code configuration: {err}"));
            std::process::exit(1);
        }
    };

    let proxy = match Proxy::start() {
        Ok(p) => p,
        Err(err) => {
            log(&format!("proxy failed: {err}"));
            std::process::exit(1);
        }
    };

    let mut cmd = claude.command();
    cmd.env(
        "ANTHROPIC_BASE_URL",
        format!("http://127.0.0.1:{}", proxy.port()),
    );
    cmd.env("ANTHROPIC_AUTH_TOKEN", "sk-any");
    cmd.env("ANTHROPIC_API_KEY", "");
    cmd.env("ANTHROPIC_MODEL", "auto");
    cmd.env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");

    for arg in &claude_args {
        cmd.arg(arg);
    }
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) => {
            log(&format!("Failed to spawn claude: {err}"));
            log("Install Claude Code and ensure `claude` is available in PATH.");
            std::process::exit(1);
        }
    };

    let status = child.wait();
    drop(proxy);
    std::process::exit(status.ok().and_then(|s| s.code()).unwrap_or(1));
}

// ─── Process resolution ─────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
struct ResolvedCommand {
    program: PathBuf,
    prefix_args: Vec<OsString>,
}

impl ResolvedCommand {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.prefix_args);
        command
    }
}

fn is_windows_script(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"))
        .unwrap_or(false)
}

fn command_for_path(path: PathBuf, windows: bool) -> ResolvedCommand {
    if windows && is_windows_script(&path) {
        ResolvedCommand {
            program: PathBuf::from("cmd.exe"),
            prefix_args: vec![
                OsString::from("/d"),
                OsString::from("/c"),
                OsString::from("call"),
                path.into_os_string(),
            ],
        }
    } else {
        ResolvedCommand {
            program: path,
            prefix_args: Vec::new(),
        }
    }
}

fn paths_from_output(output: &std::process::Output) -> Vec<PathBuf> {
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn find_on_path(name: &str) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        Command::new("where.exe")
            .arg(name)
            .output()
            .map(|output| paths_from_output(&output))
            .unwrap_or_default()
    }

    #[cfg(not(windows))]
    {
        let command = format!("command -v {name}");
        if let Ok(output) = Command::new("sh").args(["-c", command.as_str()]).output() {
            let paths = paths_from_output(&output);
            if !paths.is_empty() {
                return paths;
            }
        }
        if let Ok(output) = Command::new("which").arg(name).output() {
            return paths_from_output(&output);
        }

        Vec::new()
    }
}

#[cfg(any(windows, test))]
fn choose_supported_command_path(
    paths: &[PathBuf],
    windows: bool,
) -> Result<Option<PathBuf>, PathBuf> {
    let mut rejected = None;
    for path in paths {
        if is_supported_command_path(path, windows) {
            return Ok(Some(path.clone()));
        }
        rejected = Some(path.clone());
    }
    match rejected {
        Some(path) => Err(path),
        None => Ok(None),
    }
}

#[cfg(windows)]
fn unsupported_discovered_path_error(kind: &str, path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "{kind} command was found at '{}' but cannot be launched safely because its path contains unsupported cmd.exe characters",
            path.display()
        ),
    )
}

fn is_supported_command_path(path: &Path, windows: bool) -> bool {
    if !windows {
        return true;
    }

    match path.extension().and_then(OsStr::to_str) {
        Some(ext) if ext.eq_ignore_ascii_case("exe") => true,
        Some(ext) if ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat") => {
            !path.as_os_str().to_string_lossy().chars().any(|c| {
                matches!(c, '%' | '^' | '&' | '|' | '<' | '>' | '(' | ')' | '!' | '"')
                    || c.is_control()
            })
        }
        _ => false,
    }
}

fn command_from_env(var: &str) -> std::io::Result<Option<ResolvedCommand>> {
    let Some(path) = std::env::var_os(var).map(PathBuf::from) else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    if !is_supported_command_path(&path, cfg!(windows)) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{var} must point to a .exe, .cmd, or .bat file with no cmd.exe metacharacters"
            ),
        ));
    }
    Ok(Some(command_for_path(path, cfg!(windows))))
}

fn find_agent() -> std::io::Result<Option<ResolvedCommand>> {
    if let Some(command) = command_from_env("AGENT_PATH")? {
        return Ok(Some(command));
    }

    #[cfg(windows)]
    {
        let mut rejected_path = None;
        for name in ["agent.exe", "agent.cmd", "agent.bat"] {
            let paths = find_on_path(name);
            match choose_supported_command_path(&paths, true) {
                Ok(Some(path)) => return Ok(Some(command_for_path(path, true))),
                Err(path) => {
                    rejected_path.get_or_insert(path);
                }
                Ok(None) => {}
            }
        }

        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            let path = PathBuf::from(local_app_data)
                .join("cursor-agent")
                .join("agent.cmd");
            if path.is_file() {
                if is_supported_command_path(&path, true) {
                    return Ok(Some(command_for_path(path, true)));
                }
                rejected_path.get_or_insert(path);
            }
        }

        if let Some(path) = rejected_path {
            return Err(unsupported_discovered_path_error("Cursor agent", &path));
        }
    }

    #[cfg(not(windows))]
    {
        if let Some(path) = find_on_path("agent").into_iter().next() {
            return Ok(Some(command_for_path(path, false)));
        }

        let home = std::env::var("HOME").unwrap_or_default();
        for loc in [
            "/usr/local/bin/agent",
            "/opt/homebrew/bin/agent",
            "/usr/bin/agent",
        ] {
            if Path::new(loc).exists() {
                return Ok(Some(command_for_path(PathBuf::from(loc), false)));
            }
        }
        if !home.is_empty() {
            let local = PathBuf::from(home).join(".local").join("bin").join("agent");
            if local.exists() {
                return Ok(Some(command_for_path(local, false)));
            }
        }
    }

    Ok(None)
}

#[cfg(windows)]
fn claude_windows_candidates(user_profile: &Path) -> Vec<PathBuf> {
    vec![user_profile.join(".local").join("bin").join("claude.exe")]
}

fn find_claude() -> std::io::Result<Option<ResolvedCommand>> {
    if let Some(command) = command_from_env("CLAUDE_PATH")? {
        return Ok(Some(command));
    }

    #[cfg(windows)]
    {
        let mut rejected_path = None;
        for name in ["claude.exe", "claude.cmd", "claude.bat"] {
            let paths = find_on_path(name);
            match choose_supported_command_path(&paths, true) {
                Ok(Some(path)) => return Ok(Some(command_for_path(path, true))),
                Err(path) => {
                    rejected_path.get_or_insert(path);
                }
                Ok(None) => {}
            }
        }

        if let Some(user_profile) = std::env::var_os("USERPROFILE") {
            for path in claude_windows_candidates(Path::new(&user_profile)) {
                if path.is_file() {
                    if is_supported_command_path(&path, true) {
                        return Ok(Some(command_for_path(path, true)));
                    }
                    rejected_path.get_or_insert(path);
                }
            }
        }

        if let Some(path) = rejected_path {
            return Err(unsupported_discovered_path_error("Claude Code", &path));
        }
    }

    #[cfg(not(windows))]
    {
        if let Some(path) = find_on_path("claude").into_iter().next() {
            return Ok(Some(command_for_path(path, false)));
        }
    }

    Ok(None)
}

fn normalize_model(requested_model: &str) -> std::io::Result<String> {
    let model = match requested_model {
        "" | "default" | "cursor-auto" => "auto",
        other => other,
    };
    if model
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        Ok(model.to_string())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid model identifier",
        ))
    }
}

// ─── Proxy ────────────────────────────────────────────────────

struct Proxy {
    port: u16,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Proxy {
    fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();

        let thread = std::thread::Builder::new()
            .name("bridge-proxy".into())
            .spawn(move || loop {
                if sd.load(Ordering::Acquire) {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        std::thread::Builder::new()
                            .name("bridge-conn".into())
                            .spawn(move || handle_connection(stream))
                            .ok();
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(50))
                    }
                    Err(_) => break,
                }
            })?;

        log(&format!("proxy on 127.0.0.1:{port}"));
        Ok(Self {
            port,
            shutdown,
            thread: Some(thread),
        })
    }

    fn port(&self) -> u16 {
        self.port
    }
}

// ─── HTTP ─────────────────────────────────────────────────────
fn handle_connection(stream: TcpStream) {
    let mut reader = BufReader::new(&stream);
    let mut req_line = String::new();
    if reader
        .read_line(&mut req_line)
        .ok()
        .map_or(true, |n| n == 0)
        || req_line.trim().is_empty()
    {
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
            content_length = line
                .split(':')
                .nth(1)
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
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
            // Chunk size — strip extensions after ';'
            let size_str = line.split(';').next().unwrap_or("").trim();
            let sz = usize::from_str_radix(size_str, 16).unwrap_or(0);
            if sz == 0 {
                break;
            }
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
        ("POST", p) if p.starts_with("/v1/messages") || p.starts_with("/messages") => {
            handle_messages(stream, &body)
        }
        ("OPTIONS", _) => respond_cors(stream),
        _ => respond_404(stream),
    }
}

fn respond_cors(mut s: TcpStream) {
    let _ = s.write_all(b"HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: *\r\nContent-Length: 0\r\n\r\n");
}
fn respond_404(mut s: TcpStream) {
    let _ = s.write_all(b"HTTP/1.1 404\r\nContent-Length: 2\r\n\r\n{}");
}
fn respond_hello(mut s: TcpStream, head: bool) {
    let b = r#"{"status":"ok"}"#;
    let h = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nx-request-id: bridge-{}\r\n\r\n{}", b.len(), std::process::id(), if head { "" } else { b });
    let _ = s.write_all(h.as_bytes());
}
fn respond_models(mut s: TcpStream) {
    let body = get_models_json();
    let h = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = s.write_all(h.as_bytes());
}

fn get_models_json() -> String {
    r#"{"data":[
        {"type":"model","id":"auto","display_name":"Auto"},
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
                        if let Some(t) = block["text"].as_str() {
                            out.push_str(t);
                            out.push('\n');
                        }
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
                            out.push_str(&format!(
                                "[TOOL_ERROR: {id}]\n{content}\n[/TOOL_ERROR]\n"
                            ));
                        } else {
                            out.push_str(&format!(
                                "[TOOL_RESULT: {id}]\n{content}\n[/TOOL_RESULT]\n"
                            ));
                        }
                    }
                    Some("thinking") => {
                        if let Some(t) = block["thinking"].as_str() {
                            out.push_str(&format!("[thinking]\n{t}\n[/thinking]\n"));
                        }
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
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn build_prompt(messages: &[Message], system: &Option<serde_json::Value>) -> String {
    let mut prompt = String::new();
    let sys = extract_system_text(system);
    if !sys.is_empty() {
        prompt.push_str(&format!("[SYSTEM]\n{sys}\n[/SYSTEM]\n\n"));
    }

    for msg in messages {
        let role = match msg.role.as_str() {
            "assistant" => "Assistant",
            "user" => "User",
            _ => "User",
        };
        prompt.push_str(&format!(
            "[{role}]\n{}\n[/{role}]\n\n",
            extract_text(&msg.content)
        ));
    }
    prompt.push_str("[Assistant]\n");
    prompt
}

// ─── Agent ────────────────────────────────────────────────────

fn spawn_agent(requested_model: &str) -> std::io::Result<Child> {
    let command = find_agent()?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Cursor agent CLI not found. Install it, add it to PATH, then run `agent login`",
        )
    })?;
    let model = normalize_model(requested_model)?;
    log(&format!("spawning: {:?}", command));

    // Run agent in temp dir so it can't touch project files
    let sandbox = std::env::temp_dir().join(format!("cursor-bridge-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&sandbox);
    log(&format!("sandbox: {}", sandbox.display()));

    // Default mode (no --mode) = full agent with tool execution.
    // --force auto-approves tool calls in non-interactive mode.
    // --trust skips workspace trust prompt.
    let mut command = command.command();
    command
        .args([
            "--print",
            "--force",
            "--output-format",
            "stream-json",
            "--model",
        ])
        .arg(&model)
        .arg("--trust")
        .current_dir(&sandbox)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

fn take_stderr(agent: &mut Child) -> Option<std::thread::JoinHandle<String>> {
    let stderr = agent.stderr.take()?;
    Some(std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = BufReader::new(stderr).read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    }))
}

fn join_stderr(reader: Option<std::thread::JoinHandle<String>>) -> String {
    reader
        .and_then(|thread| thread.join().ok())
        .unwrap_or_default()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentOutcome {
    Success,
    UpstreamError,
    Incomplete,
}

fn classify_agent_outcome(result_received: bool, result_is_error: bool) -> AgentOutcome {
    if result_is_error {
        AgentOutcome::UpstreamError
    } else if result_received {
        AgentOutcome::Success
    } else {
        AgentOutcome::Incomplete
    }
}

fn result_event_is_error(event: &serde_json::Value) -> bool {
    if event["is_error"].as_bool().unwrap_or(false)
        || matches!(event["subtype"].as_str(), Some("error" | "failed"))
    {
        return true;
    }

    match &event["error"] {
        serde_json::Value::Object(error) => !error.is_empty(),
        serde_json::Value::String(error) => !error.trim().is_empty(),
        _ => false,
    }
}

fn result_event_error_message(event: &serde_json::Value) -> Option<String> {
    event["error"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| event["message"].as_str().map(str::to_owned))
        .or_else(|| event["result"].as_str().map(str::to_owned))
}

fn agent_status_failed(status: &std::io::Result<ExitStatus>) -> bool {
    !matches!(status, Ok(exit) if exit.success())
}

fn log_agent_failure(status: &std::io::Result<ExitStatus>, stderr: &str, result_received: bool) {
    log(&format!(
        "agent status: {status:?}, result received: {result_received}"
    ));
    if !stderr.trim().is_empty() {
        log(&format!("agent stderr: {}", stderr.trim()));
    }
    log("Cursor agent did not complete the request. Run `agent login` and try again if authentication is required.");
}

fn log_agent_exit_warning(status: &std::io::Result<ExitStatus>) {
    if agent_status_failed(status) {
        log(&format!(
            "agent returned a non-success exit after a terminal result: {status:?}"
        ));
    }
}

fn respond_json_error(mut stream: TcpStream, status: &str, message: &str) {
    let body = serde_json::json!({
        "type": "error",
        "error": {"type": "api_error", "message": message}
    })
    .to_string();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn write_prompt(agent: &mut Child, prompt: &str) {
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
    let requested_model = req.model.as_deref().unwrap_or("cursor-auto");

    let mut agent = match spawn_agent(requested_model) {
        Ok(a) => a,
        Err(e) => {
            respond_json_error(stream, "502 Bad Gateway", &format!("agent: {e}"));
            return;
        }
    };
    write_prompt(&mut agent, &prompt);
    let stderr_reader = take_stderr(&mut agent);

    let reader = BufReader::new(agent.stdout.take().unwrap());
    let mut text = String::new();
    let mut usage = serde_json::json!({});
    let mut result_received = false;
    let mut result_is_error = false;
    let mut result_error_message = None;

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
                result_received = true;
                result_is_error = result_event_is_error(&event);
                result_error_message = result_event_error_message(&event);
                usage = event["usage"].clone();
            }
        }
    }
    let status = agent.wait();
    let stderr = join_stderr(stderr_reader);
    match classify_agent_outcome(result_received, result_is_error) {
        AgentOutcome::Success => log_agent_exit_warning(&status),
        AgentOutcome::UpstreamError | AgentOutcome::Incomplete => {
            log_agent_failure(&status, &stderr, result_received);
            respond_json_error(
                stream,
                "502 Bad Gateway",
                result_error_message.as_deref().unwrap_or(
                    "Cursor agent did not complete the request. Run `agent login` and try again.",
                ),
            );
            return;
        }
    }

    let resp = serde_json::json!({
        "id": format!("msg_{}", std::process::id()), "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": text}], "model": requested_model, "stop_reason": "end_turn",
        "usage": { "input_tokens": usage["inputTokens"].as_u64().unwrap_or(0), "output_tokens": usage["outputTokens"].as_u64().unwrap_or(0) }
    });
    let body = serde_json::to_string(&resp).unwrap_or_default();
    let _ = stream.write_all(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    );
}

// ─── Streaming ────────────────────────────────────────────────

fn write_sse(
    stream: &mut TcpStream,
    event_type: &str,
    data: &serde_json::Value,
) -> std::io::Result<()> {
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
    let requested_model = req.model.as_deref().unwrap_or("cursor-auto");

    let mut agent = match spawn_agent(requested_model) {
        Ok(a) => a,
        Err(e) => {
            respond_json_error(stream, "502 Bad Gateway", &format!("agent: {e}"));
            return;
        }
    };
    log(&format!("prompt: {}b", prompt.len()));
    write_prompt(&mut agent, &prompt);
    let stderr_reader = take_stderr(&mut agent);

    // SSE response headers
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\n");
    let _ = stream.flush();

    let msg_id = format!("msg_{}", std::process::id());
    let _ = write_sse(
        &mut stream,
        "message_start",
        &serde_json::json!({
            "type": "message_start",
            "message": { "id": msg_id, "type": "message", "role": "assistant", "content": [], "model": requested_model, "stop_reason": null, "usage": { "input_tokens": 0, "output_tokens": 0 } }
        }),
    );

    let reader = BufReader::new(agent.stdout.take().unwrap());
    let mut content_index = 0i32;
    let mut result_received = false;
    let mut result_is_error = false;
    let mut result_error_message = None;
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
            match event["type"].as_str() {
                Some("assistant") => {
                    if let Some(blocks) = event["message"]["content"].as_array() {
                        for block in blocks {
                            let block_type = block["type"].as_str().unwrap_or("text");
                            match block_type {
                                "text" => {
                                    if let Some(text) = block["text"].as_str() {
                                        let _ = write_sse(
                                            &mut stream,
                                            "content_block_start",
                                            &serde_json::json!({
                                                "type": "content_block_start", "index": content_index,
                                                "content_block": {"type": "text", "text": text}
                                            }),
                                        );
                                        let _ = write_sse(
                                            &mut stream,
                                            "content_block_delta",
                                            &serde_json::json!({
                                                "type": "content_block_delta", "index": content_index,
                                                "delta": {"type": "text_delta", "text": text}
                                            }),
                                        );
                                        let _ = write_sse(
                                            &mut stream,
                                            "content_block_stop",
                                            &serde_json::json!({
                                                "type": "content_block_stop", "index": content_index
                                            }),
                                        );
                                        content_index += 1;
                                    }
                                }
                                "tool_use" => {
                                    let name = block["name"].as_str().unwrap_or("unknown");
                                    let input = block["input"].clone();
                                    let fallback_id = format!("toolu_{}", content_index);
                                    let tool_id = block["id"].as_str().unwrap_or(&fallback_id);
                                    let _ = write_sse(
                                        &mut stream,
                                        "content_block_start",
                                        &serde_json::json!({
                                            "type": "content_block_start", "index": content_index,
                                            "content_block": {"type": "tool_use", "id": tool_id, "name": name, "input": input}
                                        }),
                                    );
                                    let _ = write_sse(
                                        &mut stream,
                                        "content_block_stop",
                                        &serde_json::json!({
                                            "type": "content_block_stop", "index": content_index
                                        }),
                                    );
                                    content_index += 1;
                                }
                                _ => {} // skip thinking, etc
                            }
                        }
                    }
                }
                Some("result") => {
                    result_received = true;
                    result_is_error = result_event_is_error(&event);
                    result_error_message = result_event_error_message(&event);
                    usage = event["usage"].clone();
                }
                _ => {}
            }
        }
    }

    let status = agent.wait();
    let stderr = join_stderr(stderr_reader);
    match classify_agent_outcome(result_received, result_is_error) {
        AgentOutcome::Success => log_agent_exit_warning(&status),
        AgentOutcome::UpstreamError | AgentOutcome::Incomplete => {
            log_agent_failure(&status, &stderr, result_received);
            let _ = write_sse(
                &mut stream,
                "error",
                &serde_json::json!({
                    "type": "error",
                    "error": {"type": "api_error", "message": result_error_message.as_deref().unwrap_or("Cursor agent did not complete the request. Run `agent login` and try again.")}
                }),
            );
            let _ = stream.write_all(b"data: [DONE]\n\n");
            let _ = stream.flush();
            return;
        }
    }

    let _ = write_sse(
        &mut stream,
        "message_delta",
        &serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": { "input_tokens": usage["inputTokens"].as_u64().unwrap_or(0), "output_tokens": usage["outputTokens"].as_u64().unwrap_or(0) }
        }),
    );
    let _ = write_sse(
        &mut stream,
        "message_stop",
        &serde_json::json!({"type": "message_stop"}),
    );
    let _ = stream.write_all(b"data: [DONE]\n\n");
    let _ = stream.flush();
}

fn handle_messages(mut stream: TcpStream, body: &[u8]) {
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

    if req.stream.unwrap_or(true) {
        handle_streaming(stream, &req);
    } else {
        handle_blocking(stream, &req);
    }
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
        if let Ok(Some(command)) = result {
            assert!(
                !command.program.as_os_str().is_empty(),
                "program must not be empty"
            );
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
        let value: serde_json::Value = serde_json::from_str(&get_models_json()).unwrap();
        assert!(value["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "auto"));
    }

    #[test]
    fn test_normalize_model_aliases_to_cursor_auto() {
        assert_eq!(normalize_model("cursor-auto").unwrap(), "auto");
        assert_eq!(normalize_model("default").unwrap(), "auto");
        assert_eq!(normalize_model("").unwrap(), "auto");
    }

    #[test]
    fn test_normalize_model_preserves_explicit_model() {
        assert_eq!(
            normalize_model("claude-sonnet-4-6-high").unwrap(),
            "claude-sonnet-4-6-high"
        );
        assert!(normalize_model("model with spaces").is_err());
    }

    #[test]
    fn test_windows_cmd_path_uses_cmd_launcher() {
        let command = command_for_path(PathBuf::from(r"C:\Program Files\Cursor\agent.cmd"), true);
        assert_eq!(command.program, PathBuf::from("cmd.exe"));
        assert_eq!(command.prefix_args[0], OsString::from("/d"));
        assert_eq!(command.prefix_args[1], OsString::from("/c"));
        assert_eq!(command.prefix_args[2], OsString::from("call"));
        assert_eq!(
            command.prefix_args[3],
            OsString::from(r"C:\Program Files\Cursor\agent.cmd")
        );
    }

    #[test]
    fn test_native_path_is_launched_directly() {
        let command = command_for_path(PathBuf::from(r"C:\Users\me\.local\bin\claude.exe"), true);
        assert_eq!(
            command.program,
            PathBuf::from(r"C:\Users\me\.local\bin\claude.exe")
        );
        assert!(command.prefix_args.is_empty());
    }

    #[test]
    fn test_windows_script_paths_allow_spaces_and_reject_cmd_metacharacters() {
        assert!(is_supported_command_path(
            Path::new(r"C:\Program Files\Cursor\agent.cmd"),
            true
        ));
        assert!(!is_supported_command_path(
            Path::new(r"C:\Tools\safe&unsafe\agent.cmd"),
            true
        ));
        assert!(!is_supported_command_path(
            Path::new(r"C:\Tools\%TEMP%\agent.cmd"),
            true
        ));
        assert!(is_supported_command_path(
            Path::new(r"C:\Tools\claude.exe"),
            true
        ));
    }

    #[test]
    fn test_result_error_detection_ignores_placeholders() {
        assert!(!result_event_is_error(&serde_json::json!({
            "type": "result",
            "error": ""
        })));
        assert!(!result_event_is_error(&serde_json::json!({
            "type": "result",
            "error": "  "
        })));
        assert!(!result_event_is_error(&serde_json::json!({
            "type": "result",
            "error": null
        })));
        assert!(!result_event_is_error(&serde_json::json!({
            "type": "result",
            "error": {}
        })));
    }

    #[test]
    fn test_result_error_detection_requires_meaningful_error() {
        assert!(result_event_is_error(&serde_json::json!({
            "type": "result",
            "error": "agent failed"
        })));
        assert!(result_event_is_error(&serde_json::json!({
            "type": "result",
            "error": {"message": "agent failed"}
        })));
    }

    #[test]
    fn test_command_selection_prefers_safe_candidate() {
        let paths = vec![
            PathBuf::from(r"C:\Unsafe&Folder\agent.cmd"),
            PathBuf::from(r"C:\Program Files\Cursor\agent.cmd"),
        ];
        let selected = choose_supported_command_path(&paths, true).unwrap();
        assert_eq!(
            selected,
            Some(PathBuf::from(r"C:\Program Files\Cursor\agent.cmd"))
        );
    }

    #[test]
    fn test_command_selection_reports_only_rejected_candidates() {
        let paths = vec![PathBuf::from(r"C:\Unsafe&Folder\agent.cmd")];
        let rejected = choose_supported_command_path(&paths, true).unwrap_err();
        assert_eq!(rejected, paths[0]);
    }

    #[test]
    #[cfg(windows)]
    fn test_claude_windows_candidate_uses_native_install_location() {
        let candidates = claude_windows_candidates(Path::new(r"C:\Users\me"));
        assert_eq!(
            candidates,
            vec![PathBuf::from(r"C:\Users\me\.local\bin\claude.exe")]
        );
    }

    #[test]
    fn test_proxy_starts_and_stops_promptly() {
        let proxy = Proxy::start().expect("proxy should start");
        assert!(proxy.port() > 0);
        drop(proxy);
    }

    #[test]
    fn test_successful_result_wins_over_wrapper_exit_status() {
        assert_eq!(classify_agent_outcome(true, false), AgentOutcome::Success);
    }

    #[test]
    fn test_explicit_result_error_is_not_success() {
        assert_eq!(
            classify_agent_outcome(true, true),
            AgentOutcome::UpstreamError
        );
        assert!(result_event_is_error(&serde_json::json!({
            "type": "result",
            "is_error": true
        })));
        assert!(result_event_is_error(&serde_json::json!({
            "type": "result",
            "subtype": "failed"
        })));
    }

    #[test]
    fn test_missing_result_is_incomplete_even_after_successful_exit() {
        assert_eq!(
            classify_agent_outcome(false, false),
            AgentOutcome::Incomplete
        );
    }
}
