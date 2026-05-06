#!/bin/bash
cd /Users/scottmeyer/projects/ostk-cache
echo "[$(date +%T)] [Implementer] Booting. Watching for scaffolder lock release..." > implementer.log
while [ ! -f .scaffold_done ]; do sleep 1; done
echo "[$(date +%T)] [Implementer] Lock acquired. Building InMemoryPageTable..." >> implementer.log
sleep 3
cat << 'RUST' >> src/lib.rs

pub struct InMemoryPageTable {
    pages: HashMap<(WorkspaceId, PageName), Page>,
}

impl InMemoryPageTable {
    pub fn new() -> Self { Self { pages: HashMap::new() } }
}

impl PageTable for InMemoryPageTable {
    fn store(&mut self, name: PageName, content: &[u8], ws: WorkspaceId) -> Page {
        let page = Page {
            name: name.clone(),
            content_hash: format!("{:x}", md5::compute(content)),
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
        } else { None }
    }
    
    fn pin(&mut self, name: PageName, ws: WorkspaceId) {
        if let Some(page) = self.pages.get_mut(&(ws, name)) { page.pinned = true; }
    }
    
    fn evict(&mut self, name: PageName, ws: WorkspaceId) {
        self.pages.remove(&(ws, name));
    }
    
    fn release(&mut self, name: PageName, ws: WorkspaceId) {
        if let Some(page) = self.pages.get_mut(&(ws, name)) { page.state = PageState::Warm; }
    }
    
    fn restore(&mut self, name: PageName, ws: WorkspaceId) -> Option<Page> {
        if let Some(page) = self.pages.get_mut(&(ws, name)) {
            page.state = PageState::Hot;
            Some(page.clone())
        } else { None }
    }
}
RUST
cargo add md5 >> implementer.log 2>&1
echo "[$(date +%T)] [Implementer] Memory implementation bound. Yielding lock." >> implementer.log
touch .impl_done