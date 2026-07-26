# cursor-bridge

**One binary. Claude Code on Cursor's backend. Zero config.**

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
  ├── Reads your Cursor auth token from macOS keychain (or CURSOR_TOKEN env var on Linux)
  ├── Spawns `claude` with env vars pointing at the proxy
  ├── Proxy translates Anthropic API calls → Cursor agent CLI
  └── Cleans up on exit
```

You don't see the proxy. You don't manage it. It's there and gone.

## Install

```bash
# Prerequisites
# - Cursor installed with `agent` CLI authenticated (`agent login`)
# - Claude Code installed (`curl -O https://claude-code.anthropic.com/claude && chmod +x claude`)

cargo install cursor-bridge

# Then just use it
cursor-bridge
```

Or download a binary from Releases.

## Requirements

- **macOS** or **Linux**
- Cursor subscription (with `agent` CLI in PATH)
- Claude Code CLI (`claude` in PATH)
- **macOS**: token auto-read from keychain
- **Linux**: set `CURSOR_TOKEN` env var (no keychain fallback)

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

- **Linux**: requires `CURSOR_TOKEN` env var (no keychain support).
- **No workspace sandboxing** — the agent runs in your current directory.
- **Single account** — no multi-account rotation (yet).

## Legal

This project is not affiliated with Anthropic or Cursor/Anysphere. Use at your own risk.

## License

MIT
