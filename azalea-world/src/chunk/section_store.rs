//! One copy of each different section in the process. Sections here are immutable, so a section
//! shared by content cannot carry one bot's state to another.
// Spec: specs/plans/LOAD_CONTRACT.md (Swarm world design)

use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    sync::{Arc, LazyLock, Weak},
};

use parking_lot::Mutex;

use super::Section;
use crate::palette::{Palette, PalettedContainer, PalletedContainerKind};

#[derive(Default)]
struct Store {
    by_hash: HashMap<u64, Vec<Weak<Section>>>,
    next_sweep: usize,
}

#[cfg(test)]
thread_local! {
    /// Store calls made by this thread, so a test can count them.
    pub(crate) static INTERN_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

static STORE: LazyLock<Mutex<Store>> = LazyLock::new(Default::default);

fn hash_container<S: PalletedContainerKind>(c: &PalettedContainer<S>, h: &mut DefaultHasher) {
    c.bits_per_entry.hash(h);
    match &c.palette {
        Palette::SingleValue(v) => Into::<u32>::into(*v).hash(h),
        Palette::Linear(v) | Palette::Hashmap(v) => {
            for x in v {
                Into::<u32>::into(*x).hash(h);
            }
        }
        Palette::Global => {}
    }
    c.storage.data.hash(h);
}

/// The stored section with this content. The hash finds a match; the full comparison confirms it.
// Spec: specs/plans/LOAD_CONTRACT.md "Swarm world design" item 2
pub fn intern(section: Section) -> Arc<Section> {
    let mut h = DefaultHasher::new();
    section.block_count.hash(&mut h);
    hash_container(&section.states, &mut h);
    hash_container(&section.biomes, &mut h);
    intern_with_key(section, h.finish())
}

/// [`intern`] with the hash given, so a test can force two different sections onto one key.
fn intern_with_key(section: Section, key: u64) -> Arc<Section> {
    #[cfg(test)]
    INTERN_CALLS.with(|c| c.set(c.get() + 1));
    let mut store = STORE.lock();
    let entries = store.by_hash.entry(key).or_default();
    for weak in entries.iter() {
        if let Some(found) = weak.upgrade()
            && *found == section
        {
            return found;
        }
    }
    let new = Arc::new(section);
    entries.retain(|w| w.strong_count() > 0);
    entries.push(Arc::downgrade(&new));

    // Dropped sections leave dead entries; clear them when the table has doubled.
    if store.by_hash.len() >= store.next_sweep {
        store.by_hash.retain(|_, v| {
            v.retain(|w| w.strong_count() > 0);
            !v.is_empty()
        });
        store.next_sweep = (store.by_hash.len() * 2).max(1024);
    }
    new
}

#[cfg(test)]
mod tests {
    use azalea_block::BlockState;
    use azalea_core::position::ChunkSectionBlockPos;
    use azalea_registry::builtin::BlockKind;

    use super::*;

    fn with_block(x: u8, kind: BlockKind) -> Section {
        let mut s = Section::default();
        s.get_and_set_block_state(ChunkSectionBlockPos::new(x, 0, 0), kind.into());
        s
    }

    // Spec: specs/plans/LOAD_CONTRACT.md "Swarm world design" item 2
    #[test]
    fn equal_content_gives_one_pointer() {
        let a = intern(with_block(1, BlockKind::Stone));
        let b = intern(with_block(1, BlockKind::Stone));
        assert!(Arc::ptr_eq(&a, &b));
        let c = intern(with_block(2, BlockKind::Stone));
        assert!(!Arc::ptr_eq(&a, &c), "different content, different pointer");
    }

    #[test]
    fn an_edit_never_changes_what_another_holder_reads() {
        let a = intern(with_block(3, BlockKind::Dirt));
        let b = intern(with_block(3, BlockKind::Dirt));
        // Bot A edits: copy, change, store. Bot B's pointer is untouched.
        let mut edited = Section::clone(&a);
        edited.get_and_set_block_state(ChunkSectionBlockPos::new(3, 0, 0), BlockState::AIR);
        let a2 = intern(edited);
        assert!(!Arc::ptr_eq(&a2, &b));
        assert_eq!(b.get_block_state(ChunkSectionBlockPos::new(3, 0, 0)), BlockKind::Dirt.into());
        assert_eq!(a2.get_block_state(ChunkSectionBlockPos::new(3, 0, 0)), BlockState::AIR);
    }

    #[test]
    fn a_section_is_freed_when_no_bot_points_to_it() {
        let marker = with_block(9, BlockKind::GoldBlock);
        // Count only this content: other tests share the process-wide store.
        let alive = |s: &Section| {
            STORE.lock().by_hash.values().flatten().filter_map(Weak::upgrade).filter(|x| **x == *s).count()
        };
        assert_eq!(alive(&marker), 0);
        let a = intern(marker.clone());
        assert_eq!(alive(&marker), 1);
        let b = intern(marker.clone());
        assert_eq!(alive(&marker), 1, "two holders, one entry");
        drop(a);
        assert_eq!(alive(&marker), 1, "one holder left");
        // The probe does not keep the section alive. If the store held a strong reference, the
        // section would outlive its last holder and the probe would still upgrade.
        let probe = Arc::downgrade(&b);
        drop(b);
        assert_eq!(alive(&marker), 0, "no holder, no entry");
        assert!(probe.upgrade().is_none(), "the store must not keep a section alive");
    }

    #[test]
    fn a_hash_collision_with_different_content_keeps_two_entries() {
        let one = with_block(4, BlockKind::Sand);
        let two = with_block(5, BlockKind::Gravel);
        assert!(one != two);
        let a = intern_with_key(one.clone(), 0xC0111DE);
        let b = intern_with_key(two.clone(), 0xC0111DE);
        assert!(!Arc::ptr_eq(&a, &b), "same hash, different content: no false match");
        assert_eq!(*a, one);
        assert_eq!(*b, two);
        // Each still finds itself on the same key.
        assert!(Arc::ptr_eq(&a, &intern_with_key(one, 0xC0111DE)));
        assert!(Arc::ptr_eq(&b, &intern_with_key(two, 0xC0111DE)));
    }
}
