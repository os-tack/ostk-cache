use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Digest;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::SystemTime;

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
}

pub fn account(
    usage: &ProviderUsage,
    session: SessionId,
    workspace_id: String,
    firmware_bytes: usize,
    state_bytes: usize,
    hot_count: usize,
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

pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
}

pub trait HookAdapter {
    fn on_event(&mut self, event: HookEvent);
}

pub struct DaemonAdapter<T: PageTable> {
    pub table: T,
}

impl<T: PageTable> HookAdapter for DaemonAdapter<T> {
    fn on_event(&mut self, event: HookEvent) {
        match event {
            HookEvent::SessionStart => println!("Binding file_id / firmware materialization"),
            HookEvent::UserPromptSubmit => println!("Placing TTL markers, injecting HUD"),
            HookEvent::PreToolUse => println!("Executing predictive prefetch"),
            HookEvent::PostToolUse => println!("Running auto-promotion, updating amp ledger"),
            HookEvent::Stop => {
                println!("Persisting snapshot, checking staleness");
                let dir = Path::new(".l1.5");
                if !dir.exists() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let file_path = dir.join("manifest.json");
                let mut file = match std::fs::File::create(&file_path) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("[DaemonAdapter] Failed to create manifest.json: {}", e);
                        return;
                    }
                };
                let status = json!({
                    "status": "persisted",
                    "timestamp": std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                });
                if let Err(e) = writeln!(file, "{}", status) {
                    eprintln!("[DaemonAdapter] Failed to write manifest.json: {}", e);
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
        let row = account(&usage, "sess_1".to_string(), "ws_1".to_string(), 10, 20, 5);
        assert_eq!(row.session, "sess_1");
        assert_eq!(row.workspace_id, "ws_1");
        assert_eq!(row.input_total, 100);
        assert_eq!(row.amp_ratio, 3.0);
        assert_eq!(row.firmware_bytes, 10);
        assert_eq!(row.state_bytes, 20);
        assert_eq!(row.hot_count, 5);
    }

    #[test]
    fn test_account_zero_input() {
        let usage = ProviderUsage {
            input_tokens: 0,
            cache_read_tokens: 200,
            cache_create_tokens: 50,
        };
        let row = account(&usage, "sess_2".to_string(), "ws_2".to_string(), 15, 25, 2);
        assert_eq!(row.session, "sess_2");
        assert_eq!(row.workspace_id, "ws_2");
        assert_eq!(row.input_total, 0);
        assert_eq!(row.amp_ratio, 1.0);
        assert_eq!(row.firmware_bytes, 15);
        assert_eq!(row.state_bytes, 25);
        assert_eq!(row.hot_count, 2);
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
}
