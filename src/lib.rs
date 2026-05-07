pub mod rewrite_middleware;

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Digest;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::SystemTime;

/// Rotation interval for `.l1.5/hooks.jsonl`: 1 hour.
pub const HOOKS_ROTATION_INTERVAL_SECS: u64 = 3600;

/// Rotate `<dir>/hooks.jsonl` to `<dir>/hooks-<created_ts>.jsonl.gz` if it has
/// existed for at least `HOOKS_ROTATION_INTERVAL_SECS`. Tracks creation via a
/// sidecar `.hooks-rotation-stamp` file. No-op if the stamp is missing (first
/// run — initializes), if the elapsed window has not yet passed, or if the
/// log is empty.
///
/// Rotation truncates the live log in place (preserving the inode for
/// subsequent append-mode opens). Old archives accumulate; cleanup is
/// out-of-scope.
pub fn maybe_rotate_hooks_jsonl(dir: &Path) -> std::io::Result<()> {
    let stamp_path = dir.join(".hooks-rotation-stamp");
    let log_path = dir.join("hooks.jsonl");
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let created = match std::fs::read_to_string(&stamp_path) {
        Ok(s) => s.trim().parse::<u64>().unwrap_or(now),
        Err(_) => {
            // No stamp yet — first event for this dir. Record now and skip.
            std::fs::write(&stamp_path, now.to_string())?;
            return Ok(());
        }
    };

    if now < created.saturating_add(HOOKS_ROTATION_INTERVAL_SECS) {
        return Ok(());
    }

    if log_path.exists() {
        let metadata = std::fs::metadata(&log_path)?;
        if metadata.len() > 0 {
            let archive_path = dir.join(format!("hooks-{}.jsonl.gz", created));
            let mut input = std::fs::File::open(&log_path)?;
            let output = std::fs::File::create(&archive_path)?;
            let mut encoder = flate2::write::GzEncoder::new(
                output,
                flate2::Compression::default(),
            );
            std::io::copy(&mut input, &mut encoder)?;
            encoder.finish()?;
            // Truncate (preserves inode for append-mode opens).
            std::fs::write(&log_path, "")?;
        }
    }

    std::fs::write(&stamp_path, now.to_string())?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AnthropicRequest {
    pub system: Option<serde_json::Value>,
    pub messages: Vec<Message>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Message {
    pub role: String,
    pub content: serde_json::Value,
}

pub type PageName = String;
pub type WorkspaceId = String;
pub type FileId = String;
pub type SessionId = String;

#[derive(Debug, PartialEq, Clone)]
pub enum WorkspaceSource {
    Explicit,
    GitOrigin,
    Cwd,
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub priority_hash: String,
    pub source: WorkspaceSource,
}

impl Workspace {
    pub fn from_path(p: &Path) -> std::io::Result<Workspace> {
        let marker = p.join(".l1.5").join("workspace-id");
        if let Ok(content) = std::fs::read(&marker) {
            return Ok(Workspace {
                priority_hash: sha256_hex(&content),
                source: WorkspaceSource::Explicit,
            });
        }

        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(p)
            .args(["config", "--get", "remote.origin.url"])
            .output();
        if let Ok(out) = output
            && out.status.success() {
                let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !raw.is_empty() {
                    let normalized = normalize_git_url(&raw);
                    return Ok(Workspace {
                        priority_hash: sha256_hex(normalized.as_bytes()),
                        source: WorkspaceSource::GitOrigin,
                    });
                }
            }

        let canonical = p.canonicalize()?;
        let s = canonical.to_string_lossy().into_owned();
        Ok(Workspace {
            priority_hash: sha256_hex(s.as_bytes()),
            source: WorkspaceSource::Cwd,
        })
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn normalize_git_url(raw: &str) -> String {
    let s = raw.trim();
    let normalized = if let Some(rest) = s.strip_prefix("git@") {
        if let Some((host, path)) = rest.split_once(':') {
            format!("https://{}/{}", host, path)
        } else {
            s.to_string()
        }
    } else {
        s.to_string()
    };
    normalized
        .strip_suffix(".git")
        .map(|s| s.to_string())
        .unwrap_or(normalized)
}

#[derive(Clone, Debug, PartialEq)]
pub enum PageState {
    Hot,
    Warm,
    Cold,
    Evicted,
}

#[derive(Clone, Debug)]
pub struct Page {
    pub name: PageName,
    pub content_hash: String,
    pub file_id: Option<FileId>,
    pub token_count: usize,
    pub last_used: SystemTime,
    pub state: PageState,
    pub pinned: bool,
}

pub trait PageTable {
    fn store(
        &mut self,
        name: PageName,
        content: &[u8],
        ws: WorkspaceId,
    ) -> impl std::future::Future<Output = Page> + Send;
    fn load(&mut self, name: PageName, ws: WorkspaceId) -> Option<Page>;
    fn pin(&mut self, name: PageName, ws: WorkspaceId);
    fn evict(&mut self, name: PageName, ws: WorkspaceId);
    fn release(&mut self, name: PageName, ws: WorkspaceId);
    fn restore(&mut self, name: PageName, ws: WorkspaceId) -> Option<Page>;
    fn list(&self, ws: &WorkspaceId, state: Option<PageState>) -> Vec<Page>;
}

pub struct InMemoryPageTable {
    pages: HashMap<(WorkspaceId, PageName), Page>,
}

impl InMemoryPageTable {
    pub fn new() -> Self {
        Self {
            pages: HashMap::new(),
        }
    }
}

impl Default for InMemoryPageTable {
    fn default() -> Self {
        Self::new()
    }
}

impl PageTable for InMemoryPageTable {
    async fn store(&mut self, name: PageName, content: &[u8], ws: WorkspaceId) -> Page {
        let content_clone = content.to_vec();
        let ws_clone = ws.clone();

        tokio::spawn(async move {
            let _file_id = materialize(&content_clone, &ws_clone).await;
        });

        let mut hasher = sha2::Sha256::new();
        hasher.update(content);
        let content_hash = format!("{:x}", hasher.finalize());
        let page = Page {
            name: name.clone(),
            content_hash,
            file_id: None,
            token_count: content.len() / 4,
            last_used: SystemTime::now(),
            state: PageState::Hot,
            pinned: false,
        };
        self.pages.insert((ws, name), page.clone());
        page
    }

    fn load(&mut self, name: PageName, ws: WorkspaceId) -> Option<Page> {
        if let Some(page) = self.pages.get_mut(&(ws, name)) {
            page.last_used = SystemTime::now();
            Some(page.clone())
        } else {
            None
        }
    }

    fn pin(&mut self, name: PageName, ws: WorkspaceId) {
        if let Some(page) = self.pages.get_mut(&(ws, name)) {
            page.pinned = true;
        }
    }

    fn evict(&mut self, name: PageName, ws: WorkspaceId) {
        self.pages.remove(&(ws, name));
    }

    fn release(&mut self, name: PageName, ws: WorkspaceId) {
        if let Some(page) = self.pages.get_mut(&(ws, name)) {
            page.state = PageState::Warm;
        }
    }

    fn restore(&mut self, name: PageName, ws: WorkspaceId) -> Option<Page> {
        if let Some(page) = self.pages.get_mut(&(ws, name)) {
            page.state = PageState::Hot;
            Some(page.clone())
        } else {
            None
        }
    }

    fn list(&self, ws: &WorkspaceId, state: Option<PageState>) -> Vec<Page> {
        self.pages
            .iter()
            .filter(|((w, _), p)| w == ws && state.as_ref().is_none_or(|s| &p.state == s))
            .map(|(_, p)| p.clone())
            .collect()
    }
}

pub fn render_partition(history: &[&str]) -> (String, String) {
    if history.is_empty() {
        return (String::new(), String::new());
    }
    if history.len() == 1 {
        let prompt = history[0];
        let len = prompt.len();
        let threshold = len - (len / 4);

        if let Some(idx) = prompt[..threshold].rfind('\n') {
            let firmware = prompt[..idx].to_string();
            let state = prompt[idx + 1..].to_string();
            return (firmware, state);
        } else {
            return (String::new(), prompt.to_string());
        }
    }

    let first = history[0];
    let mut lcp_len = first.len();

    for s in &history[1..] {
        let mut current_match = 0;
        for (c1, c2) in first.chars().zip(s.chars()) {
            if c1 == c2 {
                current_match += c1.len_utf8();
            } else {
                break;
            }
        }
        if current_match < lcp_len {
            lcp_len = current_match;
        }
    }

    let firmware = first[..lcp_len].to_string();
    let last = history.last().unwrap();
    let state = last[lcp_len..].to_string();

    (firmware, state)
}

pub fn project_hud(amp: f64, stored: usize, hot: usize) -> String {
    format!(
        "cache: 5m=- 1h=- amp={:.1}x stored={} hot={}",
        amp, stored, hot
    )
}

pub async fn materialize(content: &[u8], _ws: &WorkspaceId) -> FileId {
    let mut hasher = sha2::Sha256::new();
    hasher.update(content);
    let hash = format!("{:x}", hasher.finalize());
    let fallback = format!("file_{}", hash);

    if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
        let client = reqwest::Client::new();
        let part = reqwest::multipart::Part::bytes(content.to_vec()).file_name("file");
        let form = reqwest::multipart::Form::new().part("file", part);

        let res = client
            .post("https://api.anthropic.com/v1/files")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .multipart(form)
            .send()
            .await;

        if let Ok(response) = res {
            if response.status().is_success() {
                if let Ok(json) = response.json::<serde_json::Value>().await
                    && let Some(id) = json.get("id").and_then(|v| v.as_str()) {
                        return id.to_string();
                    }
            } else {
                eprintln!("[proxy] materialize failed: HTTP {}", response.status());
            }
        } else if let Err(e) = res {
            eprintln!("[proxy] materialize request error: {}", e);
        }
    } else {
        eprintln!("[proxy] ANTHROPIC_API_KEY missing, using fallback hash for materialize");
    }

    fallback
}

pub struct ProviderUsage {
    pub input_tokens: usize,
    pub cache_read_tokens: usize,
    pub cache_create_tokens: usize,
}

/// Default value for `AmpRow.mode` when deserializing rows persisted before
/// the passthrough A/B mode was introduced. Old rows always reflect the
/// (then-only) mutate path.
pub fn default_amp_mode() -> String {
    "mutate".to_string()
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct AmpRow {
    pub session: SessionId,
    #[serde(default)]
    pub workspace_id: String,
    pub input_total: usize,
    #[serde(default)]
    pub cache_read_tokens: usize,
    #[serde(default)]
    pub cache_create_tokens: usize,
    pub amp_ratio: f64,
    #[serde(default)]
    pub firmware_bytes: usize,
    #[serde(default)]
    pub state_bytes: usize,
    #[serde(default)]
    pub hot_count: usize,
    #[serde(default)]
    pub timestamp: u64,
    /// "mutate" (proxy rewrote the request) or "passthrough" (proxy
    /// forwarded byte-identical, accounting only). Older ledger rows
    /// without this field default to "mutate" via `default_amp_mode`.
    #[serde(default = "default_amp_mode")]
    pub mode: String,
}

pub fn account(
    usage: &ProviderUsage,
    session: SessionId,
    workspace_id: String,
    firmware_bytes: usize,
    state_bytes: usize,
    hot_count: usize,
    mode: &str,
) -> AmpRow {
    let amp_ratio = if usage.input_tokens == 0 {
        1.0
    } else {
        (usage.cache_read_tokens + usage.input_tokens) as f64 / usage.input_tokens as f64
    };

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    AmpRow {
        session,
        workspace_id,
        input_total: usage.input_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_create_tokens: usage.cache_create_tokens,
        amp_ratio,
        firmware_bytes,
        state_bytes,
        hot_count,
        timestamp,
        mode: mode.to_string(),
    }
}

pub fn persist_amp_row(row: &AmpRow) -> std::io::Result<()> {
    let dir = Path::new(".ostk/memory");
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
    }
    let file_path = dir.join("ledger.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(file_path)?;

    let json = serde_json::to_string(row)?;
    writeln!(file, "{}", json)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum HookEventKind {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
}

impl HookEventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            HookEventKind::SessionStart => "SessionStart",
            HookEventKind::UserPromptSubmit => "UserPromptSubmit",
            HookEventKind::PreToolUse => "PreToolUse",
            HookEventKind::PostToolUse => "PostToolUse",
            HookEventKind::Stop => "Stop",
        }
    }

    pub fn parse_str(s: &str) -> Option<HookEventKind> {
        match s {
            "SessionStart" => Some(HookEventKind::SessionStart),
            "UserPromptSubmit" => Some(HookEventKind::UserPromptSubmit),
            "PreToolUse" => Some(HookEventKind::PreToolUse),
            "PostToolUse" => Some(HookEventKind::PostToolUse),
            "Stop" => Some(HookEventKind::Stop),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HookEvent {
    pub kind: HookEventKind,
    pub workspace_id: String,
    pub session_id: String,
    pub payload: Option<serde_json::Value>,
}

impl HookEvent {
    pub fn new(kind: HookEventKind, workspace_id: String, session_id: String) -> Self {
        Self {
            kind,
            workspace_id,
            session_id,
            payload: None,
        }
    }

    pub fn with_payload(mut self, payload: serde_json::Value) -> Self {
        self.payload = Some(payload);
        self
    }
}

pub trait HookAdapter {
    fn on_event(&mut self, event: HookEvent);
}

pub struct DaemonAdapter<T: PageTable> {
    pub table: T,
}

impl<T: PageTable> DaemonAdapter<T> {
    pub fn new(table: T) -> Self {
        Self { table }
    }
}

impl<T: PageTable> HookAdapter for DaemonAdapter<T> {
    fn on_event(&mut self, event: HookEvent) {
        // Terminal observation lines (preserved from before).
        match event.kind {
            HookEventKind::SessionStart => {
                println!("Binding file_id / firmware materialization")
            }
            HookEventKind::UserPromptSubmit => {
                println!("Placing TTL markers, injecting HUD")
            }
            HookEventKind::PreToolUse => println!("Executing predictive prefetch"),
            HookEventKind::PostToolUse => {
                println!("Running auto-promotion, updating amp ledger")
            }
            HookEventKind::Stop => println!("Persisting snapshot, checking staleness"),
        }

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let dir = Path::new(".l1.5");
        if !dir.exists()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            eprintln!("[DaemonAdapter] Failed to create .l1.5 dir: {}", e);
            return;
        }

        // Rotate hooks.jsonl hourly. Errors here are non-fatal; never lose
        // an event row over a rotation hiccup.
        if let Err(e) = maybe_rotate_hooks_jsonl(dir) {
            eprintln!(
                "[DaemonAdapter] hooks.jsonl rotation failed (continuing): {}",
                e
            );
        }

        // Append a row to .l1.5/hooks.jsonl for every event kind.
        let row = json!({
            "timestamp": timestamp,
            "workspace_id": event.workspace_id,
            "session_id": event.session_id,
            "event_type": event.kind.as_str(),
            "payload": event.payload.clone().unwrap_or(serde_json::Value::Null),
        });

        let hooks_path = dir.join("hooks.jsonl");
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&hooks_path)
        {
            Ok(mut f) => {
                if let Err(e) = writeln!(f, "{}", row) {
                    eprintln!(
                        "[DaemonAdapter] Failed to write hooks.jsonl: {}",
                        e
                    );
                }
            }
            Err(e) => {
                eprintln!("[DaemonAdapter] Failed to open hooks.jsonl: {}", e);
            }
        }

        // Stop additionally writes a manifest snapshot (kept for compatibility).
        if event.kind == HookEventKind::Stop {
            let file_path = dir.join("manifest.json");
            match std::fs::File::create(&file_path) {
                Ok(mut file) => {
                    let status = json!({
                        "status": "persisted",
                        "timestamp": timestamp,
                        "workspace_id": event.workspace_id,
                        "session_id": event.session_id,
                    });
                    if let Err(e) = writeln!(file, "{}", status) {
                        eprintln!(
                            "[DaemonAdapter] Failed to write manifest.json: {}",
                            e
                        );
                    }
                }
                Err(e) => {
                    eprintln!(
                        "[DaemonAdapter] Failed to create manifest.json: {}",
                        e
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_store_and_load_flow() {
        let mut table = InMemoryPageTable::new();
        let page = table
            .store("firmware".to_string(), b"sys_prompt", "ws1".to_string())
            .await;
        assert_eq!(page.state, PageState::Hot);
        assert!(page.file_id.is_none());

        let loaded = table
            .load("firmware".to_string(), "ws1".to_string())
            .unwrap();
        assert_eq!(loaded.content_hash, page.content_hash);

        table.release("firmware".to_string(), "ws1".to_string());
        let released = table
            .load("firmware".to_string(), "ws1".to_string())
            .unwrap();
        assert_eq!(released.state, PageState::Warm);
    }

    #[tokio::test]
    async fn test_list() {
        let mut table = InMemoryPageTable::new();
        table
            .store("p1".to_string(), b"c1", "ws1".to_string())
            .await;
        table
            .store("p2".to_string(), b"c2", "ws1".to_string())
            .await;
        table
            .store("p3".to_string(), b"c3", "ws2".to_string())
            .await;

        table.release("p2".to_string(), "ws1".to_string());

        let all_ws1 = table.list(&"ws1".to_string(), None);
        assert_eq!(all_ws1.len(), 2);

        let hot_ws1 = table.list(&"ws1".to_string(), Some(PageState::Hot));
        assert_eq!(hot_ws1.len(), 1);
        assert_eq!(hot_ws1[0].name, "p1");

        let warm_ws1 = table.list(&"ws1".to_string(), Some(PageState::Warm));
        assert_eq!(warm_ws1.len(), 1);
        assert_eq!(warm_ws1[0].name, "p2");
    }

    #[test]
    fn test_render_partition() {
        let prompt = "line1\nline2\nline3\nline4";
        let (firmware, state) = render_partition(&[prompt]);
        assert_eq!(firmware, "line1\nline2\nline3");
        assert_eq!(state, "line4");

        let prompt2 = "short";
        let (f2, s2) = render_partition(&[prompt2]);
        assert_eq!(f2, "");
        assert_eq!(s2, "short");
    }

    #[test]
    fn test_render_partition_lcp() {
        let h1 = "System prompt\nUser: hello\nAssistant: hi";
        let h2 = "System prompt\nUser: hello\nAssistant: hi\nUser: how are you?";
        let (firmware, state) = render_partition(&[h1, h2]);
        assert_eq!(firmware, "System prompt\nUser: hello\nAssistant: hi");
        assert_eq!(state, "\nUser: how are you?");
    }

    #[test]
    fn test_project_hud() {
        let hud = project_hud(2.5, 10, 3);
        assert_eq!(hud, "cache: 5m=- 1h=- amp=2.5x stored=10 hot=3");
    }

    #[test]
    fn test_account_normal() {
        let usage = ProviderUsage {
            input_tokens: 100,
            cache_read_tokens: 200,
            cache_create_tokens: 50,
        };
        let row = account(
            &usage,
            "sess_1".to_string(),
            "ws_1".to_string(),
            10,
            20,
            5,
            "mutate",
        );
        assert_eq!(row.session, "sess_1");
        assert_eq!(row.workspace_id, "ws_1");
        assert_eq!(row.input_total, 100);
        assert_eq!(row.amp_ratio, 3.0);
        assert_eq!(row.firmware_bytes, 10);
        assert_eq!(row.state_bytes, 20);
        assert_eq!(row.hot_count, 5);
        assert_eq!(row.mode, "mutate");
    }

    #[test]
    fn test_account_zero_input() {
        let usage = ProviderUsage {
            input_tokens: 0,
            cache_read_tokens: 200,
            cache_create_tokens: 50,
        };
        let row = account(
            &usage,
            "sess_2".to_string(),
            "ws_2".to_string(),
            15,
            25,
            2,
            "passthrough",
        );
        assert_eq!(row.session, "sess_2");
        assert_eq!(row.workspace_id, "ws_2");
        assert_eq!(row.input_total, 0);
        assert_eq!(row.amp_ratio, 1.0);
        assert_eq!(row.firmware_bytes, 15);
        assert_eq!(row.state_bytes, 25);
        assert_eq!(row.hot_count, 2);
        assert_eq!(row.mode, "passthrough");
    }

    #[test]
    fn test_amp_row_legacy_row_defaults_to_mutate() {
        // Old rows persisted before the `mode` field existed must still
        // parse, with mode defaulting to "mutate" so --mode mutate sees
        // them in stats analysis.
        let legacy = r#"{
            "session": "sess_legacy",
            "workspace_id": "ws_legacy",
            "input_total": 10,
            "amp_ratio": 1.0
        }"#;
        let row: AmpRow = serde_json::from_str(legacy).expect("legacy row parses");
        assert_eq!(row.mode, "mutate");
        assert_eq!(row.session, "sess_legacy");
    }

    fn git_init(dir: &Path) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .arg("init")
            .arg("-q")
            .output()
            .expect("git init");
        assert!(out.status.success(), "git init failed: {:?}", out);
    }

    fn git_set_origin(dir: &Path, url: &str) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["remote", "add", "origin", url])
            .output()
            .expect("git remote add");
        assert!(out.status.success(), "git remote add failed: {:?}", out);
    }

    #[test]
    fn test_workspace_explicit_overrides_git() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        git_set_origin(dir.path(), "https://github.com/foo/bar.git");

        let marker_dir = dir.path().join(".l1.5");
        std::fs::create_dir_all(&marker_dir).unwrap();
        std::fs::write(marker_dir.join("workspace-id"), b"abc").unwrap();

        let ws = Workspace::from_path(dir.path()).unwrap();
        assert_eq!(ws.source, WorkspaceSource::Explicit);
        assert_eq!(ws.priority_hash, sha256_hex(b"abc"));
    }

    #[test]
    fn test_workspace_git_overrides_cwd() {
        let dir = tempfile::tempdir().unwrap();
        git_init(dir.path());
        git_set_origin(dir.path(), "https://github.com/foo/bar.git");

        let ws = Workspace::from_path(dir.path()).unwrap();
        assert_eq!(ws.source, WorkspaceSource::GitOrigin);
        assert_eq!(ws.priority_hash, sha256_hex(b"https://github.com/foo/bar"));
    }

    #[test]
    fn test_workspace_two_clones_same_hash() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        git_init(dir_a.path());
        git_init(dir_b.path());
        git_set_origin(dir_a.path(), "git@github.com:foo/bar.git");
        git_set_origin(dir_b.path(), "https://github.com/foo/bar.git");

        let ws_a = Workspace::from_path(dir_a.path()).unwrap();
        let ws_b = Workspace::from_path(dir_b.path()).unwrap();
        assert_eq!(ws_a.source, WorkspaceSource::GitOrigin);
        assert_eq!(ws_b.source, WorkspaceSource::GitOrigin);
        assert_eq!(ws_a.priority_hash, ws_b.priority_hash);
    }

    #[test]
    fn test_workspace_different_repos_different_hashes() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        git_init(dir_a.path());
        git_init(dir_b.path());
        git_set_origin(dir_a.path(), "https://github.com/foo/bar.git");
        git_set_origin(dir_b.path(), "https://github.com/baz/qux.git");

        let ws_a = Workspace::from_path(dir_a.path()).unwrap();
        let ws_b = Workspace::from_path(dir_b.path()).unwrap();
        assert_ne!(ws_a.priority_hash, ws_b.priority_hash);
    }

    #[test]
    fn test_workspace_cwd_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::from_path(dir.path()).unwrap();
        assert_eq!(ws.source, WorkspaceSource::Cwd);
        let canonical = dir.path().canonicalize().unwrap();
        let expected = sha256_hex(canonical.to_string_lossy().as_bytes());
        assert_eq!(ws.priority_hash, expected);
    }

    /// Helper: run `f` inside a temp cwd, restoring the previous cwd on drop.
    /// We serialize tests that touch cwd via a Mutex because `set_current_dir`
    /// is process-wide.
    fn with_temp_cwd<F: FnOnce(&Path)>(f: F) {
        use std::sync::Mutex;
        static CWD_LOCK: Mutex<()> = Mutex::new(());
        let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::current_dir().expect("cwd");
        let tmp = tempfile::tempdir().expect("tempdir");
        std::env::set_current_dir(tmp.path()).expect("cd");
        f(tmp.path());
        std::env::set_current_dir(&prev).expect("restore cwd");
    }

    fn read_hooks_jsonl(dir: &Path) -> Vec<serde_json::Value> {
        let path = dir.join(".l1.5").join("hooks.jsonl");
        let contents = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {:?}: {}", path, e));
        contents
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .unwrap_or_else(|e| panic!("parse {:?}: {}", l, e))
            })
            .collect()
    }

    fn dispatch_each_kind(adapter: &mut DaemonAdapter<InMemoryPageTable>) {
        for kind in [
            HookEventKind::SessionStart,
            HookEventKind::UserPromptSubmit,
            HookEventKind::PreToolUse,
            HookEventKind::PostToolUse,
            HookEventKind::Stop,
        ] {
            adapter.on_event(HookEvent::new(
                kind,
                "ws-test".to_string(),
                "sess-test".to_string(),
            ));
        }
    }

    #[test]
    fn test_daemon_adapter_writes_row_for_each_event_kind() {
        with_temp_cwd(|dir| {
            let mut adapter = DaemonAdapter::new(InMemoryPageTable::new());
            dispatch_each_kind(&mut adapter);

            let rows = read_hooks_jsonl(dir);
            assert_eq!(rows.len(), 5, "expected one row per event kind");

            let kinds: Vec<&str> = rows
                .iter()
                .map(|r| r["event_type"].as_str().expect("event_type str"))
                .collect();
            assert_eq!(
                kinds,
                vec![
                    "SessionStart",
                    "UserPromptSubmit",
                    "PreToolUse",
                    "PostToolUse",
                    "Stop",
                ]
            );

            for r in &rows {
                assert_eq!(r["workspace_id"].as_str().unwrap(), "ws-test");
                assert_eq!(r["session_id"].as_str().unwrap(), "sess-test");
                assert!(r["timestamp"].as_u64().is_some(), "timestamp must be u64");
                // payload defaults to null when not provided
                assert!(r["payload"].is_null());
            }
        });
    }

    #[test]
    fn test_daemon_adapter_stop_writes_manifest_and_hook_row() {
        with_temp_cwd(|dir| {
            let mut adapter = DaemonAdapter::new(InMemoryPageTable::new());
            adapter.on_event(HookEvent::new(
                HookEventKind::Stop,
                "ws-stop".to_string(),
                "sess-stop".to_string(),
            ));

            // hooks.jsonl has the Stop row
            let rows = read_hooks_jsonl(dir);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["event_type"].as_str().unwrap(), "Stop");

            // manifest.json still produced
            let manifest_path = dir.join(".l1.5").join("manifest.json");
            let manifest_text = std::fs::read_to_string(&manifest_path)
                .unwrap_or_else(|e| panic!("read {:?}: {}", manifest_path, e));
            let manifest: serde_json::Value =
                serde_json::from_str(manifest_text.trim()).expect("manifest json");
            assert_eq!(manifest["status"].as_str().unwrap(), "persisted");
            assert_eq!(manifest["workspace_id"].as_str().unwrap(), "ws-stop");
            assert_eq!(manifest["session_id"].as_str().unwrap(), "sess-stop");
        });
    }

    #[test]
    fn test_daemon_adapter_appends_payload() {
        with_temp_cwd(|dir| {
            let mut adapter = DaemonAdapter::new(InMemoryPageTable::new());
            let payload = json!({"tool": "Read", "args": {"path": "/x"}});
            adapter.on_event(
                HookEvent::new(
                    HookEventKind::PreToolUse,
                    "ws-p".to_string(),
                    "sess-p".to_string(),
                )
                .with_payload(payload.clone()),
            );

            let rows = read_hooks_jsonl(dir);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["event_type"].as_str().unwrap(), "PreToolUse");
            assert_eq!(rows[0]["payload"], payload);
        });
    }

    #[test]
    fn test_hook_event_kind_str_roundtrip() {
        for kind in [
            HookEventKind::SessionStart,
            HookEventKind::UserPromptSubmit,
            HookEventKind::PreToolUse,
            HookEventKind::PostToolUse,
            HookEventKind::Stop,
        ] {
            assert_eq!(HookEventKind::parse_str(kind.as_str()), Some(kind));
        }
        assert_eq!(HookEventKind::parse_str("Unknown"), None);
    }

    #[test]
    fn rotation_initializes_stamp_on_first_call() {
        let dir = tempfile::tempdir().unwrap();
        // No stamp, no log yet. First call records stamp, doesn't archive.
        maybe_rotate_hooks_jsonl(dir.path()).unwrap();
        let stamp = dir.path().join(".hooks-rotation-stamp");
        assert!(stamp.exists(), "stamp file should be created on first call");
        let archives: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("hooks-"))
            .collect();
        assert!(archives.is_empty(), "no archive should exist on first call");
    }

    #[test]
    fn rotation_no_op_within_window() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = dir.path().join(".hooks-rotation-stamp");
        let log = dir.path().join("hooks.jsonl");

        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        std::fs::write(&stamp, now.to_string()).unwrap();
        std::fs::write(&log, "row1\nrow2\n").unwrap();

        maybe_rotate_hooks_jsonl(dir.path()).unwrap();
        let log_content = std::fs::read_to_string(&log).unwrap();
        assert_eq!(log_content, "row1\nrow2\n", "log untouched within window");
        let archives: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl.gz"))
            .collect();
        assert!(archives.is_empty(), "no archive within window");
    }

    #[test]
    fn rotation_archives_when_window_exceeded() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = dir.path().join(".hooks-rotation-stamp");
        let log = dir.path().join("hooks.jsonl");

        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let past = now - HOOKS_ROTATION_INTERVAL_SECS - 60; // > 1h ago
        std::fs::write(&stamp, past.to_string()).unwrap();
        std::fs::write(&log, "row1\nrow2\nrow3\n").unwrap();

        maybe_rotate_hooks_jsonl(dir.path()).unwrap();

        // Live log truncated.
        let log_content = std::fs::read_to_string(&log).unwrap();
        assert_eq!(log_content, "", "live log truncated post-rotation");

        // Archive named by the OLD stamp value.
        let expected_archive = dir.path().join(format!("hooks-{}.jsonl.gz", past));
        assert!(
            expected_archive.exists(),
            "archive should be at {}",
            expected_archive.display()
        );

        // Archive content decompresses to original log content.
        let gz_bytes = std::fs::read(&expected_archive).unwrap();
        let mut decoder = flate2::read::GzDecoder::new(&gz_bytes[..]);
        let mut decompressed = String::new();
        std::io::Read::read_to_string(&mut decoder, &mut decompressed).unwrap();
        assert_eq!(decompressed, "row1\nrow2\nrow3\n");

        // Stamp updated to ~now.
        let new_stamp: u64 = std::fs::read_to_string(&stamp)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(new_stamp >= now, "stamp updated forward");
    }

    #[test]
    fn rotation_skips_empty_log() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = dir.path().join(".hooks-rotation-stamp");
        let log = dir.path().join("hooks.jsonl");

        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let past = now - HOOKS_ROTATION_INTERVAL_SECS - 1;
        std::fs::write(&stamp, past.to_string()).unwrap();
        std::fs::write(&log, "").unwrap(); // empty

        maybe_rotate_hooks_jsonl(dir.path()).unwrap();

        let archives: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl.gz"))
            .collect();
        assert!(archives.is_empty(), "empty log should not be archived");

        // Stamp still updated so we don't churn.
        let new_stamp: u64 = std::fs::read_to_string(&stamp)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(new_stamp >= now);
    }

    #[test]
    fn rotation_tolerates_corrupted_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let stamp = dir.path().join(".hooks-rotation-stamp");
        std::fs::write(&stamp, "garbage-not-a-number").unwrap();

        // Should not panic. Parses as `now`, which makes the next-rotation
        // window fresh.
        maybe_rotate_hooks_jsonl(dir.path()).unwrap();
    }
}
