use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use walkdir::WalkDir;

#[derive(Debug, Serialize, Deserialize)]
struct Metadata {
    last_synced_timestamp: u64,
    total_entries_synced: usize,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            last_synced_timestamp: 0,
            total_entries_synced: 0,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
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
            // For progress entries, extract bash output
            if self.entry_type.as_deref() == Some("progress") {
                if let Some(data) = &self.data {
                    if data.get("type").and_then(|t| t.as_str()) == Some("bash_progress") {
                        let output = data.get("fullOutput")
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
            self.message.as_ref().and_then(|msg| {
                let content = msg.get("content")?;
                // Try string first
                if let Some(s) = content.as_str() {
                    return Some(s.to_string());
                }
                // Try array of content blocks
                if let Some(arr) = content.as_array() {
                    let mut parts: Vec<String> = Vec::new();
                    for block in arr {
                        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match block_type {
                            "text" => {
                                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                    if !t.is_empty() {
                                        parts.push(t.to_string());
                                    }
                                }
                            }
                            "tool_use" => {
                                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("unknown");
                                let input = block.get("input")
                                    .map(|i| serde_json::to_string(i).unwrap_or_default())
                                    .unwrap_or_default();
                                parts.push(format!("> **Tool: {}** `{}`", name, input));
                            }
                            "tool_result" => {
                                let result_content = block.get("content");
                                if let Some(rc) = result_content {
                                    if let Some(s) = rc.as_str() {
                                        if !s.is_empty() {
                                            parts.push(format!("> {}", s.lines().collect::<Vec<_>>().join("\n> ")));
                                        }
                                    } else if let Some(rc_arr) = rc.as_array() {
                                        for item in rc_arr {
                                            if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                                                if !t.is_empty() {
                                                    parts.push(format!("> {}", t.lines().collect::<Vec<_>>().join("\n> ")));
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
            }).or_else(|| {
                // Fallback: use toolUseResult for entries with no message content
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
                            parts.push(format!("> {}", stdout.lines().collect::<Vec<_>>().join("\n> ")));
                        }
                        if !stderr.is_empty() {
                            parts.push(format!("> [stderr] {}", stderr.lines().collect::<Vec<_>>().join("\n> ")));
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
}

impl SessionLogEntry {
    fn get_timestamp_millis(&self) -> u64 {
        match &self.timestamp {
            Some(ts) => match ts {
                serde_json::Value::Number(n) => n.as_u64().unwrap_or(0),
                serde_json::Value::String(s) => {
                    chrono::DateTime::parse_from_rfc3339(s)
                        .ok()
                        .map(|dt| dt.timestamp_millis() as u64)
                        .unwrap_or(0)
                }
                _ => 0,
            },
            None => 0,
        }
    }
}

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
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(timestamp),
    );
    dt.format("%Y-%m-%d").to_string()
}

fn format_time(timestamp: u64) -> String {
    let dt = DateTime::<Local>::from(
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(timestamp),
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

fn load_metadata(obsidian_vault: &PathBuf) -> Metadata {
    let metadata_file = obsidian_vault.join(".metadata.json");
    fs::read_to_string(&metadata_file)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

fn save_metadata(obsidian_vault: &PathBuf, metadata: &Metadata) -> Result<()> {
    let metadata_file = obsidian_vault.join(".metadata.json");
    let json = serde_json::to_string_pretty(metadata)?;
    fs::write(metadata_file, json)?;
    Ok(())
}

fn read_jsonl_files(projects_dir: &PathBuf, min_timestamp: u64) -> Result<Vec<SessionLogEntry>> {
    let mut entries = Vec::new();

    for entry in WalkDir::new(projects_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jsonl"))
    {
        let path = entry.path();
        let contents = fs::read_to_string(path)
            .with_context(|| format!("Failed to read {:?}", path))?;

        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionLogEntry>(line) {
                Ok(entry) => {
                    let ts = entry.get_timestamp_millis();
                    if ts > min_timestamp {
                        entries.push(entry);
                    }
                }
                Err(e) => eprintln!("Warning: Failed to parse line - {}", e),
            }
        }
    }

    entries.sort_by(|a, b| {
        let a_ts = a.get_timestamp_millis();
        let b_ts = b.get_timestamp_millis();
        b_ts.cmp(&a_ts)
    });
    Ok(entries)
}

fn read_jsonl_files_for_sessions(
    projects_dir: &PathBuf,
    session_ids: &std::collections::HashSet<String>,
) -> Result<Vec<SessionLogEntry>> {
    let mut entries = Vec::new();

    for entry in WalkDir::new(projects_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("jsonl"))
    {
        let path = entry.path();
        let contents = fs::read_to_string(path)
            .with_context(|| format!("Failed to read {:?}", path))?;

        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionLogEntry>(line) {
                Ok(entry) => {
                    if entry.session_id.as_deref().map(|id| session_ids.contains(id)).unwrap_or(false) {
                        entries.push(entry);
                    }
                }
                Err(_) => {}
            }
        }
    }

    entries.sort_by(|a, b| b.get_timestamp_millis().cmp(&a.get_timestamp_millis()));
    Ok(entries)
}

fn group_entries_by_session(
    entries: Vec<SessionLogEntry>,
) -> BTreeMap<String, Vec<SessionLogEntry>> {
    let mut grouped: BTreeMap<String, Vec<SessionLogEntry>> = BTreeMap::new();

    for entry in entries {
        let session_id = entry.session_id.as_deref().unwrap_or("unknown");
        grouped
            .entry(session_id.to_string())
            .or_default()
            .push(entry);
    }

    grouped
}

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

    // Collect agent IDs
    let mut agent_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for entry in entries {
        if let Some(agent_id) = &entry.agent_id {
            if !agent_id.is_empty() {
                agent_ids.insert(agent_id.clone());
            }
        }
    }

    if !agent_ids.is_empty() {
        markdown.push_str(&format!("**Subagents:** {}\n", agent_ids.iter().cloned().collect::<Vec<_>>().join(", ")));
    }

    markdown.push_str(&format!("**Generated:** {}\n\n", chrono::Local::now()));

    // For bash_progress entries, keep only the last one per parentToolUseID
    let mut bash_progress_seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        if entry.entry_type.as_deref() == Some("progress") {
            if let Some(data) = &entry.data {
                if data.get("type").and_then(|t| t.as_str()) == Some("bash_progress") {
                    let key = entry.parent_tool_use_id.clone()
                        .or_else(|| entry.uuid.clone())
                        .unwrap_or_else(|| i.to_string());
                    bash_progress_seen.entry(key).or_insert(i);
                }
            }
        }
    }
    let kept_bash_indices: std::collections::HashSet<usize> = bash_progress_seen.values().cloned().collect();

    let mut by_date: BTreeMap<String, Vec<&SessionLogEntry>> = BTreeMap::new();
    let mut seen_content: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (i, entry) in entries.iter().enumerate() {
        let is_bash_progress = entry.entry_type.as_deref() == Some("progress")
            && entry.data.as_ref().and_then(|d| d.get("type")).and_then(|t| t.as_str()) == Some("bash_progress");
        if is_bash_progress && !kept_bash_indices.contains(&i) {
            continue;
        }
        // Deduplicate identical content (e.g. same message sent to multiple agents)
        if let Some(display) = entry.get_display() {
            let content_key = format!("{}:{}", entry.entry_type.as_deref().unwrap_or(""), display);
            if !seen_content.insert(content_key) {
                continue;
            }
        }
        let date = format_date(entry.get_timestamp_millis());
        by_date.entry(date).or_default().push(entry);
    }

    for (date, date_entries) in by_date.iter().rev() {
        markdown.push_str(&format!("## {}\n\n", date));

        for entry in date_entries.iter().rev() {
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

fn main() -> Result<()> {
    let projects_dir = expand_home("~/.claude/projects");
    let obsidian_vault = expand_home("~/Documents/Obsidian/claude-code-sessions");

    if !projects_dir.exists() {
        anyhow::bail!("Projects directory not found: {:?}", projects_dir);
    }

    if !obsidian_vault.exists() {
        fs::create_dir_all(&obsidian_vault)
            .with_context(|| format!("Failed to create {:?}", obsidian_vault))?;
        println!("Created Obsidian vault directory: {:?}", obsidian_vault);
    }

    let metadata = load_metadata(&obsidian_vault);
    println!("Last synced: {} entries at timestamp {}", metadata.total_entries_synced, metadata.last_synced_timestamp);

    println!("Reading session logs from: {:?}", projects_dir);
    let new_entries = read_jsonl_files(&projects_dir, metadata.last_synced_timestamp)?;
    println!("Loaded {} new session entries", new_entries.len());

    if new_entries.is_empty() {
        println!("[OK] No new entries to sync");
        return Ok(());
    }

    let max_timestamp = new_entries.iter()
        .map(|e| e.get_timestamp_millis())
        .max()
        .unwrap_or(metadata.last_synced_timestamp);

    let sessions_dir = obsidian_vault.join("sessions");
    if !sessions_dir.exists() {
        fs::create_dir_all(&sessions_dir)?;
    }

    println!("Converting and saving session files...");

    // For sessions that already have output files, re-read their full history
    let new_session_ids: std::collections::HashSet<String> = new_entries.iter()
        .filter_map(|e| e.session_id.clone())
        .collect();
    let sessions_needing_reread: std::collections::HashSet<String> = new_session_ids.iter()
        .filter(|id| sessions_dir.join(format!("{}.md", id.replace("/", "_"))).exists())
        .cloned()
        .collect();

    let mut all_entries = new_entries.clone();
    if !sessions_needing_reread.is_empty() {
        println!("Re-reading full history for {} existing sessions...", sessions_needing_reread.len());
        let old_entries = read_jsonl_files_for_sessions(&projects_dir, &sessions_needing_reread)?;
        // Merge: deduplicate by uuid
        let existing_uuids: std::collections::HashSet<String> = all_entries.iter()
            .filter_map(|e| e.uuid.clone())
            .collect();
        for entry in old_entries {
            let is_duplicate = entry.uuid.as_ref()
                .map(|u| existing_uuids.contains(u))
                .unwrap_or(false);
            if !is_duplicate {
                all_entries.push(entry);
            }
        }
        all_entries.sort_by(|a, b| b.get_timestamp_millis().cmp(&a.get_timestamp_millis()));
    }

    let grouped = group_entries_by_session(all_entries);
    let mut index_lines = vec![
        "# Claude Code Sessions Index".to_string(),
        "".to_string(),
        format!("Generated: {}", chrono::Local::now()),
        "".to_string(),
        "## Sessions".to_string(),
        "".to_string(),
    ];

    let mut total_files = 0;
    for (session_id, entries) in grouped.iter() {
        let markdown = convert_session_to_markdown(entries);
        let safe_session_id = session_id.replace("/", "_");
        let session_file = sessions_dir.join(format!("{}.md", safe_session_id));
        fs::write(&session_file, &markdown)?;
        total_files += 1;

        let project = entries
            .first()
            .and_then(|e| e.project.as_deref().or(e.cwd.as_deref()))
            .map(get_project_name)
            .unwrap_or_else(|| "unknown".to_string());
        let project = project.as_str();
        index_lines.push(format!("- [{}]({}) - {}", session_id, format!("sessions/{}.md", safe_session_id), project));
    }

    let index_content = index_lines.join("\n");
    let index_file = obsidian_vault.join("README.md");
    fs::write(&index_file, &index_content)?;

    let updated_metadata = Metadata {
        last_synced_timestamp: max_timestamp,
        total_entries_synced: metadata.total_entries_synced + new_entries.len(),
    };
    save_metadata(&obsidian_vault, &updated_metadata)?;

    println!("[OK] Synced to {:?}", sessions_dir);
    println!("  New sessions: {}", total_files);
    println!("  New entries: {}", new_entries.len());
    println!("  Total synced: {}", updated_metadata.total_entries_synced);
    println!("[OK] Index created: {:?}", index_file);

    Ok(())
}
