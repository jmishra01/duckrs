//! A small version of duckdb/src/storage/standard_buffer_manager.cpp @ rust-port-baseline.
//!
//! Keeps blocks read from a file in memory up to a limit, and evicts the least recently used one
//! that nobody holds. A pinned block is an `Arc`, so a block in use stays valid and stays here
//! however far over the limit that goes, as it does in C++ only until `memory_limit` throws.
//! Temporary files and spilling come with Phase 8's out-of-core operators.

use super::block_manager::SingleFileBlockManager;
use duckrs_common::Result;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug)]
pub struct BufferManager {
    blocks: Arc<SingleFileBlockManager>,
    memory_limit: usize,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    cached: HashMap<u64, Cached>,
    /// the bytes in `cached`
    used: usize,
    clock: u64,
}

#[derive(Debug)]
struct Cached {
    block: Arc<Vec<u8>>,
    last_use: u64,
}

impl BufferManager {
    pub fn new(blocks: Arc<SingleFileBlockManager>, memory_limit: usize) -> Self {
        Self {
            blocks,
            memory_limit,
            state: Mutex::default(),
        }
    }

    pub fn block_manager(&self) -> &Arc<SingleFileBlockManager> {
        &self.blocks
    }

    /// The bytes of the blocks in memory.
    pub fn used_memory(&self) -> usize {
        self.state.lock().used
    }

    /// Forgets all blocks, after a checkpoint that changed what they hold.
    pub fn clear(&self) {
        let mut state = self.state.lock();
        state.cached.clear();
        state.used = 0;
    }

    /// `Pin`: what the block holds.
    pub fn pin(&self, block_id: u64) -> Result<Arc<Vec<u8>>> {
        let mut state = self.state.lock();
        state.clock += 1;
        let clock = state.clock;
        if let Some(cached) = state.cached.get_mut(&block_id) {
            cached.last_use = clock;
            return Ok(cached.block.clone());
        }
        drop(state);
        // outside of the lock: others can use what is in memory during the read
        let block = Arc::new(self.blocks.read_block(block_id)?);
        let mut state = self.state.lock();
        state.used += block.len();
        let previous = state.cached.insert(
            block_id,
            Cached {
                block: block.clone(),
                last_use: clock,
            },
        );
        if let Some(previous) = previous {
            // somebody else read it in the meantime
            state.used -= previous.block.len();
        }
        Self::evict(&mut state, self.memory_limit);
        Ok(block)
    }

    fn evict(state: &mut State, limit: usize) {
        while state.used > limit {
            let victim = state
                .cached
                .iter()
                .filter(|(_, cached)| Arc::strong_count(&cached.block) == 1)
                .min_by_key(|(_, cached)| cached.last_use)
                .map(|(&id, _)| id);
            let Some(victim) = victim else { return };
            let cached = state.cached.remove(&victim).expect("just found");
            state.used -= cached.block.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_what_nobody_holds_least_recently_used_first() {
        let directory = std::env::temp_dir().join(format!("duckrs-buffer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let blocks = Arc::new(SingleFileBlockManager::create(&directory.join("b.db"), 16384).unwrap());
        let size = blocks.block_size();
        for id in 0..4 {
            blocks.write_block(id, &vec![id as u8; size]).unwrap();
        }
        let buffer = BufferManager::new(blocks, 2 * size);

        let held = buffer.pin(0).unwrap();
        buffer.pin(1).unwrap();
        buffer.pin(2).unwrap();
        // block 1 went, and held block 0 stayed
        assert_eq!(buffer.used_memory(), 2 * size);
        assert!(buffer.state.lock().cached.contains_key(&0));
        assert!(!buffer.state.lock().cached.contains_key(&1));

        // what is held does not go, however far over the limit that takes us
        let also_held = buffer.pin(3).unwrap();
        let third = buffer.pin(1).unwrap();
        assert_eq!(buffer.used_memory(), 3 * size);
        assert_eq!((held[0], also_held[0], third[0]), (0, 3, 1));
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
