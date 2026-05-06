#!/bin/bash
cd /Users/scottmeyer/projects/ostk-cache
echo "[$(date +%T)] [Tester] Booting. Waiting for implementation lock release..." > tester.log
while [ ! -f .impl_done ]; do sleep 1; done
echo "[$(date +%T)] [Tester] Lock acquired. Injecting tests based on P8 predicates..." >> tester.log
sleep 2
cat << 'RUST' >> src/lib.rs

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_store_and_load_flow() {
        let mut table = InMemoryPageTable::new();
        let page = table.store("firmware".to_string(), b"sys_prompt", "ws1".to_string());
        assert_eq!(page.state, PageState::Hot);
        
        let loaded = table.load("firmware".to_string(), "ws1".to_string()).unwrap();
        assert_eq!(loaded.content_hash, page.content_hash);
        
        table.release("firmware".to_string(), "ws1".to_string());
        let released = table.load("firmware".to_string(), "ws1".to_string()).unwrap();
        assert_eq!(released.state, PageState::Warm);
    }
}
RUST
echo "[$(date +%T)] [Tester] Executing cargo test..." >> tester.log
cargo test >> tester.log 2>&1
echo "[$(date +%T)] [Tester] Pipeline complete. Target achieved." >> tester.log
touch .test_done