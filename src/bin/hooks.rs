// ostk-cache hooks CLI — install, uninstall, status of Claude Code hook
// integration for the local proxy.
//
// Hook integration model:
//   - One dispatch script (scripts/dispatch.sh) embedded via include_str!
//     and written to ~/.config/ostk-cache/hooks/dispatch.sh on install.
//   - Five entries appended to ~/.claude/settings.json's hooks lattice
//     (SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, Stop), each
//     invoking the dispatch script with the event name as argv[1].
//   - Idempotent install: re-running detects existing entries by command
//     path and is a no-op.
//   - Non-destructive: existing hook entries on the same events are
//     preserved (Claude Code runs every entry in the array per event).
//   - Quiet failure: dispatch.sh uses `curl ... || true` so a missing or
//     wedged proxy never blocks a Claude session.

use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const DISPATCH_SCRIPT: &str = include_str!("../../scripts/dispatch.sh");

const EVENTS: &[(&str, bool)] = &[
    ("SessionStart", false),
    ("UserPromptSubmit", false),
    ("PreToolUse", true),
    ("PostToolUse", true),
    ("Stop", false),
];

#[derive(Parser)]
#[command(
    name = "ostk-cache-hooks",
    about = "Install/uninstall/status Claude Code hooks for ostk-cache.",
    long_about = None,
)]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Install the dispatch script and append entries to settings.json.
    /// Idempotent. Backs settings.json up to settings.json.ostk-cache.bak
    /// before any change.
    Install {
        /// Hook scripts directory. Defaults to ~/.config/ostk-cache/hooks.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Proxy port the hooks should target (currently informational).
        #[arg(long, default_value_t = 8080)]
        port: u16,
        /// Override settings.json path. Defaults to ~/.claude/settings.json.
        #[arg(long)]
        settings: Option<PathBuf>,
    },
    /// Remove our entries from settings.json. By default keeps the script
    /// files on disk; pass --purge to remove them too.
    Uninstall {
        /// Also remove the script directory.
        #[arg(long)]
        purge: bool,
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long)]
        settings: Option<PathBuf>,
    },
    /// Print install state and proxy reachability.
    Status {
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long, default_value_t = 8080)]
        port: u16,
        #[arg(long)]
        settings: Option<PathBuf>,
    },
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME env var must be set")
}

fn default_dir() -> PathBuf {
    home().join(".config/ostk-cache/hooks")
}

fn default_settings() -> PathBuf {
    home().join(".claude/settings.json")
}

fn dispatch_path(dir: &Path) -> PathBuf {
    dir.join("dispatch.sh")
}

fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(
        ".{}.ostk-cache.tmp",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("settings")
    ));
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn entry_matches_dispatch(entry: &Value, dispatch_prefix: &str) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .map(|s| s.starts_with(dispatch_prefix))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn install(
    dir: Option<PathBuf>,
    port: u16,
    settings_path: Option<PathBuf>,
) -> std::io::Result<()> {
    let dir = dir.unwrap_or_else(default_dir);
    let settings_path = settings_path.unwrap_or_else(default_settings);

    fs::create_dir_all(&dir)?;
    let dp = dispatch_path(&dir);
    fs::write(&dp, DISPATCH_SCRIPT)?;
    let mut perms = fs::metadata(&dp)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&dp, perms)?;

    let dispatch_str = dp.to_string_lossy().to_string();

    let backup_path = settings_path.with_extension("json.ostk-cache.bak");
    if settings_path.exists() {
        fs::copy(&settings_path, &backup_path)?;
    }

    let mut s: Value = if settings_path.exists() {
        serde_json::from_slice(&fs::read(&settings_path)?).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e)
        })?
    } else {
        json!({})
    };

    if !s.is_object() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "settings.json root must be an object",
        ));
    }

    let s_obj = s.as_object_mut().unwrap();
    let hooks = s_obj.entry("hooks").or_insert_with(|| json!({}));
    let hooks_obj = hooks.as_object_mut().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "settings.hooks must be an object",
        )
    })?;

    let mut added: Vec<&str> = vec![];
    for (event, has_matcher) in EVENTS {
        let cmd = format!("{} {}", dispatch_str, event);
        let arr = hooks_obj
            .entry(*event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("settings.hooks.{} must be an array", event),
                )
            })?;

        let already = arr.iter().any(|entry| {
            entry_matches_dispatch(entry, &dispatch_str)
        });

        if !already {
            let mut entry = json!({
                "hooks": [{"type": "command", "command": cmd}]
            });
            if *has_matcher {
                entry
                    .as_object_mut()
                    .unwrap()
                    .insert("matcher".to_string(), json!(".*"));
            }
            arr.push(entry);
            added.push(event);
        }
    }

    let serialized = serde_json::to_string_pretty(&s)? + "\n";
    write_atomic(&settings_path, &serialized)?;

    println!("dispatch script: {}", dp.display());
    if backup_path.exists() {
        println!("settings backup: {}", backup_path.display());
    }
    println!("settings file:   {}", settings_path.display());
    if added.is_empty() {
        println!("install:         already up to date — no changes");
    } else {
        println!("install:         added {} entries", added.len());
        for e in &added {
            println!("                   • {}", e);
        }
    }
    println!("proxy port:      {}", port);
    println!();
    println!("Activate by restarting the proxy and starting a new Claude Code");
    println!("session. Verify with: ostk-cache hooks status.");

    Ok(())
}

fn uninstall(
    dir: Option<PathBuf>,
    purge: bool,
    settings_path: Option<PathBuf>,
) -> std::io::Result<()> {
    let dir = dir.unwrap_or_else(default_dir);
    let settings_path = settings_path.unwrap_or_else(default_settings);
    let dp = dispatch_path(&dir);
    let dispatch_str = dp.to_string_lossy().to_string();

    let mut removed_total = 0usize;
    if settings_path.exists() {
        let backup_path = settings_path.with_extension("json.ostk-cache.bak");
        fs::copy(&settings_path, &backup_path)?;

        let mut s: Value = serde_json::from_slice(&fs::read(&settings_path)?)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        if let Some(hooks) =
            s.get_mut("hooks").and_then(|h| h.as_object_mut())
        {
            let event_keys: Vec<String> = hooks.keys().cloned().collect();
            for event in event_keys {
                let arr = hooks
                    .get_mut(&event)
                    .unwrap()
                    .as_array_mut()
                    .unwrap();
                let before = arr.len();
                arr.retain(|entry| {
                    !entry_matches_dispatch(entry, &dispatch_str)
                });
                removed_total += before - arr.len();
                if arr.is_empty() {
                    hooks.remove(&event);
                }
            }
        }

        let serialized = serde_json::to_string_pretty(&s)? + "\n";
        write_atomic(&settings_path, &serialized)?;

        println!("settings backup:  {}", backup_path.display());
        println!("settings file:    {}", settings_path.display());
        println!("entries removed:  {}", removed_total);
    } else {
        println!("settings.json not found at {}", settings_path.display());
    }

    if purge {
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
            println!("purged scripts:   {}", dir.display());
        } else {
            println!("scripts dir already absent: {}", dir.display());
        }
    } else {
        println!(
            "scripts retained: {} (pass --purge to remove)",
            dir.display()
        );
    }

    Ok(())
}

fn status(
    dir: Option<PathBuf>,
    port: u16,
    settings_path: Option<PathBuf>,
) -> std::io::Result<()> {
    let dir = dir.unwrap_or_else(default_dir);
    let settings_path = settings_path.unwrap_or_else(default_settings);
    let dp = dispatch_path(&dir);
    let dispatch_str = dp.to_string_lossy().to_string();

    let dp_mark = if dp.exists() { "✓" } else { "✗ missing" };
    let sp_mark = if settings_path.exists() {
        "✓"
    } else {
        "✗ missing"
    };

    println!("Hook scripts dir: {}", dir.display());
    println!("Dispatch script:  {} {}", dp.display(), dp_mark);
    println!("Settings file:    {} {}", settings_path.display(), sp_mark);
    println!("Proxy URL:        http://127.0.0.1:{}", port);

    let s: Value = if settings_path.exists() {
        serde_json::from_slice(&fs::read(&settings_path)?).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e)
        })?
    } else {
        json!({})
    };
    let hooks = s.get("hooks").and_then(|h| h.as_object());

    println!();
    println!("Event entries:");
    for (event, _) in EVENTS {
        let installed = hooks
            .and_then(|h| h.get(*event))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().any(|e| entry_matches_dispatch(e, &dispatch_str))
            })
            .unwrap_or(false);
        let mark = if installed { "✓" } else { "✗" };
        println!("  {} {}", mark, event);
    }

    println!();
    print!("Proxy reachable:  ");
    let url = format!("http://127.0.0.1:{}/hook/event", port);
    let probe = std::process::Command::new("curl")
        .args([
            "-fsS",
            "--connect-timeout",
            "1",
            "-m",
            "2",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-X",
            "POST",
            &url,
            "-H",
            "content-type: application/json",
            "-d",
            r#"{"type":"__probe__","workspace_id":"_","session_id":"_"}"#,
        ])
        .output();
    match probe {
        Ok(o) => {
            let code = String::from_utf8_lossy(&o.stdout);
            // Probe sends type=__probe__ which is always an unknown event.
            // Live proxy with /hook/event endpoint -> 400 (unknown type).
            // Live proxy without endpoint -> 404 (older binary).
            // Down -> empty stdout (curl exits non-zero, stdout empty).
            match code.trim() {
                "400" => println!(
                    "✓ proxy alive at :{} — /hook/event endpoint present",
                    port
                ),
                "404" => println!(
                    "⚠ proxy alive at :{} but /hook/event not found — restart with the latest binary",
                    port
                ),
                "" => println!("✗ no response (proxy down or unreachable)"),
                other => println!("? unexpected probe response: HTTP {}", other),
            }
        }
        Err(_) => println!("✗ curl not available"),
    }

    Ok(())
}

fn main() -> std::io::Result<()> {
    let args = Args::parse();
    match args.cmd {
        Cmd::Install {
            dir,
            port,
            settings,
        } => install(dir, port, settings),
        Cmd::Uninstall {
            dir,
            purge,
            settings,
        } => uninstall(dir, purge, settings),
        Cmd::Status {
            dir,
            port,
            settings,
        } => status(dir, port, settings),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // HOME mutation is process-wide; serialize tests that touch it.
    static HOME_LOCK: Mutex<()> = Mutex::new(());

    fn setup_temp_home() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn run_install(temp: &tempfile::TempDir) -> PathBuf {
        let settings_path = temp.path().join("settings.json");
        let hooks_dir = temp.path().join("hooks");
        install(
            Some(hooks_dir.clone()),
            8080,
            Some(settings_path.clone()),
        )
        .unwrap();
        settings_path
    }

    #[test]
    fn install_creates_dispatch_script_and_settings_entries() {
        let _g = HOME_LOCK.lock().unwrap();
        let temp = setup_temp_home();
        let settings_path = run_install(&temp);

        let dispatch = temp.path().join("hooks").join("dispatch.sh");
        assert!(dispatch.exists());
        let perms = fs::metadata(&dispatch).unwrap().permissions();
        assert_eq!(perms.mode() & 0o777, 0o755);

        let s: Value = serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
        let hooks = s.get("hooks").unwrap().as_object().unwrap();
        for (event, has_matcher) in EVENTS {
            let arr = hooks.get(*event).unwrap().as_array().unwrap();
            assert_eq!(arr.len(), 1, "exactly one entry for {}", event);
            let entry = &arr[0];
            if *has_matcher {
                assert_eq!(entry.get("matcher").unwrap(), ".*");
            } else {
                assert!(entry.get("matcher").is_none());
            }
            let cmd = entry.get("hooks").unwrap()[0]
                .get("command")
                .unwrap()
                .as_str()
                .unwrap();
            assert!(cmd.contains("dispatch.sh"));
            assert!(cmd.ends_with(event));
        }
    }

    #[test]
    fn install_is_idempotent() {
        let _g = HOME_LOCK.lock().unwrap();
        let temp = setup_temp_home();
        let settings_path = run_install(&temp);
        let s1: Value = serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();

        // Re-run install
        let hooks_dir = temp.path().join("hooks");
        install(Some(hooks_dir), 8080, Some(settings_path.clone())).unwrap();

        let s2: Value = serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
        assert_eq!(s1, s2, "second install must not change settings.json");
    }

    #[test]
    fn install_preserves_existing_unrelated_entries() {
        let _g = HOME_LOCK.lock().unwrap();
        let temp = setup_temp_home();
        let settings_path = temp.path().join("settings.json");
        let hooks_dir = temp.path().join("hooks");

        // Pre-populate settings with some unrelated user config.
        let initial = json!({
            "env": {"FOO": "bar"},
            "permissions": {"allow": ["x"], "deny": []},
            "hooks": {
                "PreToolUse": [
                    {"matcher": "Edit|Write", "hooks": [{"type":"command","command":"my-other-tool pre"}]}
                ],
                "Stop": [
                    {"hooks": [{"type":"command","command":"my-stop-hook"}]}
                ]
            }
        });
        fs::write(
            &settings_path,
            serde_json::to_string_pretty(&initial).unwrap(),
        )
        .unwrap();

        install(Some(hooks_dir), 8080, Some(settings_path.clone())).unwrap();

        let s: Value = serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
        // Non-hooks keys preserved.
        assert_eq!(s.get("env").unwrap().get("FOO").unwrap(), "bar");
        assert!(s.get("permissions").is_some());

        // Pre-existing hooks preserved alongside our new entries.
        let pretool = s.get("hooks").unwrap().get("PreToolUse").unwrap().as_array().unwrap();
        assert_eq!(pretool.len(), 2);
        assert_eq!(pretool[0].get("matcher").unwrap(), "Edit|Write");
        assert_eq!(
            pretool[0].get("hooks").unwrap()[0].get("command").unwrap(),
            "my-other-tool pre"
        );

        let stop = s.get("hooks").unwrap().get("Stop").unwrap().as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(
            stop[0].get("hooks").unwrap()[0].get("command").unwrap(),
            "my-stop-hook"
        );
    }

    #[test]
    fn uninstall_removes_only_our_entries() {
        let _g = HOME_LOCK.lock().unwrap();
        let temp = setup_temp_home();
        let settings_path = temp.path().join("settings.json");
        let hooks_dir = temp.path().join("hooks");

        // Pre-existing user entry.
        let initial = json!({
            "hooks": {
                "Stop": [
                    {"hooks": [{"type":"command","command":"keep-me"}]}
                ]
            }
        });
        fs::write(
            &settings_path,
            serde_json::to_string_pretty(&initial).unwrap(),
        )
        .unwrap();

        install(Some(hooks_dir.clone()), 8080, Some(settings_path.clone())).unwrap();
        uninstall(Some(hooks_dir.clone()), false, Some(settings_path.clone())).unwrap();

        let s: Value = serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
        let stop = s.get("hooks").unwrap().get("Stop").unwrap().as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert_eq!(
            stop[0].get("hooks").unwrap()[0].get("command").unwrap(),
            "keep-me"
        );
        // Events that had only our entry are removed entirely.
        for ev in &["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse"] {
            assert!(
                s.get("hooks").unwrap().get(*ev).is_none(),
                "{} event should be gone",
                ev
            );
        }
    }

    #[test]
    fn uninstall_purge_removes_scripts_dir() {
        let _g = HOME_LOCK.lock().unwrap();
        let temp = setup_temp_home();
        let settings_path = temp.path().join("settings.json");
        let hooks_dir = temp.path().join("hooks");

        install(Some(hooks_dir.clone()), 8080, Some(settings_path.clone())).unwrap();
        assert!(hooks_dir.exists());

        uninstall(Some(hooks_dir.clone()), true, Some(settings_path)).unwrap();
        assert!(!hooks_dir.exists());
    }

    #[test]
    fn install_handles_missing_settings_file() {
        let _g = HOME_LOCK.lock().unwrap();
        let temp = setup_temp_home();
        // settings.json doesn't exist; install must create it.
        let _ = run_install(&temp);
    }
}
