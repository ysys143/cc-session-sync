use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

// ── Entry struct ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, Clone)]
struct SessionLogEntry {
    #[serde(default)]
    display: Option<String>,
    #[serde(default)]
    pasted_contents: serde_json::Value,
    #[serde(default)]
    message: Option<serde_json::Value>,
    #[serde(alias = "timestamp", default)]
    timestamp: Option<serde_json::Value>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, rename = "sessionId")]
    session_id: Option<String>,
    #[serde(default, rename = "agentId")]
    agent_id: Option<String>,
    #[serde(default, rename = "type")]
    entry_type: Option<String>,
    #[serde(default)]
    data: Option<serde_json::Value>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default, rename = "parentToolUseID")]
    parent_tool_use_id: Option<String>,
    #[serde(default, rename = "isSidechain")]
    is_sidechain: bool,
    #[serde(default, rename = "toolUseResult")]
    tool_use_result: Option<serde_json::Value>,
}

impl SessionLogEntry {
    fn get_display(&self) -> Option<String> {
        self.display.clone().or_else(|| {
            if self.entry_type.as_deref() == Some("progress") {
                if let Some(data) = &self.data {
                    if data.get("type").and_then(|t| t.as_str()) == Some("bash_progress") {
                        let output = data
                            .get("fullOutput")
                            .or_else(|| data.get("output"))
                            .and_then(|o| o.as_str())
                            .unwrap_or("");
                        if !output.is_empty() {
                            return Some(output.to_string());
                        }
                    }
                }
                return None;
            }
            self.message
                .as_ref()
                .and_then(|msg| {
                    let content = msg.get("content")?;
                    if let Some(s) = content.as_str() {
                        return Some(s.to_string());
                    }
                    if let Some(arr) = content.as_array() {
                        let mut parts: Vec<String> = Vec::new();
                        for block in arr {
                            let block_type =
                                block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                            match block_type {
                                "text" => {
                                    if let Some(t) =
                                        block.get("text").and_then(|t| t.as_str())
                                    {
                                        if !t.is_empty() {
                                            parts.push(t.to_string());
                                        }
                                    }
                                }
                                "tool_use" => {
                                    let name = block
                                        .get("name")
                                        .and_then(|n| n.as_str())
                                        .unwrap_or("unknown");
                                    let input = block
                                        .get("input")
                                        .map(|i| serde_json::to_string(i).unwrap_or_default())
                                        .unwrap_or_default();
                                    parts.push(format!("> **Tool: {}** `{}`", name, input));
                                }
                                "tool_result" => {
                                    let result_content = block.get("content");
                                    if let Some(rc) = result_content {
                                        if let Some(s) = rc.as_str() {
                                            if !s.is_empty() {
                                                parts.push(format!(
                                                    "> {}",
                                                    s.lines()
                                                        .collect::<Vec<_>>()
                                                        .join("\n> ")
                                                ));
                                            }
                                        } else if let Some(rc_arr) = rc.as_array() {
                                            for item in rc_arr {
                                                if let Some(t) =
                                                    item.get("text").and_then(|t| t.as_str())
                                                {
                                                    if !t.is_empty() {
                                                        parts.push(format!(
                                                            "> {}",
                                                            t.lines()
                                                                .collect::<Vec<_>>()
                                                                .join("\n> ")
                                                        ));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        if !parts.is_empty() {
                            return Some(parts.join("\n\n"));
                        }
                    }
                    None
                })
                .or_else(|| {
                    self.tool_use_result.as_ref().and_then(|r| {
                        if let Some(s) = r.as_str() {
                            if !s.is_empty() {
                                return Some(format!("> [Error] {}", s));
                            }
                        } else if r.is_object() {
                            let stdout = r.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
                            let stderr = r.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
                            let mut parts = Vec::new();
                            if !stdout.is_empty() {
                                parts.push(format!(
                                    "> {}",
                                    stdout.lines().collect::<Vec<_>>().join("\n> ")
                                ));
                            }
                            if !stderr.is_empty() {
                                parts.push(format!(
                                    "> [stderr] {}",
                                    stderr.lines().collect::<Vec<_>>().join("\n> ")
                                ));
                            }
                            if !parts.is_empty() {
                                return Some(parts.join("\n\n"));
                            }
                        }
                        None
                    })
                })
        })
    }

    fn get_timestamp_millis(&self) -> u64 {
        match &self.timestamp {
            Some(ts) => match ts {
                serde_json::Value::Number(n) => n.as_u64().unwrap_or(0),
                serde_json::Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
                    .ok()
                    .map(|dt| dt.timestamp_millis() as u64)
                    .unwrap_or(0),
                _ => 0,
            },
            None => 0,
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn expand_home(path: &str) -> PathBuf {
    if path.starts_with("~/") {
        let home = dirs::home_dir().expect("Could not determine home directory");
        home.join(&path[2..])
    } else {
        PathBuf::from(path)
    }
}

fn format_date(timestamp: u64) -> String {
    let dt = DateTime::<Local>::from(
        SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(timestamp),
    );
    dt.format("%Y-%m-%d").to_string()
}

fn format_time(timestamp: u64) -> String {
    let dt = DateTime::<Local>::from(
        SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(timestamp),
    );
    dt.format("%H:%M:%S").to_string()
}

fn get_project_name(project_path: &str) -> String {
    PathBuf::from(project_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn file_mtime_and_size(path: &PathBuf) -> Result<(i64, i64)> {
    let meta = fs::metadata(path).with_context(|| format!("stat {:?}", path))?;
    let mtime = meta
        .modified()
        .with_context(|| format!("mtime {:?}", path))?
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let size = meta.len() as i64;
    Ok((mtime, size))
}

/// Parse every line of a JSONL file and return entries (invalid lines skipped).
fn parse_jsonl(path: &PathBuf) -> Result<Vec<SessionLogEntry>> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("Failed to read {:?}", path))?;
    let mut entries = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<SessionLogEntry>(line) {
            Ok(e) => entries.push(e),
            Err(e) => eprintln!("Warning: Failed to parse line in {:?} - {}", path, e),
        }
    }
    Ok(entries)
}

// ── SQLite helpers ────────────────────────────────────────────────────────────

fn open_db(obsidian_vault: &PathBuf) -> Result<Connection> {
    let db_path = obsidian_vault.join(".metadata.db");
    let conn = Connection::open(&db_path)
        .with_context(|| format!("Failed to open SQLite DB at {:?}", db_path))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS files (
             path TEXT PRIMARY KEY,
             mtime INTEGER NOT NULL,
             size INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS session_files (
             session_id TEXT NOT NULL,
             file_path TEXT NOT NULL,
             PRIMARY KEY (session_id, file_path)
         );
         CREATE TABLE IF NOT EXISTS sessions (
             session_id TEXT PRIMARY KEY,
             project TEXT,
             output_path TEXT,
             entry_count INTEGER DEFAULT 0,
             synced_at INTEGER NOT NULL
         );",
    )?;
    Ok(conn)
}

fn db_file_record(conn: &Connection, path: &str) -> Option<(i64, i64)> {
    conn.query_row(
        "SELECT mtime, size FROM files WHERE path = ?1",
        params![path],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .ok()
}

fn db_upsert_file(conn: &Connection, path: &str, mtime: i64, size: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO files (path, mtime, size) VALUES (?1, ?2, ?3)
         ON CONFLICT(path) DO UPDATE SET mtime=excluded.mtime, size=excluded.size",
        params![path, mtime, size],
    )?;
    Ok(())
}

fn db_upsert_session_files(
    conn: &Connection,
    session_id: &str,
    file_path: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO session_files (session_id, file_path) VALUES (?1, ?2)",
        params![session_id, file_path],
    )?;
    Ok(())
}

fn db_files_for_session(conn: &Connection, session_id: &str) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT file_path FROM session_files WHERE session_id = ?1")?;
    let paths: Vec<String> = stmt
        .query_map(params![session_id], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(paths)
}

fn db_upsert_session(
    conn: &Connection,
    session_id: &str,
    project: Option<&str>,
    output_path: &str,
    entry_count: usize,
    synced_at: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO sessions (session_id, project, output_path, entry_count, synced_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(session_id) DO UPDATE SET
             project=excluded.project,
             output_path=excluded.output_path,
             entry_count=excluded.entry_count,
             synced_at=excluded.synced_at",
        params![session_id, project, output_path, entry_count as i64, synced_at],
    )?;
    Ok(())
}

/// Returns all sessions ordered by synced_at DESC.
fn db_all_sessions(
    conn: &Connection,
) -> Result<Vec<(String, Option<String>, Option<String>, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT session_id, project, output_path, synced_at FROM sessions ORDER BY synced_at DESC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

// ── Markdown generation ───────────────────────────────────────────────────────

fn convert_session_to_markdown(entries: &[SessionLogEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }

    let mut markdown = String::new();
    let session_id = entries
        .first()
        .and_then(|e| e.session_id.as_deref())
        .unwrap_or("unknown");
    let project = entries
        .first()
        .and_then(|e| e.project.as_deref().or(e.cwd.as_deref()))
        .map(get_project_name)
        .unwrap_or_else(|| "unknown".to_string());

    markdown.push_str(&format!("# Session: {}\n\n", session_id));
    markdown.push_str(&format!("**Project:** {}\n", project));

    let mut agent_ids: HashSet<String> = HashSet::new();
    for entry in entries {
        if let Some(agent_id) = &entry.agent_id {
            if !agent_id.is_empty() {
                agent_ids.insert(agent_id.clone());
            }
        }
    }
    if !agent_ids.is_empty() {
        let mut ids: Vec<_> = agent_ids.iter().cloned().collect();
        ids.sort();
        markdown.push_str(&format!("**Subagents:** {}\n", ids.join(", ")));
    }
    markdown.push_str(&format!("**Generated:** {}\n\n", chrono::Local::now()));

    // Keep only the first bash_progress entry per parentToolUseID
    let mut bash_progress_seen: HashMap<String, usize> = HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        if entry.entry_type.as_deref() == Some("progress") {
            if let Some(data) = &entry.data {
                if data.get("type").and_then(|t| t.as_str()) == Some("bash_progress") {
                    let key = entry
                        .parent_tool_use_id
                        .clone()
                        .or_else(|| entry.uuid.clone())
                        .unwrap_or_else(|| i.to_string());
                    bash_progress_seen.entry(key).or_insert(i);
                }
            }
        }
    }
    let kept_bash_indices: HashSet<usize> = bash_progress_seen.values().cloned().collect();

    let mut by_date: BTreeMap<String, Vec<&SessionLogEntry>> = BTreeMap::new();
    let mut seen_content: HashSet<String> = HashSet::new();

    for (i, entry) in entries.iter().enumerate() {
        let is_bash_progress = entry.entry_type.as_deref() == Some("progress")
            && entry
                .data
                .as_ref()
                .and_then(|d| d.get("type"))
                .and_then(|t| t.as_str())
                == Some("bash_progress");
        if is_bash_progress && !kept_bash_indices.contains(&i) {
            continue;
        }
        if let Some(display) = entry.get_display() {
            let content_key = format!(
                "{}:{}",
                entry.entry_type.as_deref().unwrap_or(""),
                display
            );
            if !seen_content.insert(content_key) {
                continue;
            }
        }
        let date = format_date(entry.get_timestamp_millis());
        by_date.entry(date).or_default().push(entry);
    }

    // Entries are sorted ascending; iterate dates ascending, entries ascending
    for (date, date_entries) in by_date.iter() {
        markdown.push_str(&format!("## {}\n\n", date));
        for entry in date_entries.iter() {
            let role_prefix = match entry.entry_type.as_deref() {
                Some("user") => "**User:** ",
                Some("assistant") => "**Assistant:** ",
                Some("progress") => "**Output:** ",
                _ => "",
            };
            if let Some(display) = entry.get_display() {
                let time = format_time(entry.get_timestamp_millis());
                markdown.push_str(&format!("### {}\n", time));
                markdown.push_str(&format!("{}{}\n\n", role_prefix, display));
            }
        }
    }

    markdown
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let projects_dir = expand_home("~/.claude/projects");
    let obsidian_vault = std::env::var("CLAUDE_SESSIONS_PATH")
        .map(|p| expand_home(&p))
        .unwrap_or_else(|_| expand_home("~/Documents/Obsidian/claude-code-sessions"));

    if !projects_dir.exists() {
        anyhow::bail!("Projects directory not found: {:?}", projects_dir);
    }
    if !obsidian_vault.exists() {
        fs::create_dir_all(&obsidian_vault)
            .with_context(|| format!("Failed to create {:?}", obsidian_vault))?;
        println!("Created Obsidian vault directory: {:?}", obsidian_vault);
    }

    let sessions_dir = obsidian_vault.join("sessions");
    if !sessions_dir.exists() {
        fs::create_dir_all(&sessions_dir)?;
    }

    let conn = open_db(&obsidian_vault)?;

    // ── Step 1-4: Scan files, detect changes ─────────────────────────────────
    let mut changed_files: Vec<PathBuf> = Vec::new();
    let mut unchanged_files: Vec<PathBuf> = Vec::new();

    for dir_entry in WalkDir::new(&projects_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jsonl"))
    {
        let path = dir_entry.path().to_path_buf();
        let path_str = path.to_string_lossy().to_string();
        let (mtime, size) = file_mtime_and_size(&path)?;
        match db_file_record(&conn, &path_str) {
            Some((db_mtime, db_size)) if db_mtime == mtime && db_size == size => {
                unchanged_files.push(path);
            }
            _ => {
                changed_files.push(path);
            }
        }
    }

    println!(
        "Changed files: {}, Unchanged: {}",
        changed_files.len(),
        unchanged_files.len()
    );

    // ── Step 3: Read changed files, update DB, cache entries ─────────────────
    // file_path -> Vec<SessionLogEntry>
    let mut file_cache: HashMap<PathBuf, Vec<SessionLogEntry>> = HashMap::new();
    let mut affected_sessions: HashSet<String> = HashSet::new();

    for path in &changed_files {
        let path_str = path.to_string_lossy().to_string();
        let (mtime, size) = file_mtime_and_size(path)?;
        let entries = parse_jsonl(path)?;

        // Collect session_ids from this file
        for entry in &entries {
            if let Some(sid) = &entry.session_id {
                if !sid.is_empty() {
                    affected_sessions.insert(sid.clone());
                    db_upsert_session_files(&conn, sid, &path_str)?;
                }
            }
        }

        db_upsert_file(&conn, &path_str, mtime, size)?;
        file_cache.insert(path.clone(), entries);
    }

    // ── Step 6: Early exit if nothing changed ────────────────────────────────
    if affected_sessions.is_empty() {
        println!("No changes detected.");
        return Ok(());
    }

    println!("Affected sessions: {}", affected_sessions.len());

    // ── Step 7: For each affected session, collect ALL file paths, build entries
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut written_count = 0usize;

    for session_id in &affected_sessions {
        // Get every file that has ever contributed to this session
        let all_paths = db_files_for_session(&conn, session_id)?;

        let mut session_entries: Vec<SessionLogEntry> = Vec::new();
        let mut seen_uuids: HashSet<String> = HashSet::new();

        for path_str in &all_paths {
            let path = PathBuf::from(path_str);

            // Use cache if available (changed file already read), else read now
            let entries: Vec<SessionLogEntry> = if let Some(cached) = file_cache.get(&path) {
                cached.clone()
            } else {
                parse_jsonl(&path).unwrap_or_default()
            };

            for entry in entries {
                if entry.session_id.as_deref() != Some(session_id.as_str()) {
                    continue;
                }
                // UUID dedup
                let key = entry.uuid.clone().unwrap_or_else(|| {
                    format!(
                        "{}:{}",
                        entry.get_timestamp_millis(),
                        entry.entry_type.as_deref().unwrap_or("")
                    )
                });
                if seen_uuids.insert(key) {
                    session_entries.push(entry);
                }
            }
        }

        // Sort ascending by timestamp
        session_entries.sort_by_key(|e| e.get_timestamp_millis());

        let entry_count = session_entries.len();
        let markdown = convert_session_to_markdown(&session_entries);

        let safe_id = session_id.replace("/", "_");
        let session_file = sessions_dir.join(format!("{}.md", safe_id));
        let output_path = format!("sessions/{}.md", safe_id);
        fs::write(&session_file, &markdown)?;
        written_count += 1;

        let project = session_entries
            .first()
            .and_then(|e| e.project.as_deref().or(e.cwd.as_deref()))
            .map(get_project_name);

        db_upsert_session(
            &conn,
            session_id,
            project.as_deref(),
            &output_path,
            entry_count,
            now_secs,
        )?;
    }

    // ── Step 8: Write README from ALL sessions in DB ─────────────────────────
    let all_sessions = db_all_sessions(&conn)?;
    let mut index_lines = vec![
        "# Claude Code Sessions Index".to_string(),
        "".to_string(),
        format!("Generated: {}", chrono::Local::now()),
        "".to_string(),
        "## Sessions".to_string(),
        "".to_string(),
    ];
    for (sid, project, output_path, _synced_at) in &all_sessions {
        let proj = project.as_deref().unwrap_or("unknown");
        let path = output_path.as_deref().unwrap_or("");
        index_lines.push(format!("- [{}]({}) - {}", sid, path, proj));
    }
    let index_content = index_lines.join("\n");
    fs::write(obsidian_vault.join("README.md"), &index_content)?;

    println!("Written: {} sessions", written_count);
    println!("Index updated: {} total sessions", all_sessions.len());

    Ok(())
}
