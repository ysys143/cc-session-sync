# sync-sessions-to-obsidian

A Rust CLI tool that converts Claude Code JSONL session logs to formatted markdown with SQLite-based incremental synchronization.

**Key features:**
- Recursive JSONL file scanning with change detection via mtime/size
- YAML frontmatter (date, time, datetime, project, cwd, summary)
- Role-based entry formatting (User/Assistant/Output)
- Intelligent deduplication (bash_progress by tool ID, UUID-based content dedup)
- Full tool result capture (no truncation)
- SQLite incremental sync with per-file mtime tracking
- Automatic project name extraction with fallback logic
- Smart summary extraction (filters noise, truncates to 100 chars)
- Environment variable support for custom output paths
- SessionEnd hook integration for automatic syncing

## Building

```bash
cargo build --release
```

The binary is created at `target/release/sync` (2.6 MB).

## Usage

### Direct Invocation

```bash
./target/release/sync
```

Scans `~/.claude/projects/**/*.jsonl` and writes markdown files to the default Obsidian vault path (`~/Documents/Obsidian/claude-code-sessions`).

### Custom Output Directory

Set `CLAUDE_SESSIONS_PATH` to specify a different output location:

```bash
export CLAUDE_SESSIONS_PATH=~/my-obsidian-vault
./target/release/sync
```

### Automatic Integration with SessionEnd Hook

Add the following to `~/.claude/settings.json` to run `sync` automatically when Claude Code sessions end:

```json
{
  "hooks": {
    "SessionEnd": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "/path/to/sync"
          },
          {
            "type": "command",
            "command": "nohup qmd update > /tmp/qmd-update.log 2>&1 &"
          }
        ]
      }
    ]
  }
}
```

The second command (qmd update) runs in the background without blocking the hook. Adjust paths as needed.

## How It Works

### 1. File Change Detection

The tool scans `~/.claude/projects` recursively for `.jsonl` files. For each file, it records the modification time and size in SQLite's `files` table. Only files with changed mtime or size are reprocessed.

Directory structure:
```
~/.claude/projects/
├── -Users-username-project-1/
│   ├── session.jsonl
│   └── session-2.jsonl
└── -Users-username-project-2/
    └── session.jsonl
```

### 2. Session Extraction and Deduplication

For each changed file:
1. Parses all JSONL lines (invalid lines logged as warnings)
2. Extracts unique `session_id` values
3. Tracks which files contribute entries to each session
4. Deduplicates entries:
   - **bash_progress entries**: Keeps only the first occurrence per `parentToolUseID` or `uuid`
   - **All entries**: Uses UUID-based and content-based dedup to prevent duplicates across multiple file changes

### 3. Markdown Conversion

For each affected session:
1. Collects all entries from all files that contributed to that session
2. Sorts entries by timestamp (ascending)
3. Generates markdown with YAML frontmatter and role-based formatting

**YAML frontmatter example:**
```yaml
---
date: 2026-03-15
time: 14:30:45
datetime: 2026-03-15 14:30:45
project: my-project
cwd: /Users/username/projects/my-project
summary: Implement feature X with error handling
---
```

**Entry formatting:**
- **User entries**: `**User:** [content]`
- **Assistant entries**: `**Assistant:** [content]`
- **Tool output**: `**Output:** [content]` (from progress entries)

Tool results are captured in full with no truncation. Multi-line content is prefixed with `> ` for blockquote formatting.

### 4. Database Schema

Three tables track sync state and metadata, stored in `.metadata.db`:

**`files` table:**
```sql
CREATE TABLE files (
  path TEXT PRIMARY KEY,
  mtime INTEGER NOT NULL,
  size INTEGER NOT NULL
);
```
Stores file modification time and size for change detection.

**`sessions` table:**
```sql
CREATE TABLE sessions (
  session_id TEXT PRIMARY KEY,
  project TEXT,
  output_path TEXT,
  entry_count INTEGER DEFAULT 0,
  synced_at INTEGER NOT NULL,
  summary TEXT,
  session_datetime TEXT
);
```
Stores session metadata: project name, output file path, entry count, sync timestamp, summary, and session datetime.

**`session_files` table:**
```sql
CREATE TABLE session_files (
  session_id TEXT NOT NULL,
  file_path TEXT NOT NULL,
  PRIMARY KEY (session_id, file_path)
);
```
Maps sessions to all JSONL files that contribute entries to them.

### 5. Project Name Extraction

Project names are derived in this order:
1. From `cwd` or `project` fields in session entries (extracts final path component)
2. Fallback: From the JSONL file path encoding `~/.claude/projects/<encoded-path>/`
   - Encoded paths use `-` as separator (leading `-` represents root `/`)
   - Takes the final segment as project name

Example: `~/.claude/projects/-Users-username-my-project/session.jsonl` → `my-project`

### 6. Summary Extraction

The first "real" user message (up to 100 characters, single line) is extracted as the session summary. Filtered out as noise:
- Commands starting with `<command`, `<local-command`, `<bash-`
- Lines starting with `>` or `/`
- System messages (`You are...`, `Base directory`, `Warmup`)
- Korean-language system prompts (`요약할 텍스트:`)
- Table rows and XML-tagged content

## Output Structure

```
~/Documents/Obsidian/claude-code-sessions/
├── .metadata.db
└── sessions/
    ├── <session-id-1>.md
    ├── <session-id-2>.md
    └── <session-id-n>.md
```

Session IDs with `/` are replaced with `_` for filesystem safety.

## Dependencies

- **rusqlite** (0.31): SQLite bindings with bundled SQLite
- **walkdir** (2.4): Recursive directory traversal
- **chrono** (0.4): Timestamp formatting and parsing
- **serde** (1.0): JSON deserialization
- **serde_json** (1.0): JSONL parsing
- **anyhow** (1.0): Error handling
- **dirs** (5.0): Home directory resolution

## Integration with qmd (Search)

The `sessions/` directory can be indexed by qmd (Claude Code's markdown search) as a collection:

```json
{
  "collections": {
    "claude-sessions": {
      "path": "~/Documents/Obsidian/claude-code-sessions/sessions",
      "type": "markdown"
    }
  }
}
```

This enables full-text BM25 keyword search across all sessions. Time-based filtering can be performed via SQLite queries on the `.metadata.db` file.

## Example Session Markdown

```markdown
---
date: 2026-03-15
time: 14:30:45
datetime: 2026-03-15 14:30:45
project: my-project
cwd: /Users/username/projects/my-project
summary: Implement feature X with error handling
---

# Session: abc123def456

**Project:** my-project
**Generated:** 2026-03-15 15:45:32.123456789 UTC

## 2026-03-15

### 14:30:45
**User:** Implement feature X with error handling

### 14:31:02
**Assistant:** I'll start by examining the codebase structure.

### 14:31:15
**Output:** > Scanned 42 files in src/

### 14:32:10
**User:** Add validation logic

### 14:32:45
**Assistant:** Here's the validation function:
> ```rust
> fn validate_input(input: &str) -> Result<bool> {
>   ...
> }
> ```

### 14:33:00
**Output:** > [stderr] warning: unused variable
> [stdout] Build succeeded
```

## Verification

Test the tool locally:

```bash
# Build and run with default output path
cargo build --release
./target/release/sync

# Check output
ls ~/Documents/Obsidian/claude-code-sessions/sessions/ | wc -l
sqlite3 ~/Documents/Obsidian/claude-code-sessions/.metadata.db "SELECT COUNT(*) FROM sessions;"
```

Verify a session was created:

```bash
head -50 ~/Documents/Obsidian/claude-code-sessions/sessions/$(ls -t ~/Documents/Obsidian/claude-code-sessions/sessions/ | head -1)
```

## Recent Changes

- **89f3036:** Fix project name extraction by scanning all entries and falling back to JSONL path
- **4c2dfa9:** Remove `_index.md` generation in favor of direct SQLite queries
- **4969328:** Add YAML frontmatter and datetime and session summaries
- **df19ed3:** Support `CLAUDE_SESSIONS_PATH` environment variable for configurable output directory
- **5ada1b3:** Replace global timestamp metadata with SQLite per-file mtime tracking
- **2408f2e:** Remove 20-line truncation limit on tool result content

## License

MIT
