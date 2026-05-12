use clap::Parser;
use ostk_cache::{AmpRow, WorkspaceId, SessionId};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long, default_value = "session")]
    window: String,

    #[arg(long, default_value = "json")]
    format: String,

    #[arg(long)]
    workspace: Option<String>,

    /// Filter ledger rows by proxy mode. Accepted values:
    /// - "mutate" — proxy rewrote the request (legacy default).
    /// - "passthrough" — proxy forwarded verbatim (n=47 baseline).
    /// - "rebuild_local" — Layer 1 standalone request rebuild.
    /// - "rebuild_kernel" — Layer 2 federated kernel-projection rebuild.
    /// - "all" — no filter (default).
    /// Old rows persisted before the mode field existed parse as
    /// "mutate" via serde default.
    #[arg(long, default_value = "all")]
    mode: String,
}

#[derive(Default, Debug)]
struct SessionStats {
    workspace_id: WorkspaceId,
    session_id: SessionId,
    turns: usize,
    input_tokens_total_sum: usize,
    cache_read_total_sum: usize,
    cache_create_total_sum: usize,
    amp_ratios: Vec<f64>,
    hot_pages_max: usize,
    evictions: usize,
    firmware_bytes: usize,
    state_bytes_sum: usize,
    last_hot_count: usize,
}

fn main() {
    let args = Args::parse();

    // Validate --mode before doing any work; surface bad values with a
    // pointed error rather than silently returning an empty result set.
    match args.mode.as_str() {
        "mutate"
        | "passthrough"
        | "rebuild_local"
        | "rebuild_kernel"
        | "rebuild_skip"
        | "all" => {}
        other => {
            eprintln!(
                "Invalid --mode {:?}: expected one of mutate, passthrough, rebuild_local, rebuild_kernel, rebuild_skip, all",
                other
            );
            std::process::exit(1);
        }
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let window_secs = match args.window.as_str() {
        "5m" => Some(5 * 60),
        "1h" => Some(60 * 60),
        "24h" => Some(24 * 60 * 60),
        "session" => None,
        _ => {
            eprintln!("Invalid window: {}", args.window);
            std::process::exit(1);
        }
    };

    let ledger_path = Path::new(".ostk/memory/ledger.jsonl");
    if !ledger_path.exists() {
        eprintln!("Ledger not found at {:?}", ledger_path);
        std::process::exit(1);
    }

    let file = File::open(ledger_path).unwrap();
    let reader = BufReader::new(file);

    let mut stats_map: HashMap<(WorkspaceId, SessionId), SessionStats> = HashMap::new();

    for line in reader.lines() {
        if let Ok(l) = line
            && let Ok(row) = serde_json::from_str::<AmpRow>(&l) {
                if let Some(ws) = &args.workspace
                    && &row.workspace_id != ws {
                        continue;
                    }

                if args.mode != "all" && row.mode != args.mode {
                    continue;
                }

                if let Some(w_secs) = window_secs
                    && row.timestamp > 0 && row.timestamp < now - w_secs {
                        continue;
                    }

                let key = (row.workspace_id.clone(), row.session.clone());
                let entry = stats_map.entry(key).or_insert_with(|| SessionStats {
                    workspace_id: row.workspace_id.clone(),
                    session_id: row.session.clone(),
                    ..Default::default()
                });

                entry.turns += 1;
                entry.input_tokens_total_sum += row.input_tokens_total;
                entry.cache_read_total_sum += row.cache_read_tokens;
                entry.cache_create_total_sum += row.cache_create_tokens;
                entry.amp_ratios.push(row.amp_ratio);
                
                if row.hot_count > entry.hot_pages_max {
                    entry.hot_pages_max = row.hot_count;
                }
                
                if row.hot_count < entry.last_hot_count {
                    entry.evictions += entry.last_hot_count - row.hot_count;
                }
                entry.last_hot_count = row.hot_count;
                
                entry.firmware_bytes = row.firmware_bytes;
                entry.state_bytes_sum += row.state_bytes;
            }
    }

    let mut results = Vec::new();

    for (_, stats) in stats_map {
        let mut sorted_amps = stats.amp_ratios.clone();
        sorted_amps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        
        let turns_f = stats.turns as f64;
        let amp_mean = if stats.turns > 0 {
            stats.amp_ratios.iter().sum::<f64>() / turns_f
        } else {
            1.0
        };

        let amp_p50 = if sorted_amps.is_empty() {
            1.0
        } else {
            sorted_amps[sorted_amps.len() / 2]
        };

        let amp_p95 = if sorted_amps.is_empty() {
            1.0
        } else {
            let idx = (sorted_amps.len() as f64 * 0.95) as usize;
            sorted_amps[std::cmp::min(idx, sorted_amps.len() - 1)]
        };

        let cache_hit_rate = if stats.cache_read_total_sum + stats.input_tokens_total_sum == 0 {
            0.0
        } else {
            stats.cache_read_total_sum as f64 / (stats.cache_read_total_sum as f64 + stats.input_tokens_total_sum as f64)
        };

        let state_bytes_mean = if stats.turns > 0 {
            stats.state_bytes_sum as f64 / turns_f
        } else {
            0.0
        };

        // Tokens saved = input-token-equivalent the rebuild avoided, using
        // Anthropic's cache pricing (read=0.1x, create=1.25x, fresh input=1x).
        // Counterfactual: every read+create was billed at 1x (no cache at all).
        // Actual: read*0.1 + create*1.25. Savings per row = 0.9*read - 0.25*create.
        // The raw cache_read_total is the louder number — those tokens would
        // have been re-billed full-price every turn without the rewrite.
        let cache_read_f = stats.cache_read_total_sum as f64;
        let cache_create_f = stats.cache_create_total_sum as f64;
        let tokens_saved_input_eq = 0.9 * cache_read_f - 0.25 * cache_create_f;

        let result = serde_json::json!({
            "workspace_id": stats.workspace_id,
            "session_id": stats.session_id,
            "mode": args.mode,
            "turns": stats.turns,
            "amp_mean": amp_mean,
            "amp_p50": amp_p50,
            "amp_p95": amp_p95,
            "cache_hit_rate": cache_hit_rate,
            "cache_read_tokens_total": stats.cache_read_total_sum,
            "cache_create_tokens_total": stats.cache_create_total_sum,
            "input_tokens_total": stats.input_tokens_total_sum,
            "tokens_saved_input_eq": tokens_saved_input_eq,
            "hot_pages_max": stats.hot_pages_max,
            "evictions": stats.evictions,
            "firmware_bytes": stats.firmware_bytes,
            "state_bytes_mean": state_bytes_mean
        });

        results.push(result);
    }

    if args.format == "json" {
        for res in &results {
            println!("{}", serde_json::to_string(res).unwrap());
        }
    } else if args.format == "csv"
        && !results.is_empty() {
            println!("workspace_id,session_id,mode,turns,amp_mean,amp_p50,amp_p95,cache_hit_rate,cache_read_tokens_total,cache_create_tokens_total,input_tokens_total,tokens_saved_input_eq,hot_pages_max,evictions,firmware_bytes,state_bytes_mean");
            for res in results {
                let obj = res.as_object().unwrap();
                println!("{},{},{},{},{:.2},{:.2},{:.2},{:.4},{},{},{},{:.0},{},{},{},{:.2}",
                    obj["workspace_id"].as_str().unwrap_or(""),
                    obj["session_id"].as_str().unwrap_or(""),
                    obj["mode"].as_str().unwrap_or(""),
                    obj["turns"],
                    obj["amp_mean"].as_f64().unwrap_or(0.0),
                    obj["amp_p50"].as_f64().unwrap_or(0.0),
                    obj["amp_p95"].as_f64().unwrap_or(0.0),
                    obj["cache_hit_rate"].as_f64().unwrap_or(0.0),
                    obj["cache_read_tokens_total"],
                    obj["cache_create_tokens_total"],
                    obj["input_tokens_total"],
                    obj["tokens_saved_input_eq"].as_f64().unwrap_or(0.0),
                    obj["hot_pages_max"],
                    obj["evictions"],
                    obj["firmware_bytes"],
                    obj["state_bytes_mean"].as_f64().unwrap_or(0.0)
                );
            }
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stats_computation() {
        let mut stats_map: HashMap<(WorkspaceId, SessionId), SessionStats> = HashMap::new();
        
        let key = ("ws1".to_string(), "sess1".to_string());
        let mut entry = SessionStats {
            workspace_id: "ws1".to_string(),
            session_id: "sess1".to_string(),
            ..Default::default()
        };

        for i in 1..=10 {
            entry.turns += 1;
            entry.input_tokens_total_sum += 100;
            entry.cache_read_total_sum += 50;
            entry.amp_ratios.push(i as f64);
        }
        
        stats_map.insert(key, entry);
        
        let stats = stats_map.get(&("ws1".to_string(), "sess1".to_string())).unwrap();
        let mut sorted_amps = stats.amp_ratios.clone();
        sorted_amps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        
        let turns_f = stats.turns as f64;
        let amp_mean = stats.amp_ratios.iter().sum::<f64>() / turns_f;
        let amp_p50 = sorted_amps[sorted_amps.len() / 2];
        let idx = (sorted_amps.len() as f64 * 0.95) as usize;
        let amp_p95 = sorted_amps[std::cmp::min(idx, sorted_amps.len() - 1)];

        assert_eq!(amp_mean, 5.5);
        assert_eq!(amp_p50, 6.0);
        assert_eq!(amp_p95, 10.0);
    }
}
