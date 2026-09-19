//! Per-tab heap accounting for the debug HUD.
//!
//! All tabs share one process, so the OS only knows the total. The
//! global allocator is wrapped by `tracking-allocator`, which stamps
//! every allocation with the "allocation group" active on its thread;
//! [`in_tab`] makes a tab's group active around that tab's work, and a
//! free credits the group that allocated, wherever it happens.
//!
//! A group token can be entered by one thread at a time and cannot be
//! cloned, so a tab owns a small pool of tokens (one per concurrent
//! thread working for it) and its usage is the sum over their groups.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use tracking_allocator::{
    AllocationGroupId, AllocationGroupToken, AllocationRegistry, AllocationTracker, Allocator,
};

#[global_allocator]
static GLOBAL: Allocator<std::alloc::System> = Allocator::system();

/// Live bytes per allocation group id. Groups past the table fold into
/// slot 0 and are reported with the shell.
const GROUP_SLOTS: usize = 4096;
static LIVE_BYTES: [AtomicI64; GROUP_SLOTS] = [const { AtomicI64::new(0) }; GROUP_SLOTS];

/// Registry key of the browser shell (its UI, framebuffers, GPU, HUD).
const SHELL: u64 = u64::MAX;

#[derive(Default)]
struct Groups {
    /// Every group id ever handed to this owner.
    ids: Vec<usize>,
    /// Tokens not currently entered by any thread.
    idle: Vec<AllocationGroupToken>,
}

static OWNERS: Mutex<BTreeMap<u64, Groups>> = Mutex::new(BTreeMap::new());

fn slot(group: &AllocationGroupId) -> &'static AtomicI64 {
    let id = group.as_usize().get();
    &LIVE_BYTES[if id < GROUP_SLOTS { id } else { 0 }]
}

struct ByteCounter;

impl AllocationTracker for ByteCounter {
    fn allocated(&self, _addr: usize, _object: usize, wrapped: usize, group: AllocationGroupId) {
        slot(&group).fetch_add(wrapped as i64, Ordering::Relaxed);
    }

    fn deallocated(
        &self,
        _addr: usize,
        _object: usize,
        wrapped: usize,
        source: AllocationGroupId,
        _current: AllocationGroupId,
    ) {
        slot(&source).fetch_sub(wrapped as i64, Ordering::Relaxed);
    }
}

/// Starts accounting. Allocations made before this call stay uncounted.
pub fn enable() {
    if AllocationRegistry::set_global_tracker(ByteCounter).is_ok() {
        AllocationRegistry::enable_tracking();
    }
}

fn owners() -> std::sync::MutexGuard<'static, BTreeMap<u64, Groups>> {
    OWNERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn scoped<R>(owner: u64, work: impl FnOnce() -> R) -> R {
    let token = {
        let mut owners = owners();
        let groups = owners.entry(owner).or_default();
        groups.idle.pop().or_else(|| {
            let token = AllocationGroupToken::register()?;
            groups.ids.push(token.id().as_usize().get());
            Some(token)
        })
    };
    // Group ids ran out: run unattributed rather than not at all.
    let Some(mut token) = token else {
        return work();
    };
    let result = {
        let _entered = token.enter();
        work()
    };
    // A tab closed mid-work has no pool left to return the token to.
    if let Some(groups) = owners().get_mut(&owner) {
        groups.idle.push(token);
    }
    result
}

/// Runs `work` with its allocations charged to tab `tab`.
pub fn in_tab<R>(tab: u64, work: impl FnOnce() -> R) -> R {
    scoped(tab, work)
}

/// Runs `work` with its allocations charged to the shell, even when
/// called from inside [`in_tab`].
pub fn in_shell<R>(work: impl FnOnce() -> R) -> R {
    scoped(SHELL, work)
}

/// Drops a closed tab's bookkeeping.
pub fn forget(tab: u64) {
    owners().remove(&tab);
}

fn owner_bytes(owner: u64) -> u64 {
    let total: i64 = owners().get(&owner).map_or(0, |groups| {
        groups
            .ids
            .iter()
            .filter(|&&id| id < GROUP_SLOTS)
            .map(|&id| LIVE_BYTES[id].load(Ordering::Relaxed))
            .sum()
    });
    total.max(0) as u64
}

/// Live heap bytes charged to tab `tab`.
pub fn tab_bytes(tab: u64) -> u64 {
    owner_bytes(tab)
}

/// Live heap bytes charged to no tab: the shell plus untagged threads.
pub fn shell_bytes() -> u64 {
    let root = LIVE_BYTES[AllocationGroupId::ROOT.as_usize().get()].load(Ordering::Relaxed);
    let overflow = LIVE_BYTES[0].load(Ordering::Relaxed);
    owner_bytes(SHELL) + (root + overflow).max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEGABYTE: u64 = 1024 * 1024;

    #[test]
    fn allocations_are_charged_to_their_tab_until_freed() {
        enable();
        let (first, second) = (9_000_001, 9_000_002);
        let buffer = in_tab(first, || vec![0u8; 4 * MEGABYTE as usize]);
        assert!(tab_bytes(first) >= 4 * MEGABYTE);
        assert!(tab_bytes(second) < MEGABYTE);

        // Freed from another tab's scope: the allocating tab is credited.
        in_tab(second, || drop(buffer));
        assert!(tab_bytes(first) < MEGABYTE);
    }

    #[test]
    fn shell_scope_overrides_the_enclosing_tab() {
        enable();
        let tab = 9_000_003;
        // The shell's own groups, not `shell_bytes`: that also counts the
        // untagged allocations of tests running in parallel.
        let before = owner_bytes(SHELL);
        let buffer = in_tab(tab, || in_shell(|| vec![0u8; 4 * MEGABYTE as usize]));
        assert!(tab_bytes(tab) < MEGABYTE);
        assert!(owner_bytes(SHELL) >= before + 4 * MEGABYTE);
        drop(buffer);
    }

    #[test]
    fn worker_threads_share_their_tabs_total() {
        enable();
        let tab = 9_000_004;
        let held = in_tab(tab, || {
            let worker =
                std::thread::spawn(move || in_tab(tab, || vec![0u8; 2 * MEGABYTE as usize]));
            (vec![0u8; 2 * MEGABYTE as usize], worker.join().unwrap())
        });
        assert!(tab_bytes(tab) >= 4 * MEGABYTE);
        drop(held);
    }
}
