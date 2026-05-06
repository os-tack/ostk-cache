#!/bin/bash
cd /Users/scottmeyer/projects/ostk-cache
echo "[$(date +%T)] [Probe-Builder] Booting..." > probe.log

mkdir -p src/bin
cat << 'RUST' > src/bin/probe_p5b.rs
use ostk_cache::{InMemoryPageTable, PageTable};

fn main() {
    let mut table = InMemoryPageTable::new();
    let ws = "ws_probe".to_string();
    
    // Simulate caching a large codebase context
    let firmware_content = vec![0u8; 500_000]; 
    table.store("firmware".to_string(), &firmware_content, ws.clone());
    
    // Generate Low Pressure HUD
    println!("--- LOW PRESSURE ENVIRONMENT ---");
    println!("[meminfo] ctx: 12% 96k/800k Buffers:0k");
    println!("cache: 5m=80% 1h=90% amp=15.0x stored=12 hot=3\n");
    
    // Generate High Pressure HUD
    println!("--- HIGH PRESSURE ENVIRONMENT ---");
    println!("[meminfo] ctx: 98% 784k/800k Buffers:0k ⚠ SEVERE PRESSURE ⚠");
    println!("cache: 5m=5% 1h=10% amp=1.1x stored=142 hot=60 ⚠ eviction imminent\n");
    
    println!("[Probe-Builder] Target binaries ready for LLM agent ingestion.");
}
RUST

cargo build --bin probe_p5b >> probe.log 2>&1
echo "[$(date +%T)] [Probe-Builder] Build complete. Yielding lock." >> probe.log
touch .probe_done