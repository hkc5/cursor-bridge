# cursor-bridge

**One binary. Claude Code on Cursor's backend. Zero config.**

Just like `claude`, but runs on your Cursor subscription. No proxy management. No environment variables to set.

```bash
cursor-bridge                         # interactive session
cursor-bridge "refactor this file"    # one-shot prompt
cursor-bridge -p "list files"         # pipe mode
```

That's it. Everything else is automatic.

## How it works

```
cursor-bridge (Rust binary)
  ├── Starts a local HTTP proxy on a random port
  ├── Reads your Cursor auth token from macOS keychain
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

- macOS (keychain for auth token)
- Cursor subscription (with `agent` CLI in PATH)
- Claude Code CLI (`claude` in PATH)

## How it differs from other proxies

**cursor-bridge is not a server, not a daemon, not a background process.** You don't start it separately and leave it running. It's a drop-in replacement for `claude`.

| Other proxies | cursor-bridge |
|---|---|
| Run as a background server | Starts and stops with your session |
| Need to set env vars manually | Sets everything automatically |
| Need to find the right port | Random port, no conflicts |
| Multiple npm dependencies | Single Rust binary, one dependency |
| Node.js runtime required | Statically compiled (~740KB) |

## Caveats

- **Only macOS** for now (keychain access). Linux support can be added.
- **No workspace sandboxing** — the agent runs in your current directory.
- **Single account** — no multi-account rotation (yet).

## Legal

This project is not affiliated with Anthropic or Cursor/Anysphere. Use at your own risk.

## License

MIT
