# Vlk

**Memory that actually helps your agent stop looping.**

v0.5.0

### The Problem

Coding agents (Cursor, Claude in Zed, etc.) get stuck repeating the same errors over and over. The context window fills with noise and the agent becomes ineffective.

Vlk solves this by giving your agent a **sense of time**.

### How It Works

Vlk divides context into three states:

| State      | Meaning                            | Shown to agent |
|------------|------------------------------------|----------------|
| **PRESENT**    | What's happening right now         | Yes            |
| **PAST**       | Errors already learned from        | No             |
| **FUTURE**     | Lessons to prevent future mistakes | Yes (as constraint) |

When the agent hits a loop (e.g. 3 identical 503 errors), `vlk_time_travel` archives the errors and adds a clear lesson:

> **[PREVENTIVE FUTURE CONSTRAINT]** Use local cache — endpoint returned 503 five times.

### Main Tools

- **`vlk_fetch_context`** — Get clean context (PRESENT + FUTURE only)
- **`vlk_time_travel`** — Archive dead errors and create a lesson
- **`vlk_revoke_future`** — Remove a wrong constraint
- **`vlk_get_history`** — View full timeline

### Quick Install

```bash
git clone https://github.com/aranajhonny/vlk.git
cd vlk/vlk-core
cargo build --release
Run the server:
Bash./target/release/vlk-core
```

Setup in Zed
```json
JSON// .zed/settings.json
{
  "context_servers": {
    "vlk": {
      "command": "/path/to/vlk-core",
      "env": {
        "DATABASE_URL": "sqlite:/path/to/vlk.db?mode=rwc"
      }
    }
  }
}
```

Basic Usage
```
Call vlk_fetch_context before every agent turn
When you detect a loop, call vlk_time_travel with a clear lesson
The agent only sees useful lessons — never the repeated noise again
```

Vlk also automatically detects and mitigates loops of 3+ similar errors.
Benefits

Dramatically reduces token usage
Prevents repetitive error loops
Persists across IDE restarts
Lightweight (Rust + SQLite)
Works with Cursor, Zed, and any MCP client

License: MIT
