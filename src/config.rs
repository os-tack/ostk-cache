//! Resolved runtime configuration for ostk-cache.
//!
//! ## Resolution precedence (highest wins)
//!
//! 1. CLI flags (`--mode`, `--port`, ...)
//! 2. Environment variables (legacy escape hatch — same names as before)
//! 3. Workspace TOML at `<cwd>/.ostk/cache.toml`
//! 4. Built-in defaults
//!
//! Each resolved value carries the [`Source`] it came from so the
//! `--print-config` flag can show the operator exactly where each
//! setting was resolved.

use clap::{Parser, ValueEnum};
use serde::Deserialize;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Modes (enum used by both CLI parsing and TOML)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Forward request bodies byte-identical. Accounting still runs.
    Passthrough,
    /// Legacy single-shot rewrite: 1h cache marker on system, HUD prepend
    /// on the last user message, strip user `cache_control`. Default.
    Mutate,
    /// Layer 1 standalone request rebuild — discard prior turns, replace
    /// with synthesized kernel-projection context. In-flight chain
    /// preserved.
    Rebuild,
    /// Layer 2 federated rebuild — same as `rebuild` but the live
    /// envelope is fetched from a running ostk kernel daemon over
    /// `.ostk/ostk.sock`. Falls back to `rebuild` if the kernel isn't
    /// reachable.
    RebuildKernel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    Anthropic,
    Gpt,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Provider::Anthropic => "anthropic",
            Provider::Gpt => "gpt",
        })
    }
}

impl Mode {
    /// The legacy `AmpRow.mode` tag for this mode. Stable values written
    /// to `ledger.jsonl` — do NOT rename without a back-compat plan.
    pub fn ledger_tag(self) -> &'static str {
        match self {
            Mode::Passthrough => "passthrough",
            Mode::Mutate => "mutate",
            Mode::Rebuild => "rebuild_local",
            Mode::RebuildKernel => "rebuild_kernel",
        }
    }

    pub fn is_rebuild(self) -> bool {
        matches!(self, Mode::Rebuild | Mode::RebuildKernel)
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Mode::Passthrough => "passthrough",
            Mode::Mutate => "mutate",
            Mode::Rebuild => "rebuild",
            Mode::RebuildKernel => "rebuild-kernel",
        })
    }
}

// ---------------------------------------------------------------------------
// Source (provenance for --print-config)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Default,
    Toml,
    Env,
    Cli,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Source::Default => "default",
            Source::Toml => "toml",
            Source::Env => "env",
            Source::Cli => "cli",
        })
    }
}

#[derive(Debug, Clone)]
pub struct Resolved<T> {
    pub value: T,
    pub source: Source,
}

impl<T> Resolved<T> {
    fn new(value: T, source: Source) -> Self {
        Self { value, source }
    }
}

// ---------------------------------------------------------------------------
// CLI args (clap derive)
// ---------------------------------------------------------------------------

#[derive(Parser, Debug, Clone)]
#[command(
    name = "ostk-cache",
    about = "L1.5 caching proxy for Anthropic /v1/messages and OpenAI /v1/responses APIs",
    long_about = None,
    version,
)]
pub struct CliArgs {
    /// TCP port to bind on 127.0.0.1.
    #[arg(long)]
    pub port: Option<u16>,

    /// Upstream Anthropic API base URL.
    #[arg(long)]
    pub upstream: Option<String>,

    /// Provider adapter to optimize for.
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,

    /// Proxy operation mode. Default: mutate.
    #[arg(long, value_enum)]
    pub mode: Option<Mode>,

    /// Enable cross-session transcript-tail enrichment (rebuild modes only).
    /// Default: on. Use --no-tail-transcript to disable.
    #[arg(long)]
    pub tail_transcript: bool,

    /// Disable transcript-tail enrichment (overrides env/toml).
    #[arg(long, conflicts_with = "tail_transcript")]
    pub no_tail_transcript: bool,

    /// Cap on tail events ingested per request.
    #[arg(long)]
    pub tail_limit: Option<usize>,

    /// Override the Claude Code projects directory (default ~/.claude/projects).
    #[arg(long)]
    pub claude_projects_dir: Option<PathBuf>,

    /// Override workspace .ostk dir for file-handle cache lookup.
    #[arg(long)]
    pub ostk_dir: Option<PathBuf>,

    /// Pin explicit kernel socket path (otherwise cwd-walk for `.ostk/ostk.sock`).
    #[arg(long)]
    pub kernel_socket: Option<PathBuf>,

    /// Kernel-projection IPC timeout in milliseconds.
    #[arg(long)]
    pub kernel_timeout_ms: Option<u64>,

    /// Disable file-handle rewrite pass.
    #[arg(long)]
    pub no_rewrite: bool,

    /// Soft cap on outgoing request size in MiB. 0 disables. Default: 30.
    #[arg(long)]
    pub soft_cap_mb: Option<u64>,

    /// Disable the soft cap entirely (shorthand for --soft-cap-mb 0).
    #[arg(long, conflicts_with = "soft_cap_mb")]
    pub no_soft_cap: bool,

    /// Capture full inbound request, outbound request, and upstream response bodies.
    #[arg(long)]
    pub capture_http: bool,

    /// Directory for --capture-http output. Default: <cwd>/.ostk/http-capture.
    #[arg(long)]
    pub capture_http_dir: Option<PathBuf>,

    /// Verbose per-turn logging (multi-line breakdown instead of the
    /// single `[turn ...]` line).
    #[arg(long, short = 'v')]
    pub verbose: bool,

    /// Path to a TOML config file. Default: `<cwd>/.ostk/cache.toml`.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Print resolved configuration with source attribution and exit.
    #[arg(long)]
    pub print_config: bool,
}

// ---------------------------------------------------------------------------
// TOML schema
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    port: Option<u16>,
    upstream: Option<String>,
    provider: Option<Provider>,
    mode: Option<Mode>,
    soft_cap_mb: Option<u64>,
    #[serde(default)]
    tail: TailFile,
    #[serde(default)]
    rewrite: RewriteFile,
    #[serde(default)]
    kernel: KernelFile,
    #[serde(default)]
    capture: CaptureFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TailFile {
    transcript: Option<bool>,
    limit: Option<usize>,
    claude_projects_dir: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RewriteFile {
    enabled: Option<bool>,
    ostk_dir: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct KernelFile {
    socket: Option<PathBuf>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureFile {
    http: Option<bool>,
    dir: Option<PathBuf>,
}

fn load_file_config(path: &Path) -> Option<FileConfig> {
    let raw = std::fs::read_to_string(path).ok()?;
    match toml::from_str::<FileConfig>(&raw) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            eprintln!(
                "[ostk-cache] config {}: parse error — {} (using defaults for affected fields)",
                path.display(),
                e
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Built-in defaults
// ---------------------------------------------------------------------------

pub const DEFAULT_PORT: u16 = 8080;
pub const DEFAULT_UPSTREAM: &str = "https://api.anthropic.com";
pub const DEFAULT_OPENAI_UPSTREAM: &str = "https://api.openai.com";
pub const DEFAULT_PROVIDER: Provider = Provider::Anthropic;
pub const DEFAULT_MODE: Mode = Mode::Mutate;
pub const DEFAULT_TAIL_LIMIT: usize = 50;
pub const DEFAULT_KERNEL_TIMEOUT_MS: u64 = 500;
pub const DEFAULT_SOFT_CAP_MB: u64 = 30;
pub const ANTHROPIC_HARD_LIMIT_MB: u64 = 32;

fn default_claude_projects_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".claude").join("projects")
}

// ---------------------------------------------------------------------------
// Resolved Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Config {
    pub port: Resolved<u16>,
    pub upstream: Resolved<String>,
    pub provider: Resolved<Provider>,
    pub mode: Resolved<Mode>,
    pub tail_transcript: Resolved<bool>,
    pub tail_limit: Resolved<usize>,
    pub claude_projects_dir: Resolved<PathBuf>,
    pub rewrite_enabled: Resolved<bool>,
    pub ostk_dir: Resolved<Option<PathBuf>>,
    pub kernel_socket: Resolved<Option<PathBuf>>,
    pub kernel_timeout_ms: Resolved<u64>,
    pub soft_cap_mb: Resolved<u64>,
    pub capture_http: Resolved<bool>,
    pub capture_http_dir: Resolved<PathBuf>,
    pub verbose: Resolved<bool>,
    pub config_path: Option<PathBuf>,
}

impl Config {
    /// Resolve config from the four sources. `cwd` is used to locate the
    /// default workspace TOML if `cli.config` isn't set.
    pub fn resolve(cli: CliArgs, cwd: &Path) -> Self {
        let config_path = cli.config.clone().or_else(|| {
            let default = cwd.join(".ostk").join("cache.toml");
            if default.exists() { Some(default) } else { None }
        });
        let file = config_path.as_deref().and_then(load_file_config);
        let file = file.as_ref();

        // Helper: env_var that returns Option<String>, ignoring empty values.
        let env_str = |k: &str| -> Option<String> {
            std::env::var(k).ok().filter(|s| !s.is_empty())
        };
        let env_truthy = |k: &str| -> Option<bool> {
            env_str(k).map(|v| {
                matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes")
            })
        };
        let env_num = |k: &str| -> Option<u64> { env_str(k).and_then(|s| s.parse().ok()) };

        // ---- port ------------------------------------------------------
        let port = pick_value(
            cli.port,
            env_num("PROXY_PORT").map(|n| n as u16),
            file.and_then(|f| f.port),
            DEFAULT_PORT,
        );

        // ---- provider --------------------------------------------------
        let provider = {
            let from_env = env_str("OSTK_PROVIDER").and_then(|v| {
                match v.trim().to_ascii_lowercase().as_str() {
                    "anthropic" => Some(Provider::Anthropic),
                    "gpt" | "openai" => Some(Provider::Gpt),
                    _ => None,
                }
            });
            pick_value(
                cli.provider,
                from_env,
                file.and_then(|f| f.provider),
                DEFAULT_PROVIDER,
            )
        };

        // ---- upstream --------------------------------------------------
        let upstream_env = match provider.value {
            Provider::Anthropic => env_str("ANTHROPIC_BASE_URL"),
            Provider::Gpt => env_str("OPENAI_BASE_URL").or_else(|| env_str("ANTHROPIC_BASE_URL")),
        };
        let upstream_default = match provider.value {
            Provider::Anthropic => DEFAULT_UPSTREAM,
            Provider::Gpt => DEFAULT_OPENAI_UPSTREAM,
        };
        let upstream = pick_value(
            cli.upstream.clone(),
            upstream_env,
            file.and_then(|f| f.upstream.clone()),
            upstream_default.to_string(),
        );

        // ---- mode (env reconstructs from legacy two-var combination) --
        let mode = {
            let from_env = match (
                env_truthy("OSTK_CACHE_PASSTHROUGH").unwrap_or(false),
                env_str("OSTK_CACHE_REBUILD"),
            ) {
                (true, _) => Some(Mode::Passthrough),
                (false, Some(v)) => match v.trim().to_ascii_lowercase().as_str() {
                    "1" | "true" | "yes" => Some(Mode::Rebuild),
                    "kernel" => Some(Mode::RebuildKernel),
                    "0" | "false" | "no" | "" => None,
                    _ => None,
                },
                _ => None,
            };
            pick_value(cli.mode, from_env, file.and_then(|f| f.mode), DEFAULT_MODE)
        };

        // ---- tail transcript ------------------------------------------
        let tail_cli = if cli.no_tail_transcript {
            Some(false)
        } else if cli.tail_transcript {
            Some(true)
        } else {
            None
        };
        let tail_transcript = pick_value(
            tail_cli,
            env_truthy("OSTK_CACHE_TAIL_TRANSCRIPT"),
            file.and_then(|f| f.tail.transcript),
            true,
        );

        let tail_limit = pick_value(
            cli.tail_limit,
            env_num("OSTK_CACHE_TAIL_LIMIT").map(|n| n as usize),
            file.and_then(|f| f.tail.limit),
            DEFAULT_TAIL_LIMIT,
        );

        let claude_projects_dir = pick_value(
            cli.claude_projects_dir.clone(),
            env_str("OSTK_CACHE_CLAUDE_PROJECTS_DIR").map(PathBuf::from),
            file.and_then(|f| f.tail.claude_projects_dir.clone()),
            default_claude_projects_dir(),
        );

        // ---- rewrite enabled (env defaults ON; CLI --no-rewrite forces OFF) ----
        let rewrite_cli = if cli.no_rewrite { Some(false) } else { None };
        let rewrite_enabled = pick_value(
            rewrite_cli,
            env_truthy("OSTK_REWRITE_ENABLED"),
            file.and_then(|f| f.rewrite.enabled),
            true,
        );

        let ostk_dir = pick_value_opt(
            cli.ostk_dir.clone(),
            env_str("OSTK_DIR").map(PathBuf::from),
            file.and_then(|f| f.rewrite.ostk_dir.clone()),
        );

        // ---- kernel socket / timeout ----------------------------------
        let kernel_socket = pick_value_opt(
            cli.kernel_socket.clone(),
            env_str("OSTK_KERNEL_SOCKET").map(PathBuf::from),
            file.and_then(|f| f.kernel.socket.clone()),
        );

        let kernel_timeout_ms = pick_value(
            cli.kernel_timeout_ms,
            env_num("OSTK_CACHE_KERNEL_TIMEOUT_MS"),
            file.and_then(|f| f.kernel.timeout_ms),
            DEFAULT_KERNEL_TIMEOUT_MS,
        );

        // ---- soft cap --------------------------------------------------
        let soft_cap_cli = if cli.no_soft_cap {
            Some(0u64)
        } else {
            cli.soft_cap_mb
        };
        let soft_cap_mb = pick_value(
            soft_cap_cli,
            None, // no env equivalent — new surface
            file.and_then(|f| f.soft_cap_mb),
            DEFAULT_SOFT_CAP_MB,
        );

        let capture_http = pick_value(
            if cli.capture_http { Some(true) } else { None },
            env_truthy("OSTK_CAPTURE_HTTP"),
            file.and_then(|f| f.capture.http),
            false,
        );

        let capture_http_dir = pick_value(
            cli.capture_http_dir.clone(),
            env_str("OSTK_CAPTURE_HTTP_DIR").map(PathBuf::from),
            file.and_then(|f| f.capture.dir.clone()),
            crate::http_capture::default_capture_dir(cwd),
        );

        let verbose = pick_value(
            if cli.verbose { Some(true) } else { None },
            None,
            None,
            false,
        );

        Self {
            port,
            upstream,
            provider,
            mode,
            tail_transcript,
            tail_limit,
            claude_projects_dir,
            rewrite_enabled,
            ostk_dir,
            kernel_socket,
            kernel_timeout_ms,
            soft_cap_mb,
            capture_http,
            capture_http_dir,
            verbose,
            config_path,
        }
    }

    /// Format the resolved config as a human-readable table for
    /// `--print-config`. Each line is `field = value (source)`.
    pub fn print_table(&self) -> String {
        let mut out = String::new();
        let cfg_line = match &self.config_path {
            Some(p) => format!("# config file: {}\n", p.display()),
            None => "# config file: (none — using defaults + env + cli only)\n".to_string(),
        };
        out.push_str(&cfg_line);
        out.push('\n');
        out.push_str(&row("port", &self.port.value, self.port.source));
        out.push_str(&row("provider", &self.provider.value, self.provider.source));
        out.push_str(&row("upstream", &self.upstream.value, self.upstream.source));
        out.push_str(&row("mode", &self.mode.value, self.mode.source));
        out.push_str(&row(
            "tail.transcript",
            &self.tail_transcript.value,
            self.tail_transcript.source,
        ));
        out.push_str(&row("tail.limit", &self.tail_limit.value, self.tail_limit.source));
        out.push_str(&row(
            "tail.claude_projects_dir",
            &self.claude_projects_dir.value.display(),
            self.claude_projects_dir.source,
        ));
        out.push_str(&row(
            "rewrite.enabled",
            &self.rewrite_enabled.value,
            self.rewrite_enabled.source,
        ));
        out.push_str(&row(
            "rewrite.ostk_dir",
            &display_opt_path(&self.ostk_dir.value),
            self.ostk_dir.source,
        ));
        out.push_str(&row(
            "kernel.socket",
            &display_opt_path(&self.kernel_socket.value),
            self.kernel_socket.source,
        ));
        out.push_str(&row(
            "kernel.timeout_ms",
            &self.kernel_timeout_ms.value,
            self.kernel_timeout_ms.source,
        ));
        let cap = if self.soft_cap_mb.value == 0 {
            "disabled".to_string()
        } else {
            format!("{}MB", self.soft_cap_mb.value)
        };
        out.push_str(&row("soft_cap", &cap, self.soft_cap_mb.source));
        out.push_str(&row(
            "capture.http",
            &self.capture_http.value,
            self.capture_http.source,
        ));
        out.push_str(&row(
            "capture.dir",
            &self.capture_http_dir.value.display(),
            self.capture_http_dir.source,
        ));
        out.push_str(&row("verbose", &self.verbose.value, self.verbose.source));
        out
    }

    /// Short two-line startup banner: mode, soft-cap, tail status.
    pub fn banner(&self) -> String {
        let cap = if self.soft_cap_mb.value == 0 {
            "off".to_string()
        } else {
            format!("{}MB", self.soft_cap_mb.value)
        };
        let tail = if self.tail_transcript.value {
            format!("on (limit={})", self.tail_limit.value)
        } else {
            "off".to_string()
        };
        let capture = if self.capture_http.value {
            format!("on ({})", self.capture_http_dir.value.display())
        } else {
            "off".to_string()
        };
        format!(
            "  provider={}  mode={}  soft-cap={}  tail={}\n  rewrite={}  capture={}  kernel-timeout={}ms",
            self.provider.value,
            self.mode.value,
            cap,
            tail,
            if self.rewrite_enabled.value { "on" } else { "off" },
            capture,
            self.kernel_timeout_ms.value,
        )
    }
}

fn display_opt_path(p: &Option<PathBuf>) -> String {
    match p {
        Some(p) => p.display().to_string(),
        None => "(unset)".into(),
    }
}

fn row<V: std::fmt::Display>(name: &str, value: &V, source: Source) -> String {
    format!("{name:<26} = {value:<32} ({source})\n")
}

// ---------------------------------------------------------------------------
// Precedence pick helpers
// ---------------------------------------------------------------------------

fn pick_value<T>(cli: Option<T>, env: Option<T>, toml_v: Option<T>, default: T) -> Resolved<T> {
    if let Some(v) = cli {
        Resolved::new(v, Source::Cli)
    } else if let Some(v) = env {
        Resolved::new(v, Source::Env)
    } else if let Some(v) = toml_v {
        Resolved::new(v, Source::Toml)
    } else {
        Resolved::new(default, Source::Default)
    }
}

/// Variant of [`pick_value`] for optional values whose "default" is `None`.
fn pick_value_opt<T>(cli: Option<T>, env: Option<T>, toml_v: Option<T>) -> Resolved<Option<T>> {
    if let Some(v) = cli {
        Resolved::new(Some(v), Source::Cli)
    } else if let Some(v) = env {
        Resolved::new(Some(v), Source::Env)
    } else if let Some(v) = toml_v {
        Resolved::new(Some(v), Source::Toml)
    } else {
        Resolved::new(None, Source::Default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Env mutation tests must run serially.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // Caller MUST already hold ENV_LOCK. (Mutex is not re-entrant; locking
    // inside a test that already locked deadlocks the test thread.)
    fn clear_env() {
        for k in [
            "PROXY_PORT",
            "ANTHROPIC_BASE_URL",
            "OPENAI_BASE_URL",
            "OSTK_PROVIDER",
            "OSTK_CACHE_PASSTHROUGH",
            "OSTK_CACHE_REBUILD",
            "OSTK_CACHE_TAIL_TRANSCRIPT",
            "OSTK_CACHE_TAIL_LIMIT",
            "OSTK_CACHE_CLAUDE_PROJECTS_DIR",
            "OSTK_REWRITE_ENABLED",
            "OSTK_DIR",
            "OSTK_KERNEL_SOCKET",
            "OSTK_CACHE_KERNEL_TIMEOUT_MS",
            "OSTK_CAPTURE_HTTP",
            "OSTK_CAPTURE_HTTP_DIR",
        ] {
            unsafe { std::env::remove_var(k) };
        }
    }

    fn empty_cli() -> CliArgs {
        CliArgs {
            port: None,
            upstream: None,
            provider: None,
            mode: None,
            tail_transcript: false,
            no_tail_transcript: false,
            tail_limit: None,
            claude_projects_dir: None,
            ostk_dir: None,
            kernel_socket: None,
            kernel_timeout_ms: None,
            no_rewrite: false,
            soft_cap_mb: None,
            no_soft_cap: false,
            capture_http: false,
            capture_http_dir: None,
            verbose: false,
            config: None,
            print_config: false,
        }
    }

    #[test]
    fn default_resolution_no_env_no_file() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::resolve(empty_cli(), tmp.path());
        assert_eq!(cfg.port.value, DEFAULT_PORT);
        assert_eq!(cfg.port.source, Source::Default);
        assert_eq!(cfg.mode.value, DEFAULT_MODE);
        assert_eq!(cfg.soft_cap_mb.value, DEFAULT_SOFT_CAP_MB);
        assert!(cfg.rewrite_enabled.value);
        assert_eq!(cfg.rewrite_enabled.source, Source::Default);
    }

    #[test]
    fn env_overrides_default() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        unsafe { std::env::set_var("PROXY_PORT", "9999") };
        unsafe { std::env::set_var("OSTK_CACHE_REBUILD", "kernel") };
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::resolve(empty_cli(), tmp.path());
        assert_eq!(cfg.port.value, 9999);
        assert_eq!(cfg.port.source, Source::Env);
        assert_eq!(cfg.mode.value, Mode::RebuildKernel);
        assert_eq!(cfg.mode.source, Source::Env);
        clear_env();
    }

    #[test]
    fn toml_overrides_default_but_not_env() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".ostk")).unwrap();
        std::fs::write(
            tmp.path().join(".ostk").join("cache.toml"),
            r#"port = 7777
mode = "rebuild"
soft_cap_mb = 25
[tail]
transcript = true
limit = 99
[rewrite]
enabled = false
"#,
        )
        .unwrap();
        let cfg = Config::resolve(empty_cli(), tmp.path());
        assert_eq!(cfg.port.value, 7777);
        assert_eq!(cfg.port.source, Source::Toml);
        assert_eq!(cfg.mode.value, Mode::Rebuild);
        assert_eq!(cfg.soft_cap_mb.value, 25);
        assert!(cfg.tail_transcript.value);
        assert_eq!(cfg.tail_limit.value, 99);
        assert!(!cfg.rewrite_enabled.value);

        // Env beats TOML
        unsafe { std::env::set_var("PROXY_PORT", "5555") };
        let cfg = Config::resolve(empty_cli(), tmp.path());
        assert_eq!(cfg.port.value, 5555);
        assert_eq!(cfg.port.source, Source::Env);
        clear_env();
    }

    #[test]
    fn cli_overrides_everything() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        unsafe { std::env::set_var("PROXY_PORT", "5555") };
        let mut cli = empty_cli();
        cli.port = Some(4242);
        cli.mode = Some(Mode::Passthrough);
        cli.no_soft_cap = true;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".ostk")).unwrap();
        std::fs::write(
            tmp.path().join(".ostk").join("cache.toml"),
            "port = 7777\n",
        )
        .unwrap();

        let cfg = Config::resolve(cli, tmp.path());
        assert_eq!(cfg.port.value, 4242);
        assert_eq!(cfg.port.source, Source::Cli);
        assert_eq!(cfg.mode.value, Mode::Passthrough);
        assert_eq!(cfg.soft_cap_mb.value, 0);
        clear_env();
    }

    #[test]
    fn gpt_provider_uses_openai_upstream_env() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        unsafe { std::env::set_var("OSTK_PROVIDER", "gpt") };
        unsafe { std::env::set_var("OPENAI_BASE_URL", "http://openai.test") };
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::resolve(empty_cli(), tmp.path());
        assert_eq!(cfg.provider.value, Provider::Gpt);
        assert_eq!(cfg.provider.source, Source::Env);
        assert_eq!(cfg.upstream.value, "http://openai.test");
        assert_eq!(cfg.upstream.source, Source::Env);
        clear_env();
    }

    #[test]
    fn legacy_passthrough_env_routes_to_mode() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        unsafe { std::env::set_var("OSTK_CACHE_PASSTHROUGH", "1") };
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::resolve(empty_cli(), tmp.path());
        assert_eq!(cfg.mode.value, Mode::Passthrough);
        assert_eq!(cfg.mode.source, Source::Env);
        clear_env();
    }
}
