# cursor-bridge

**One binary. Claude Code on Cursor's backend. Zero config.**

[![Crates.io](https://img.shields.io/crates/v/cursor-bridge)](https://crates.io/crates/cursor-bridge)
[![License](https://img.shields.io/github/license/hkc5/cursor-bridge)](LICENSE)
[![Stars](https://img.shields.io/github/stars/hkc5/cursor-bridge)](https://github.com/hkc5/cursor-bridge)
[![CI](https://img.shields.io/github/actions/workflow/status/hkc5/cursor-bridge/ci.yml?branch=main)](https://github.com/hkc5/cursor-bridge/actions/workflows/ci.yml)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows%2011-blue)](.)

![demo](demo.webp)

## Why does this exist?

You have a **Cursor subscription**. You want to use **Claude Code** (the CLI).
Cursor's **Auto model** is included with your subscription — free, unlimited, no extra per-token cost.

Without cursor-bridge, you'd pay separately for Anthropic API credits or a Claude Pro plan.
With cursor-bridge, you just run `cursor-bridge` and it works — Claude Code runs on your Cursor backend.

**Use cases:**
- You're already paying for Cursor → get Claude Code for free on top
- You want Claude Code's agent capabilities (file editing, shell commands, tool use) without Anthropic billing
- Cursor's Auto model is free and unlimited with subscription — Claude Code becomes effectively free to run

```bash
cursor-bridge                         # interactive session
cursor-bridge "refactor this file"    # one-shot prompt
cursor-bridge -p "list files"         # pipe mode
```

That's it. No proxy management. No env vars. Everything automatic.

## How it works

```
cursor-bridge (Rust binary)
  ├── Starts a local HTTP proxy on a random port
  ├── Uses the authenticated Cursor `agent` CLI as the backend
  ├── Spawns `claude` with env vars pointing at the proxy
  ├── Proxy translates Anthropic API calls → Cursor agent CLI
  └── Cleans up on exit
```

You don't see the proxy. You don't manage it. It's there and gone.

## Install

```bash
# Prerequisites
# - Cursor installed with the `agent` CLI
# - Cursor CLI authenticated (`agent login`)
# - Claude Code installed and available as `claude`

cargo install cursor-bridge

# Then just use it
cursor-bridge
```

Or download a binary from Releases.

## Requirements

- **macOS**, **Linux**, or **Windows 11 x64**
- Cursor subscription with the `agent` CLI in `PATH`
- Authenticated Cursor CLI: run `agent login` before starting the bridge
- Claude Code CLI (`claude` in `PATH`)

### Windows

Install Claude Code from PowerShell:

```powershell
irm https://claude.ai/install.ps1 | iex
```

This is the official installer command, but it downloads and executes a remote PowerShell script. Review the [official installation and integrity documentation](https://code.claude.com/docs/en/setup#binary-integrity-and-code-signing) first if your environment requires verified installation. You can use the package-managed alternative instead:

```powershell
winget install Anthropic.ClaudeCode
```

The Cursor CLI currently exposes `agent.cmd`; the bridge launches it through `cmd.exe`. Claude Code is expected to be the native `claude.exe` command. Use `AGENT_PATH` or `CLAUDE_PATH` when either command is installed outside `PATH`.

The bridge does not read Cursor tokens or Windows Credential Manager. Authentication is owned by the Cursor CLI, so run `agent login` when setup is incomplete or a session reports an authentication failure.

Windows `.cmd` and `.bat` overrides must use ordinary filesystem paths without command-interpreter metacharacters. Native `.exe` paths are launched directly.

For nonstandard installations, set the command paths in PowerShell:

```powershell
$env:AGENT_PATH = "C:\Tools\cursor-agent\agent.cmd"
$env:CLAUDE_PATH = "C:\Tools\claude\claude.exe"
cursor-bridge
```

## How it differs from other proxies

**All other solutions are background servers you manage. cursor-bridge is a command you run.**

Existing proxies (`cursor-api-proxy`, `cursor-composer-in-claude`, `cursor-proxy`) are Node.js servers that live in your process list, occupy a port, and need manual env var wiring. They don't ship with Claude Code — they sit between you and it, adding ceremony.

**cursor-bridge is the opposite.** There is nothing to start, stop, or configure. It *is* the session:

| The old way | cursor-bridge |
|---|---|
| Start a proxy daemon, note the port, set env vars, *then* run `claude` | Run `cursor-bridge` — done |
| Background process that outlives your session | Lives and dies with your terminal |
| Pick a port, pray it doesn't clash | Random port, zero conflicts |
| `npm install` + `npx` + Node.js runtime (60+ MB) | One Rust binary, ~780 KB, statically compiled |
| Multiple npm packages, peer deps, version mismatches | `cargo install` or download. One binary. Nothing else. |

No daemon. No `npm install`. No env vars. No port hunting. No cleanup. Just a single binary that works.

cursor-bridge replaces `claude` entirely — it manages the proxy lifecycle internally, spawns the CLI, and cleans up after itself when you're done.

## Caveats

- **Windows**: the first release targets x64 only.
- **Authentication**: the bridge does not start `agent login` automatically.
- **Workspace edits** — the agent currently runs in `%TEMP%\cursor-bridge-<process-id>`, so file-editing prompts do not operate on the current repository yet.
- **Single account** — no multi-account rotation (yet).

## Legal

This project is not affiliated with Anthropic or Cursor/Anysphere. Use at your own risk.

## License

MIT
