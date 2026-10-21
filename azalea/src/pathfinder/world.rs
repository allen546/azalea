use std::{
    array,
    cell::{RefCell, UnsafeCell},
    fmt::Debug,
    mem,
    sync::Arc,
};

use azalea_block::{
    BlockState,
    properties::{self, FacingCubic, SlabKind, StairShape},
};
use azalea_core::{
    bitset::FastFixedBitSet,
    position::{BlockPos, ChunkPos, ChunkSectionBlockPos},
};
use azalea_entity::unmodelled::{WorldBorderState, is_unmodelled_block};
use azalea_physics::collision::BlockWithShape;
use azalea_registry::builtin::BlockKind;
use azalea_world::{World, palette::PalettedContainer};
use parking_lot::RwLock;
use rustc_hash::FxHashMap;

use super::{mining::MiningCache, positions::RelBlockPos};
use crate::pathfinder::positions::SmallChunkSectionPos;

const MAX_VIEW_DISTANCE: usize = 32;

/// An efficient representation of the world used for the pathfinder.
pub struct CachedWorld {
    /// The origin that the [`RelBlockPos`] types will be relative to.
    ///
    /// This is for an optimization that reduces the size of the block positions
    /// that are used by the pathfinder.
    origin: BlockPos,

    min_y: i32,
    world_lock: Arc<RwLock<World>>,

    // we use the bounded cache by default and then switch if it gets too big
    bounded_chunk_cache: RefCell<[(ChunkPos, CachedChunk); MAX_VIEW_DISTANCE * MAX_VIEW_DISTANCE]>,
    unbounded_chunk_cache: RefCell<FxHashMap<ChunkPos, CachedChunk>>,

    cached_blocks: UnsafeCell<CachedSections>,

    #[allow(clippy::type_complexity)]
    cached_mining_costs: UnsafeCell<Option<Box<[(RelBlockPos, f32)]>>>,

    /// The world border at the time the path was planned, if the server sent one (patch 34).
    border: Option<BorderWall>,
}

/// How far from the world border a cell must stay: the distance at which a player's box meets the
/// border's collision shape (`WorldBorder#isInsideCloseToBorder`: `2 * max(1, box size)`, so 2
/// blocks for a player) plus one block.
// Spec: specs/B1-movement.md §4.3
pub const BORDER_WALL_MARGIN: f64 = 3.0;

/// The world border as a rectangle, for the planner's border wall. Grim's `Simulation` check
/// flags a client that ignores the border's collision (`Entity#collectColliders`), and the fork
/// does not model it, so paths stay [`BORDER_WALL_MARGIN`] blocks inside it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BorderWall {
    pub min_x: f64,
    pub max_x: f64,
    pub min_z: f64,
    pub max_z: f64,
}

impl BorderWall {
    /// A snapshot at `tick`. A moving border takes the smaller of its size now and the size it
    /// moves to, so a shrinking border is walled where it will end up.
    // Spec: specs/B1-movement.md §4.3
    pub fn from_state(state: &WorldBorderState, tick: u64) -> Self {
        let half = state.size_at(tick).min(state.to_size) / 2.0;
        Self {
            min_x: state.center_x - half,
            max_x: state.center_x + half,
            min_z: state.center_z - half,
            max_z: state.center_z + half,
        }
    }

    /// Whether the cell is outside the border or closer to it than the margin.
    // Spec: specs/B1-movement.md §4.3
    pub fn walls(&self, pos: BlockPos) -> bool {
        let (x, z) = (pos.x as f64, pos.z as f64);
        x < self.min_x + BORDER_WALL_MARGIN
            || x + 1.0 > self.max_x - BORDER_WALL_MARGIN
            || z < self.min_z + BORDER_WALL_MARGIN
            || z + 1.0 > self.max_z - BORDER_WALL_MARGIN
    }
}

// we store `PalettedContainer`s instead of `Chunk`s or `Section`s because it
// doesn't contain any unnecessary data like heightmaps or biomes.
type CachedChunk = Box<[PalettedContainer<BlockState>]>;

pub struct CachedSections {
    pub fast_sections: Box<[Option<CachedSection>; FAST_SECTIONS_CACHE_SIZE]>,
    pub fallback_sections: Vec<CachedSection>,
}

const FAST_SECTIONS_CACHE_SIZE: usize = 16 * 16 * 16;
fn fast_section_idx(pos: SmallChunkSectionPos) -> usize {
    (pos.y as usize % 16) + (pos.x as usize % 16) * 16 + (pos.z as usize % 16) * 16 * 16
}

impl CachedSections {
    pub fn get_mut(&mut self, pos: SmallChunkSectionPos) -> Option<&mut CachedSection> {
        let idx = fast_section_idx(pos);

        if let Some(fast_item) = &mut self.fast_sections[idx]
            && fast_item.pos == pos
        {
            return Some(fast_item);
        }

        if let Some(item) = self.fallback_sections.iter_mut().find(|s| s.pos == pos) {
            return Some(item);
        }

        None
    }

    #[inline]
    pub fn insert(&mut self, section: CachedSection) {
        let idx = fast_section_idx(section.pos);

        if let item @ None = &mut self.fast_sections[idx] {
            *item = Some(section);
            return;
        }

        // this benchmarks better than pushing even when we linear search later. i guess
        // it has better cache locality?
        let index = self
            .fallback_sections
            .binary_search_by(|s| s.pos.cmp(&section.pos))
            .unwrap_or_else(|e| e);
        self.fallback_sections.insert(index, section);
    }
}
impl Default for CachedSections {
    fn default() -> Self {
        Self {
            fast_sections: (0..FAST_SECTIONS_CACHE_SIZE)
                .map(|_| None)
                .collect::<Box<[_]>>()
                .try_into()
                .unwrap(),
            fallback_sections: Default::default(),
        }
    }
}

pub struct CachedSection {
    pub pos: SmallChunkSectionPos,
    pub bitsets: Box<SectionBitsets>,
}
impl Debug for CachedSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedSection")
            .field("pos", &self.pos)
            .finish()
    }
}

#[derive(Default)]
pub struct SectionBitsets {
    /// Blocks that we can fully pass through (like air).
    pub passable: FastFixedBitSet<4096>,
    /// Blocks that we can stand on and do parkour from.
    pub solid: FastFixedBitSet<4096>,
    /// Blocks that we can stand on but might not be able to parkour from.
    pub standable: FastFixedBitSet<4096>,
    /// Water source blocks.
    pub water: FastFixedBitSet<4096>,
    /// Honey blocks, scaffolding and piston bases: the blocks whose neighbours are walled (patch 36).
    pub wall_source: FastFixedBitSet<4096>,
    /// Whether `wall_source` has any bit set: a cell whose neighbours are all in a section with
    /// none needs no lookup (patch 36 speed fix).
    pub has_wall_source: bool,
    /// Blocks on patch 30's unmodelled list (slime, bed, scaffolding, ...): lets the diagonal and
    /// parkour moves test a floor without reading its block state (patch 36 speed fix).
    pub unmodelled: FastFixedBitSet<4096>,
}

impl CachedWorld {
    pub fn new(world_lock: Arc<RwLock<World>>, origin: BlockPos) -> Self {
        let min_y = world_lock.read().chunks.min_y();
        Self {
            origin,
            min_y,
            world_lock,
            bounded_chunk_cache: RefCell::new(array::from_fn(|_| {
                (ChunkPos::new(i32::MAX, i32::MAX), Default::default())
            })),
            unbounded_chunk_cache: Default::default(),
            cached_blocks: Default::default(),
            cached_mining_costs: UnsafeCell::new(None),
            border: None,
        }
    }

    /// Wall the cells near the world border (patch 34). Without a border, no border walls.
    // Spec: specs/B1-movement.md §4.3
    pub fn with_border(mut self, border: Option<BorderWall>) -> Self {
        self.border = border;
        self
    }

    // ```
    // fn get_block_state(&self, pos: BlockPos) -> Option<BlockState> {
    //     self.with_section(ChunkSectionPos::from(pos), |section| {
    //         let state = section.get(pos.x as usize, pos.y as usize, pos.z as usize);
    //         BlockState::try_from(state).unwrap_or(BlockState::AIR)
    //     })
    // }
    // ```

    fn with_section<T>(
        &self,
        section_pos: SmallChunkSectionPos,
        f: impl FnOnce(&azalea_world::palette::PalettedContainer<BlockState>) -> T,
    ) -> Option<T> {
        if section_pos.y * 16 < self.min_y {
            // y position is out of bounds
            return None;
        }

        let chunk_pos = ChunkPos::new(section_pos.x as i32, section_pos.z as i32);
        let section_index =
            azalea_world::chunk::section_index(section_pos.y * 16, self.min_y) as usize;

        let mut cache_idx = 0;

        let mut unbounded_chunk_cache = self.unbounded_chunk_cache.borrow_mut();
        let mut bounded_chunk_cache = self.bounded_chunk_cache.borrow_mut();
        if unbounded_chunk_cache.is_empty() {
            const D: i32 = MAX_VIEW_DISTANCE as i32;
            let cache_x = i32::rem_euclid(chunk_pos.x, D) * D;
            let cache_z = i32::rem_euclid(chunk_pos.z, D);
            cache_idx = (cache_x + cache_z) as usize;

            // get section from cache
            if !bounded_chunk_cache[cache_idx].1.is_empty() {
                if bounded_chunk_cache[cache_idx].0 != chunk_pos {
                    // switch to the unbounded cache :(

                    for (moving_chunk_pos, moving_chunk) in bounded_chunk_cache.iter_mut() {
                        if !moving_chunk.is_empty() {
                            unbounded_chunk_cache
                                .insert(*moving_chunk_pos, mem::take(moving_chunk));
                        }
                    }
                }

                let sections = &bounded_chunk_cache[cache_idx].1;
                if section_index >= sections.len() {
                    // y position is out of bounds
                    return None;
                };
                let section = &sections[section_index];
                return Some(f(section));
            }
        } else if let Some(sections) = unbounded_chunk_cache.get(&chunk_pos) {
            if section_index >= sections.len() {
                // y position is out of bounds
                return None;
            };
            let section = &sections[section_index];
            return Some(f(section));
        }

        let bench_t = azalea_client::bench_metrics::Timer::new(azalea_client::bench_metrics::Kind::PfChunkClone);
        azalea_client::bench_metrics::count(azalea_client::bench_metrics::counter::CHUNK_CLONES, 1);
        let world = self.world_lock.read();
        let chunk = world.chunks.get(&chunk_pos)?;
        let chunk = chunk.read();

        let sections = chunk
            .sections
            .iter()
            .map(|section| section.states.clone())
            .collect::<Box<[PalettedContainer<BlockState>]>>();
        drop(bench_t);

        if section_index >= sections.len() {
            // y position is out of bounds
            return None;
        };

        let section = &sections[section_index];
        let r = f(section);

        // add the sections to the chunk cache
        if unbounded_chunk_cache.is_empty() {
            bounded_chunk_cache[cache_idx] = (chunk_pos, sections);
        } else {
            unbounded_chunk_cache.insert(chunk_pos, sections);
        }

        Some(r)
    }

    fn calculate_bitsets_for_section(&self, section_pos: SmallChunkSectionPos) -> CachedSection {
        let _bench_t = azalea_client::bench_metrics::Timer::new(azalea_client::bench_metrics::Kind::PfBitsets);
        azalea_client::bench_metrics::count(azalea_client::bench_metrics::counter::SECTIONS_BUILT, 1);
        if azalea_client::bench_metrics::enabled() && std::env::var_os("BENCH_SECTION_UNIQUE").is_some() {
            static SEEN: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashSet<(i32, i32, i32)>>> = std::sync::OnceLock::new();
            let seen = SEEN.get_or_init(Default::default);
            if seen.lock().insert((section_pos.x as i32, section_pos.y as i32, section_pos.z as i32)) {
                azalea_client::bench_metrics::count(azalea_client::bench_metrics::counter::SECTIONS_UNIQUE, 1);
            }
        }
        let bitsets = self
            .with_section(section_pos, |section| {
                let mut bitsets = SectionBitsets {
                    passable: FastFixedBitSet::<4096>::new(),
                    solid: FastFixedBitSet::<4096>::new(),
                    standable: FastFixedBitSet::<4096>::new(),
                    water: FastFixedBitSet::<4096>::new(),
                    wall_source: FastFixedBitSet::<4096>::new(),
                    has_wall_source: false,
                    unmodelled: FastFixedBitSet::<4096>::new(),
                };

                for i in 0..4096 {
                    let block_state = section.get_at_index(i);
                    if is_block_state_passable(block_state) {
                        bitsets.passable.set(i);
                    }
                    if is_block_state_solid(block_state) {
                        bitsets.solid.set(i);
                    }
                    if is_block_state_standable(block_state) {
                        bitsets.standable.set(i);
                    }
                    if is_block_state_water(block_state) {
                        bitsets.water.set(i);
                    }
                    if !block_state.is_air() {
                        let flags = state_flags(block_state);
                        if flags & FLAG_WALL_SOURCE != 0 {
                            bitsets.wall_source.set(i);
                            bitsets.has_wall_source = true;
                        }
                        if flags & FLAG_UNMODELLED != 0 {
                            bitsets.unmodelled.set(i);
                        }
                    }
                }
                Box::new(bitsets)
            })
            .unwrap_or_default();

        CachedSection {
            pos: section_pos,
            bitsets,
        }
    }

    fn check_bitset_for_block<T>(
        &self,
        pos: BlockPos,
        cb: impl FnOnce(&SectionBitsets, usize) -> T,
    ) -> T {
        let (section_pos, section_block_pos) = (
            SmallChunkSectionPos::from(pos),
            ChunkSectionBlockPos::from(pos),
        );
        let index = u16::from(section_block_pos) as usize;
        // SAFETY: we're only accessing this from one thread
        let cached_blocks = unsafe { &mut *self.cached_blocks.get() };
        if let Some(cached) = cached_blocks.get_mut(section_pos) {
            return cb(&cached.bitsets, index);
        }

        let cached = self.calculate_bitsets_for_section(section_pos);
        let passable = cb(&cached.bitsets, index);
        cached_blocks.insert(cached);
        passable
    }

    pub fn is_block_passable(&self, pos: RelBlockPos) -> bool {
        self.is_block_pos_passable(pos.apply(self.origin))
    }
    fn is_block_pos_passable(&self, pos: BlockPos) -> bool {
        let (passable, own_has_source) = self.check_bitset_for_block(pos, |bitsets, index| {
            (bitsets.passable.index(index), bitsets.has_wall_source)
        });
        passable
            && !self.border.as_ref().is_some_and(|border| border.walls(pos))
            && !self.neighbour_wall_in(pos, own_has_source)
    }

    /// Whether the cell is walled by something other than its own block: a neighbour (honey, a
    /// piston face) or the world border. Such a cell may be air, so the planner must not mine it
    /// either.
    // Spec: specs/B1-movement.md §4.3
    pub fn is_wall_cell(&self, pos: RelBlockPos) -> bool {
        self.is_wall_cell_at_pos(pos.apply(self.origin))
    }
    fn is_wall_cell_at_pos(&self, pos: BlockPos) -> bool {
        if let Some(border) = &self.border
            && border.walls(pos)
        {
            return true;
        }
        self.is_neighbour_wall(pos)
    }

    /// A cell beside a honey block (`HoneyBlock#entityInside` slides a bot that touches its side),
    /// or in front of a piston base's face (`PistonBaseBlock#triggerEvent` turns it into a moving
    /// block), is walled. Queried for each passable cell: the source bitset keeps it to six
    /// lookups, and a state is read only when a source is found.
    // Spec: specs/B1-movement.md §4.3
    fn is_neighbour_wall(&self, pos: BlockPos) -> bool {
        self.neighbour_wall(pos, true)
    }

    /// `fast` skips a neighbour in the cell's own section when that section has no wall source;
    /// a neighbour in another section is always looked up. Both give the same answer (tested).
    // Spec: specs/B1-movement.md §4.3
    fn neighbour_wall(&self, pos: BlockPos, fast: bool) -> bool {
        let own_has_source =
            !fast || self.check_bitset_for_block(pos, |bitsets, _| bitsets.has_wall_source);
        self.neighbour_wall_in(pos, own_has_source)
    }

    /// The check itself, given whether the cell's own section holds a wall source.
    // Spec: specs/B1-movement.md §4.3
    #[inline(always)]
    fn neighbour_wall_in(&self, pos: BlockPos, own_has_source: bool) -> bool {
        // interior of a section with no source: all six neighbours are in that section
        if !own_has_source
            && (pos.x & 15) % 15 != 0
            && (pos.y & 15) % 15 != 0
            && (pos.z & 15) % 15 != 0
        {
            return false;
        }
        self.neighbour_wall_scan(pos, own_has_source)
    }

    #[inline(never)]
    fn neighbour_wall_scan(&self, pos: BlockPos, own_has_source: bool) -> bool {
        const DIRS: [(i32, i32, i32, FacingCubic); 6] = [
            (0, 0, -1, FacingCubic::South),
            (1, 0, 0, FacingCubic::West),
            (0, 0, 1, FacingCubic::North),
            (-1, 0, 0, FacingCubic::East),
            (0, 1, 0, FacingCubic::Down),
            (0, -1, 0, FacingCubic::Up),
        ];
        for (i, &(dx, dy, dz, toward_us)) in DIRS.iter().enumerate() {
            let n = BlockPos::new(pos.x + dx, pos.y + dy, pos.z + dz);
            if !own_has_source
                && (n.x >> 4, n.y >> 4, n.z >> 4) == (pos.x >> 4, pos.y >> 4, pos.z >> 4)
            {
                continue;
            }
            if !self.check_bitset_for_block(n, |bitsets, index| bitsets.wall_source.index(index)) {
                continue;
            }
            let state = self.get_block_state_at_pos(n);
            if matches!(BlockKind::from(state), BlockKind::HoneyBlock | BlockKind::Scaffolding) {
                // honey is walled on its four sides only; so is scaffolding, whose side has no
                // collision in vanilla (`ScaffoldingBlock#getCollisionShape`) but whose shape the
                // fork collides with, so a bot hugging its corner draws Grim `Simulation`
                if i < 4 {
                    return true;
                }
            } else if state.property::<FacingCubic>() == Some(toward_us) {
                return true;
            }
        }
        false
    }

    /// Whether the block is on patch 30's unmodelled list, from the cached bitset.
    // Spec: specs/B1-movement.md §4.3
    pub fn is_block_unmodelled(&self, pos: RelBlockPos) -> bool {
        self.check_bitset_for_block(pos.apply(self.origin), |bitsets, index| {
            bitsets.unmodelled.index(index)
        })
    }

    pub fn is_block_water(&self, pos: RelBlockPos) -> bool {
        self.is_block_pos_water(pos.apply(self.origin))
    }
    fn is_block_pos_water(&self, pos: BlockPos) -> bool {
        self.check_bitset_for_block(pos, |bitsets, index| bitsets.water.index(index))
    }

    /// Get the block state at the given position.
    ///
    /// This is relatively slow, so you should avoid it whenever possible.
    pub fn get_block_state(&self, pos: RelBlockPos) -> BlockState {
        self.get_block_state_at_pos(pos.apply(self.origin))
    }

    fn get_block_state_at_pos(&self, pos: BlockPos) -> BlockState {
        let (section_pos, section_block_pos) = (
            SmallChunkSectionPos::from(pos),
            ChunkSectionBlockPos::from(pos),
        );
        let index = u16::from(section_block_pos) as usize;

        self.with_section(section_pos, |section| section.get_at_index(index))
            .unwrap_or_default()
    }

    pub fn is_block_solid(&self, pos: RelBlockPos) -> bool {
        self.is_block_pos_solid(pos.apply(self.origin))
    }
    pub fn is_block_standable(&self, pos: RelBlockPos) -> bool {
        self.is_block_pos_standable(pos.apply(self.origin))
    }

    /// Standable, or on the unmodelled list: a floor a jump or diagonal must not cross as a gap
    /// (patch 36), from one section lookup.
    // Spec: specs/B1-movement.md §4.3
    pub fn is_block_standable_or_unmodelled(&self, pos: RelBlockPos) -> bool {
        self.check_bitset_for_block(pos.apply(self.origin), |bitsets, index| {
            bitsets.standable.index(index) || bitsets.unmodelled.index(index)
        })
    }

    fn is_block_pos_solid(&self, pos: BlockPos) -> bool {
        self.check_bitset_for_block(pos, |bitsets, index| bitsets.solid.index(index))
    }
    fn is_block_pos_standable(&self, pos: BlockPos) -> bool {
        self.check_bitset_for_block(pos, |bitsets, index| bitsets.standable.index(index))
    }

    /// Returns how much it costs to break this block.
    ///
    /// Returns 0 if the block is already passable.
    pub fn cost_for_breaking_block(&self, pos: RelBlockPos, mining_cache: &MiningCache) -> f32 {
        let cached_mining_costs = self.cached_mining_costs();

        let hash_index = calculate_cached_mining_costs_index(pos);
        let &(cached_pos, potential_cost) =
            unsafe { cached_mining_costs.get_unchecked(hash_index) };
        if cached_pos == pos {
            return potential_cost;
        }

        let cost = self.uncached_cost_for_breaking_block(pos, mining_cache);
        unsafe {
            *cached_mining_costs.get_unchecked_mut(hash_index) = (pos, cost);
        };

        cost
    }

    // this is fine because pathfinding is single-threaded
    #[allow(clippy::mut_from_ref)]
    fn cached_mining_costs(&self) -> &mut [(RelBlockPos, f32)] {
        // SAFETY: again, pathfinding is single-threaded
        let cached_mining_costs = unsafe { &mut *self.cached_mining_costs.get() };
        if let Some(cached_mining_costs) = cached_mining_costs {
            return cached_mining_costs;
        }
        // delay initialization so we don't have to create this if it's unused

        // this uses about 2mb of memory. it *really* helps though.
        *cached_mining_costs = Some(
            vec![(RelBlockPos::new(i16::MAX, i32::MAX, i16::MAX), 0.); CACHED_MINING_COSTS_SIZE]
                .into(),
        );

        cached_mining_costs.as_mut().unwrap()
    }

    fn uncached_cost_for_breaking_block(
        &self,
        pos: RelBlockPos,
        mining_cache: &MiningCache,
    ) -> f32 {
        if self.is_block_passable(pos) {
            // if the block is passable then it doesn't need to be broken
            return 0.;
        }
        if self.is_wall_cell(pos) {
            // patch 34: a cell walled by a neighbour or the border is not a block to break
            return f32::INFINITY;
        }

        let rel_pos = pos;
        let pos = pos.apply(self.origin);

        let (section_pos, section_block_pos) = (
            SmallChunkSectionPos::from(pos),
            ChunkSectionBlockPos::from(pos),
        );

        // we use this as an optimization to avoid getting the section again if the
        // block is in the same section
        let up_is_in_same_section = section_block_pos.y != 15;
        let north_is_in_same_section = section_block_pos.z != 0;
        let east_is_in_same_section = section_block_pos.x != 15;
        let south_is_in_same_section = section_block_pos.z != 15;
        let west_is_in_same_section = section_block_pos.x != 0;

        let mut is_falling_block_above = false;

        let Some(mut mining_cost) = self.with_section(section_pos, |section| {
            let block_state = section.get_at_index(u16::from(section_block_pos) as usize);
            let mining_cost = mining_cache.cost_for(block_state);

            if mining_cost == f32::INFINITY {
                // the block is unbreakable
                return f32::INFINITY;
            }

            // if there's a falling block or liquid above this block, abort
            if up_is_in_same_section {
                let up_block = section.get_at_index(u16::from(section_block_pos.up(1)) as usize);
                if mining_cache.is_liquid(up_block) {
                    return f32::INFINITY;
                }
                if mining_cache.is_falling_block(up_block) {
                    is_falling_block_above = true;
                }
            }

            // if there's a liquid to the north of this block, abort
            if north_is_in_same_section {
                let north_block =
                    section.get_at_index(u16::from(section_block_pos.north(1)) as usize);
                if mining_cache.is_liquid(north_block) {
                    return f32::INFINITY;
                }
            }

            // liquid to the east
            if east_is_in_same_section {
                let east_block =
                    section.get_at_index(u16::from(section_block_pos.east(1)) as usize);
                if mining_cache.is_liquid(east_block) {
                    return f32::INFINITY;
                }
            }

            // liquid to the south
            if south_is_in_same_section {
                let south_block =
                    section.get_at_index(u16::from(section_block_pos.south(1)) as usize);
                if mining_cache.is_liquid(south_block) {
                    return f32::INFINITY;
                }
            }

            // liquid to the west
            if west_is_in_same_section {
                let west_block =
                    section.get_at_index(u16::from(section_block_pos.west(1)) as usize);
                if mining_cache.is_liquid(west_block) {
                    return f32::INFINITY;
                }
            }

            // the block is probably safe to break, we'll have to check the adjacent blocks
            // that weren't in the same section next though
            mining_cost
        }) else {
            // the chunk isn't loaded
            let cost = if self.is_block_pos_solid(pos) {
                // assume it's unbreakable if it's solid and out of render distance
                f32::INFINITY
            } else {
                0.
            };
            return cost;
        };

        if mining_cost == f32::INFINITY {
            // the block is unbreakable
            return f32::INFINITY;
        }

        fn check_should_avoid_this_block(
            world: &CachedWorld,
            pos: BlockPos,
            check: impl FnOnce(BlockState) -> bool,
        ) -> bool {
            let block_state = world
                .with_section(SmallChunkSectionPos::from(pos), |section| {
                    section.get_at_index(u16::from(ChunkSectionBlockPos::from(pos)) as usize)
                })
                .unwrap_or_default();
            check(block_state)
        }

        // check the adjacent blocks that weren't in the same section
        if !up_is_in_same_section
            && check_should_avoid_this_block(self, pos.up(1), |b| {
                if mining_cache.is_falling_block(b) {
                    is_falling_block_above = true;
                }
                mining_cache.is_liquid(b)
            })
        {
            return f32::INFINITY;
        }
        if !north_is_in_same_section
            && check_should_avoid_this_block(self, pos.north(1), |b| mining_cache.is_liquid(b))
        {
            return f32::INFINITY;
        }
        if !east_is_in_same_section
            && check_should_avoid_this_block(self, pos.east(1), |b| mining_cache.is_liquid(b))
        {
            return f32::INFINITY;
        }
        if !south_is_in_same_section
            && check_should_avoid_this_block(self, pos.south(1), |b| mining_cache.is_liquid(b))
        {
            return f32::INFINITY;
        }
        if !west_is_in_same_section
            && check_should_avoid_this_block(self, pos.west(1), |b| mining_cache.is_liquid(b))
        {
            return f32::INFINITY;
        }

        if is_falling_block_above {
            mining_cost += self.cost_for_breaking_block(rel_pos.up(1), mining_cache);
        }

        mining_cost
    }

    /// Whether this block and the block above are passable
    pub fn is_passable(&self, pos: RelBlockPos) -> bool {
        self.is_passable_at_block_pos(pos.apply(self.origin))
    }
    fn is_passable_at_block_pos(&self, pos: BlockPos) -> bool {
        self.is_block_pos_passable(pos) && self.is_block_pos_passable(pos.up(1))
    }

    pub fn cost_for_passing(&self, pos: RelBlockPos, mining_cache: &MiningCache) -> f32 {
        self.cost_for_breaking_block(pos, mining_cache)
            + self.cost_for_breaking_block(pos.up(1), mining_cache)
    }

    /// Whether we can stand in this position.
    ///
    /// Checks if the block below is solid, and that the two blocks above that
    /// are passable.
    pub fn is_standable(&self, pos: RelBlockPos) -> bool {
        self.is_standable_at_block_pos(pos.apply(self.origin))
    }
    fn is_standable_at_block_pos(&self, pos: BlockPos) -> bool {
        self.is_block_pos_standable(pos.down(1)) && self.is_passable_at_block_pos(pos)
    }

    pub fn cost_for_standing(&self, pos: RelBlockPos, mining_cache: &MiningCache) -> f32 {
        if !self.is_block_standable(pos.down(1)) {
            return f32::INFINITY;
        }
        self.cost_for_passing(pos, mining_cache)
    }

    /// Get the amount of air/passable blocks until the next non-passable block
    /// below this one.
    pub fn fall_distance(&self, pos: RelBlockPos) -> u32 {
        let mut distance = 0;
        let mut current_pos = pos.down(1);
        while self.is_block_passable(current_pos) {
            distance += 1;
            current_pos = current_pos.down(1);

            if current_pos.y < self.min_y {
                return u32::MAX;
            }
        }
        distance
    }

    pub fn origin(&self) -> BlockPos {
        self.origin
    }
}

const CACHED_MINING_COSTS_SIZE: usize = 2usize.pow(18);
fn calculate_cached_mining_costs_index(pos: RelBlockPos) -> usize {
    // create an 18-bit index by taking the bottom bits from each axis

    const X_BITS: usize = 6;
    const Y_BITS: usize = 6;
    const Z_BITS: usize = 6;

    const X_MASK: usize = (1 << X_BITS) - 1;
    const Y_MASK: usize = (1 << Y_BITS) - 1;
    const Z_MASK: usize = (1 << Z_BITS) - 1;

    let hash_index = ((pos.x as usize & X_MASK) << (Y_BITS + Z_BITS))
        | ((pos.z as usize & Z_MASK) << Y_BITS)
        | (pos.y as usize & Y_MASK);
    debug_assert!(hash_index < CACHED_MINING_COSTS_SIZE);
    hash_index
}

/// Whether our client could pass through this block.
pub fn is_block_state_passable(block_state: BlockState) -> bool {
    // i already tried optimizing this by having it cache in an IntMap/FxHashMap but
    // it wasn't measurably faster

    if block_state.is_air() {
        // fast path
        return true;
    }
    if !block_state.is_collision_shape_empty() {
        return false;
    }
    let registry_block = BlockKind::from(block_state);
    if is_block_state_unmodelled(block_state) {
        // Patch 34: the block moves the bot in a way the fork does not model (bubble column,
        // moving piston; the rest have collision). One list with the patch 30 disconnect gate.
        return false;
    }
    if registry_block == BlockKind::Water {
        return false;
    }
    if block_state
        .property::<azalea_block::properties::Waterlogged>()
        .unwrap_or_default()
    {
        return false;
    }
    if registry_block == BlockKind::Lava {
        return false;
    }
    // block.waterlogged currently doesn't account for seagrass and some other water
    // blocks
    if block_state == BlockKind::Seagrass.into() {
        return false;
    }

    // don't walk into fire
    if registry_block == BlockKind::Fire || registry_block == BlockKind::SoulFire {
        return false;
    }

    if registry_block == BlockKind::PowderSnow {
        // we can't jump out of powder snow
        return false;
    }

    if registry_block == BlockKind::Cobweb {
        // Null fork: a cobweb slows movement to a fraction of normal speed, so
        // the planned timing would be wrong (`WebBlock#entityInside`).
        return false;
    }

    if registry_block == BlockKind::SweetBerryBush {
        // these hurt us
        return false;
    }

    true
}

/// Whether this block has a solid hitbox at the top (i.e. we can stand on it
/// and do parkour from it).
#[inline]
#[expect(clippy::needless_bool, reason = "Kept for future improvements")]
pub fn is_block_state_solid(block_state: BlockState) -> bool {
    if block_state.is_air() {
        // fast path
        return false;
    }

    // Patch 34: slime blocks, beds, scaffolding and moving pistons are not planned on.
    if is_block_state_unmodelled(block_state) {
        return false;
    }

    if block_state.is_collision_shape_full() {
        // hazard
        if block_state == BlockState::from(BlockKind::MagmaBlock) {
            return false;
        };

        return true;
    }

    if matches!(
        block_state.property::<properties::SlabKind>(),
        Some(properties::SlabKind::Top | properties::SlabKind::Double)
    ) {
        // top slabs
        return true;
    }

    let block = BlockKind::from(block_state);
    // solid enough
    if matches!(block, BlockKind::DirtPath | BlockKind::Farmland) {
        return true;
    }

    false
}

/// Whether we can stand on this block (but not necessarily do parkour jumps
/// from it).
pub fn is_block_state_standable(block_state: BlockState) -> bool {
    if block_state.is_air() {
        // fast path
        return false;
    }

    if is_block_state_unmodelled(block_state) {
        // Patch 34: see `is_block_state_solid`.
        return false;
    }

    if is_block_state_solid(block_state) {
        return true;
    }

    if block_state.property::<SlabKind>().is_some()
        || block_state.property::<StairShape>().is_some()
    {
        return true;
    }

    false
}

const FLAG_UNMODELLED: u8 = 1;
const FLAG_WALL_SOURCE: u8 = 2;

/// Per-state flags for the wall checks, built once: the section builder asks four questions of
/// every state of every section, so a table read replaces a kind lookup each time (the bench).
fn state_flags(block_state: BlockState) -> u8 {
    static FLAGS: std::sync::OnceLock<Box<[u8]>> = std::sync::OnceLock::new();
    let flags = FLAGS.get_or_init(|| {
        let mut v = Vec::new();
        let mut id = 0u32;
        while let Ok(state) = BlockState::try_from(id) {
            let kind = BlockKind::from(state);
            let mut f = 0;
            if is_unmodelled_block(kind) {
                f |= FLAG_UNMODELLED;
            }
            if matches!(kind, BlockKind::HoneyBlock | BlockKind::Scaffolding | BlockKind::Piston | BlockKind::StickyPiston) {
                f |= FLAG_WALL_SOURCE;
            }
            v.push(f);
            id += 1;
        }
        v.into_boxed_slice()
    });
    flags[block_state.id() as usize]
}

/// Blocks on patch 30's list of unmodelled blocks (one table with the disconnect gate).
// Spec: specs/B1-movement.md §4.3
pub fn is_block_state_unmodelled(block_state: BlockState) -> bool {
    state_flags(block_state) & FLAG_UNMODELLED != 0
}

/// Honey blocks, scaffolding and piston bases, whose neighbouring cells the planner walls (patch 36).
// Spec: specs/B1-movement.md §4.3
pub fn is_block_state_wall_source(block_state: BlockState) -> bool {
    state_flags(block_state) & FLAG_WALL_SOURCE != 0
}

pub fn is_block_state_water(block_state: BlockState) -> bool {
    // only the default blockstate, which is source blocks
    block_state == BlockState::from(BlockKind::Water)
}

#[cfg(test)]
mod tests {
    use azalea_world::{Chunk, ChunkStorage, PartialWorld};

    use super::*;

    #[test]
    fn test_is_passable() {
        let mut partial_world = PartialWorld::default();
        let mut world = ChunkStorage::default();

        partial_world
            .chunks
            .set(&ChunkPos { x: 0, z: 0 }, Some(Chunk::default()), &mut world);
        partial_world.chunks.set_block_state(
            BlockPos::new(0, 0, 0),
            BlockKind::Stone.into(),
            &world,
        );
        partial_world
            .chunks
            .set_block_state(BlockPos::new(0, 1, 0), BlockState::AIR, &world);

        let ctx = CachedWorld::new(Arc::new(RwLock::new(world.into())), BlockPos::default());
        assert!(!ctx.is_block_pos_passable(BlockPos::new(0, 0, 0)));
        assert!(ctx.is_block_pos_passable(BlockPos::new(0, 1, 0)));
    }

    // A cobweb has no collision but slows the bot to a fraction of its speed, so the
    // planner treats it as not passable (patch 26).
    #[test]
    fn test_cobweb_is_not_passable() {
        assert!(!is_block_state_passable(BlockKind::Cobweb.into()));
        assert!(is_block_state_passable(BlockState::AIR));
    }

    #[test]
    fn test_is_solid() {
        let mut partial_world = PartialWorld::default();
        let mut world = ChunkStorage::default();
        partial_world
            .chunks
            .set(&ChunkPos { x: 0, z: 0 }, Some(Chunk::default()), &mut world);
        partial_world.chunks.set_block_state(
            BlockPos::new(0, 0, 0),
            BlockKind::Stone.into(),
            &world,
        );
        partial_world
            .chunks
            .set_block_state(BlockPos::new(0, 1, 0), BlockState::AIR, &world);

        let ctx = CachedWorld::new(Arc::new(RwLock::new(world.into())), BlockPos::default());
        assert!(ctx.is_block_pos_solid(BlockPos::new(0, 0, 0)));
        assert!(!ctx.is_block_pos_solid(BlockPos::new(0, 1, 0)));
    }

    #[test]
    fn test_is_standable() {
        let mut partial_world = PartialWorld::default();
        let mut world = ChunkStorage::default();
        partial_world
            .chunks
            .set(&ChunkPos { x: 0, z: 0 }, Some(Chunk::default()), &mut world);
        partial_world.chunks.set_block_state(
            BlockPos::new(0, 0, 0),
            BlockKind::Stone.into(),
            &world,
        );
        partial_world
            .chunks
            .set_block_state(BlockPos::new(0, 1, 0), BlockState::AIR, &world);
        partial_world
            .chunks
            .set_block_state(BlockPos::new(0, 2, 0), BlockState::AIR, &world);
        partial_world
            .chunks
            .set_block_state(BlockPos::new(0, 3, 0), BlockState::AIR, &world);

        let ctx = CachedWorld::new(Arc::new(RwLock::new(world.into())), BlockPos::default());
        assert!(ctx.is_standable_at_block_pos(BlockPos::new(0, 1, 0)));
        assert!(!ctx.is_standable_at_block_pos(BlockPos::new(0, 0, 0)));
        assert!(!ctx.is_standable_at_block_pos(BlockPos::new(0, 2, 0)));
    }

    // ---- patch 34: walls for blocks whose effect on motion the fork does not model ----

    use azalea_block::BlockStates;
    use azalea_entity::unmodelled::WorldBorderState;

    use crate::pathfinder::{
        astar::{self, PathfinderTimeout, a_star},
        call_successors_fn,
        custom_state::CustomPathfinderStateRef,
        goals::{BlockPosGoal, Goal},
        mining::MiningCache,
        moves,
    };

    /// A 16x16 stone floor at y = 0 in chunk (0, 0), plus `blocks`, everything else air.
    fn floor_world(blocks: &[(BlockPos, BlockState)]) -> (ChunkStorage, PartialWorld) {
        let mut partial_world = PartialWorld::default();
        let mut world = ChunkStorage::default();
        partial_world
            .chunks
            .set(&ChunkPos { x: 0, z: 0 }, Some(Chunk::default()), &mut world);
        for x in 0..16 {
            for z in 0..16 {
                partial_world.chunks.set_block_state(
                    BlockPos::new(x, 0, z),
                    BlockKind::Stone.into(),
                    &world,
                );
            }
        }
        for &(pos, state) in blocks {
            partial_world.chunks.set_block_state(pos, state, &world);
        }
        (world, partial_world)
    }

    /// A cached world that keeps the partial world alive (it owns the chunks).
    struct TestWorld {
        world: CachedWorld,
        _partial: PartialWorld,
    }
    impl std::ops::Deref for TestWorld {
        type Target = CachedWorld;
        fn deref(&self) -> &CachedWorld {
            &self.world
        }
    }
    impl TestWorld {
        fn with_border(self, border: Option<BorderWall>) -> Self {
            Self { world: self.world.with_border(border), _partial: self._partial }
        }
    }

    fn cached(parts: (ChunkStorage, PartialWorld)) -> TestWorld {
        cached_at(parts, BlockPos::default())
    }

    fn cached_at((world, partial): (ChunkStorage, PartialWorld), origin: BlockPos) -> TestWorld {
        TestWorld {
            world: CachedWorld::new(Arc::new(RwLock::new(world.into())), origin),
            _partial: partial,
        }
    }

    fn piston_facing(kind: BlockKind, facing: FacingCubic, extended: bool) -> BlockState {
        BlockStates::from(kind)
            .into_iter()
            .find(|s| {
                s.property::<FacingCubic>() == Some(facing)
                    && s.property::<properties::Extended>().is_some_and(|e| e == extended)
            })
            .unwrap()
    }

    // The planner reads the patch 30 gate's list: no kind in it is passable, solid or standable
    // for any of its states (bubble column and moving piston have no collision, so the first
    // is the one that matters for them).
    #[test]
    fn walled_kinds_are_not_planned_through() {
        let mut n = 0;
        for kind in (0u32..).map_while(|i| BlockKind::try_from(i).ok()) {
            if !is_unmodelled_block(kind) {
                continue;
            }
            n += 1;
            for state in BlockStates::from(kind) {
                assert!(!is_block_state_passable(state), "{kind:?} passable");
                assert!(!is_block_state_solid(state), "{kind:?} solid");
                assert!(!is_block_state_standable(state), "{kind:?} standable");
            }
        }
        assert_eq!(n, 20);
    }

    // The section fast path walls exactly the cells the full six-lookup check walls, with honey,
    // scaffolding and pistons on a section edge and in the next section, and with none at all.
    #[test]
    fn neighbour_wall_fast_path_matches_slow_check() {
        let honey = BlockState::from(BlockKind::HoneyBlock);
        let scaf = BlockState::from(BlockKind::Scaffolding);
        let sets: Vec<Vec<(BlockPos, BlockState)>> = vec![
            vec![],
            vec![(BlockPos::new(16, 1, 5), honey)],
            vec![(BlockPos::new(15, 1, 5), scaf), (BlockPos::new(3, 1, 16), honey)],
            vec![
                (BlockPos::new(16, 1, 20), piston_facing(BlockKind::Piston, FacingCubic::West, false)),
                (BlockPos::new(8, 16, 8), honey),
            ],
            vec![(BlockPos::new(9, 15, 9), honey), (BlockPos::new(-1, 1, 5), scaf)],
        ];
        for blocks in sets {
            let mut partial = PartialWorld::default();
            let mut storage = ChunkStorage::default();
            for cx in -1..=2 {
                for cz in -1..=2 {
                    partial.chunks.set(&ChunkPos { x: cx, z: cz }, Some(Chunk::default()), &mut storage);
                }
            }
            for &(pos, state) in &blocks {
                partial.chunks.set_block_state(pos, state, &storage);
            }
            let w = cached((storage, partial));
            let mut walled = 0;
            for x in -2..=34 {
                for y in -1..=18 {
                    for z in -2..=34 {
                        let p = BlockPos::new(x, y, z);
                        let slow = w.neighbour_wall(p, false);
                        assert_eq!(w.neighbour_wall(p, true), slow, "{p:?} {blocks:?}");
                        walled += slow as usize;
                    }
                }
            }
            assert_eq!(walled > 0, !blocks.is_empty(), "{blocks:?}");
        }
    }

    // Each per-state wall through the cached queries; a plain stone floor still stands.
    #[test]
    fn per_state_walls_in_the_cached_world() {
        for kind in [
            BlockKind::BubbleColumn,
            BlockKind::SlimeBlock,
            BlockKind::Scaffolding,
            BlockKind::MovingPiston,
            BlockKind::RedBed,
        ] {
            let world = cached(floor_world(&[(BlockPos::new(5, 0, 5), kind.into())]));
            // the block itself, as a floor: cannot stand on it
            assert!(!world.is_standable_at_block_pos(BlockPos::new(5, 1, 5)), "{kind:?} floor");
            // the block in the feet cell: cannot pass
            let world = cached(floor_world(&[(BlockPos::new(5, 1, 5), kind.into())]));
            assert!(!world.is_passable_at_block_pos(BlockPos::new(5, 1, 5)), "{kind:?} feet");
            assert!(!world.is_passable_at_block_pos(BlockPos::new(5, 0, 5)), "{kind:?} head");
            if kind != BlockKind::Scaffolding {
                // scaffolding also walls its sides (see `scaffolding_walls_its_sides`)
                assert!(world.is_standable_at_block_pos(BlockPos::new(4, 1, 5)), "{kind:?} beside");
            }
        }
    }

    // Honey: the feet cell or the head cell beside it is walled; above, below and away are not.
    #[test]
    fn honey_walls_its_sides_at_body_height() {
        let honey = BlockState::from(BlockKind::HoneyBlock);
        // honey at feet height
        let w = cached(floor_world(&[(BlockPos::new(5, 1, 5), honey)]));
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let side = BlockPos::new(5 + dx, 1, 5 + dz);
            assert!(!w.is_block_pos_passable(side), "feet cell {side:?}");
            assert!(!w.is_passable_at_block_pos(side));
            assert!(!w.is_standable_at_block_pos(side));
        }
        assert!(w.is_block_pos_passable(BlockPos::new(5, 2, 5)), "above honey");
        assert!(w.is_standable_at_block_pos(BlockPos::new(3, 1, 5)), "two away");
        assert!(w.is_standable_at_block_pos(BlockPos::new(4, 1, 4)), "diagonal");
        // honey at head height: the feet cell beside it is not passable (its head cell is walled)
        let w = cached(floor_world(&[(BlockPos::new(5, 2, 5), honey)]));
        assert!(!w.is_passable_at_block_pos(BlockPos::new(4, 1, 5)));
        assert!(w.is_block_pos_passable(BlockPos::new(4, 1, 5)));
        // the cell under honey is not a side
        let w = cached(floor_world(&[(BlockPos::new(5, 3, 5), honey)]));
        assert!(w.is_standable_at_block_pos(BlockPos::new(5, 1, 5)));
        assert!(w.is_block_pos_passable(BlockPos::new(5, 2, 5)));
    }

    // Scaffolding: the cells beside it at body height are walled like honey's; above is not.
    #[test]
    fn scaffolding_walls_its_sides() {
        let w = cached(floor_world(&[(BlockPos::new(5, 1, 5), BlockKind::Scaffolding.into())]));
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            assert!(!w.is_block_pos_passable(BlockPos::new(5 + dx, 1, 5 + dz)), "side ({dx}, {dz})");
        }
        assert!(w.is_block_pos_passable(BlockPos::new(4, 1, 4)), "diagonal");
        assert!(w.is_block_pos_passable(BlockPos::new(3, 1, 5)), "two away");
    }

    // Piston faces, each facing, sticky or not, retracted or extended: only the cell in front of
    // the face is walled.
    #[test]
    fn piston_face_cell_is_walled() {
        let at = BlockPos::new(8, 5, 8);
        let dirs = [
            (FacingCubic::North, 0, 0, -1),
            (FacingCubic::South, 0, 0, 1),
            (FacingCubic::East, 1, 0, 0),
            (FacingCubic::West, -1, 0, 0),
            (FacingCubic::Up, 0, 1, 0),
            (FacingCubic::Down, 0, -1, 0),
        ];
        for kind in [BlockKind::Piston, BlockKind::StickyPiston] {
            for extended in [false, true] {
                for &(facing, fx, fy, fz) in &dirs {
                    let w = cached(floor_world(&[(at, piston_facing(kind, facing, extended))]));
                    let front = BlockPos::new(at.x + fx, at.y + fy, at.z + fz);
                    assert!(!w.is_block_pos_passable(front), "{kind:?} {facing:?} front");
                    for &(other, ox, oy, oz) in &dirs {
                        if other == facing {
                            continue;
                        }
                        let n = BlockPos::new(at.x + ox, at.y + oy, at.z + oz);
                        assert!(w.is_block_pos_passable(n), "{kind:?} {facing:?} not {other:?}");
                    }
                }
            }
        }
    }

    // The border margin: inside the border but within 3 blocks is walled; farther in is not; no
    // border state, no wall; a shrinking border is walled where it ends.
    #[test]
    fn border_margin() {
        let state = WorldBorderState {
            center_x: 0.0,
            center_z: 0.0,
            from_size: 100.0,
            to_size: 100.0,
            start_tick: 0,
            lerp_ticks: 0,
        };
        let wall = BorderWall::from_state(&state, 10);
        // the border's east edge is x = 50; a cell [46, 47) is 3 blocks inside
        assert!(!wall.walls(BlockPos::new(46, 70, 0)));
        assert!(wall.walls(BlockPos::new(47, 70, 0)));
        assert!(wall.walls(BlockPos::new(60, 70, 0)));
        assert!(!wall.walls(BlockPos::new(-50 + 3, 70, 0)));
        assert!(wall.walls(BlockPos::new(-50 + 2, 70, 0)));
        assert!(wall.walls(BlockPos::new(0, 70, 47)));
        assert!(!wall.walls(BlockPos::new(0, 70, 0)));

        // a border centred so chunk (0, 0) is near its east edge: size 20 centred at (4, 4),
        // so the edges are -6 and 14
        let edge = WorldBorderState { center_x: 4.0, center_z: 4.0, from_size: 20.0, to_size: 20.0, ..state };
        let with = cached(floor_world(&[])).with_border(Some(BorderWall::from_state(&edge, 0)));
        let without = cached(floor_world(&[]));
        assert!(!with.is_standable_at_block_pos(BlockPos::new(12, 1, 4)));
        assert!(with.is_standable_at_block_pos(BlockPos::new(10, 1, 4)));
        assert!(with.is_wall_cell(crate::pathfinder::positions::RelBlockPos::new(12, 1, 4)));
        assert!(without.is_standable_at_block_pos(BlockPos::new(12, 1, 4)));

        // shrinking from 100 to 20: walled at the final size even at the start
        let shrinking = WorldBorderState { from_size: 100.0, to_size: 20.0, start_tick: 0, lerp_ticks: 1000, ..state };
        assert_eq!(BorderWall::from_state(&shrinking, 0).max_x, 10.0);
        // growing from 20 to 100: walled at the current size
        let growing = WorldBorderState { from_size: 20.0, to_size: 100.0, start_tick: 0, lerp_ticks: 1000, ..state };
        assert_eq!(BorderWall::from_state(&growing, 0).max_x, 10.0);
    }

    // A cell walled by a neighbour or the border is not a block to mine through.
    #[test]
    fn wall_cells_are_not_mined() {
        let menu = || Some(azalea_inventory::Menu::Player(azalea_inventory::Player::default()));
        let w = cached(floor_world(&[(BlockPos::new(5, 1, 5), BlockKind::HoneyBlock.into())]));
        let cache = MiningCache::new(menu());
        assert_eq!(
            w.cost_for_breaking_block(crate::pathfinder::positions::RelBlockPos::new(4, 1, 5), &cache),
            f32::INFINITY
        );
        // an ordinary air cell costs nothing
        assert_eq!(
            w.cost_for_breaking_block(crate::pathfinder::positions::RelBlockPos::new(2, 1, 5), &cache),
            0.
        );
        // a bubble column or scaffolding cell is never broken
        let w = cached(floor_world(&[(BlockPos::new(5, 1, 5), BlockKind::Scaffolding.into())]));
        assert_eq!(
            w.cost_for_breaking_block(crate::pathfinder::positions::RelBlockPos::new(5, 1, 5), &cache),
            f32::INFINITY
        );
    }

    /// Plan from (2, 1, 8) to (13, 1, 8) on the floor world with `blocks`; the nodes of the path.
    fn plan(blocks: &[(BlockPos, BlockState)]) -> (Vec<BlockPos>, bool, TestWorld) {
        let origin = BlockPos::new(2, 1, 8);
        let goal = BlockPosGoal(BlockPos::new(13, 1, 8));
        let world = cached_at(floor_world(blocks), origin);
        let mining_cache = MiningCache::new(None);
        let successors = |pos: crate::pathfinder::positions::RelBlockPos| {
            call_successors_fn(
                &world,
                &mining_cache,
                &CustomPathfinderStateRef::default(),
                moves::default_move,
                pos,
            )
        };
        let astar::Path { movements, is_partial, .. } = a_star(
            crate::pathfinder::positions::RelBlockPos::get_origin(origin),
            |n| goal.heuristic(n.apply(origin)),
            successors,
            |n| goal.success(n.apply(origin)),
            PathfinderTimeout::Nodes(200_000),
            PathfinderTimeout::Nodes(200_000),
        );
        let nodes = movements.iter().map(|m| m.target.apply(origin)).collect();
        (nodes, is_partial, world)
    }

    /// No node of the path is in, on, beside or under a block the planner walls.
    fn assert_clear_of_walls(nodes: &[BlockPos], world: &TestWorld) {
        for &n in nodes {
            for p in [n, n.up(1), n.down(1)] {
                assert!(
                    !is_unmodelled_block(BlockKind::from(world.get_block_state_at_pos(p))),
                    "node {n:?} touches {:?} at {p:?}; path {nodes:?}",
                    world.get_block_state_at_pos(p)
                );
            }
            assert!(!world.is_wall_cell_at_pos(n), "node {n:?} is in a wall cell");
            assert!(!world.is_wall_cell_at_pos(n.up(1)), "node {n:?} head is in a wall cell");
        }
        // a diagonal step passes over the corners of both side cells: neither floor may be walled
        for w in nodes.windows(2) {
            let (a, b) = (w[0], w[1]);
            if (b.x - a.x).abs() == 1 && (b.z - a.z).abs() == 1 {
                for side in [BlockPos::new(b.x, a.y, a.z), BlockPos::new(a.x, a.y, b.z)] {
                    assert!(
                        !is_unmodelled_block(BlockKind::from(world.get_block_state_at_pos(side.down(1)))),
                        "diagonal {a:?} -> {b:?} cuts the corner of {side:?}"
                    );
                }
            }
        }
    }

    // One small plan per kind: a wall of the block across the straight line with gaps at both
    // ends. The path goes round and never touches the block.
    #[test]
    fn plans_route_round_each_wall() {
        let line = |f: &dyn Fn(i32) -> Vec<(BlockPos, BlockState)>| -> Vec<(BlockPos, BlockState)> {
            (4..=12).flat_map(f).collect()
        };
        let kinds_at_feet = [BlockKind::BubbleColumn, BlockKind::Scaffolding, BlockKind::RedBed];
        for kind in kinds_at_feet {
            let blocks = line(&|z| vec![(BlockPos::new(7, 1, z), kind.into())]);
            let (nodes, partial, world) = plan(&blocks);
            assert!(!partial && !nodes.is_empty(), "{kind:?}");
            assert_clear_of_walls(&nodes, &world);
        }
        // slime floor: replace the stone under the line
        let blocks = line(&|z| vec![(BlockPos::new(7, 0, z), BlockKind::SlimeBlock.into())]);
        let (nodes, partial, world) = plan(&blocks);
        assert!(!partial && !nodes.is_empty());
        assert_clear_of_walls(&nodes, &world);
        // a slime strip 1, 2 and 3 wide across the whole floor: no jump over it (take-off and
        // landing sit within a millimetre of its top face), so there is no path across
        for width in 1..=3 {
            let blocks: Vec<_> = (0..16)
                .flat_map(|z| (0..width).map(move |dx| (BlockPos::new(6 + dx, 0, z), BlockKind::SlimeBlock.into())))
                .collect();
            let (nodes, partial, world) = plan(&blocks);
            assert!(partial, "slime strip {width} wide was crossed: {nodes:?}");
            assert_clear_of_walls(&nodes, &world);
            assert!(nodes.iter().all(|n| n.x < 6), "{nodes:?}");
        }
        // honey wall, two high
        let blocks = line(&|z| {
            vec![
                (BlockPos::new(7, 1, z), BlockKind::HoneyBlock.into()),
                (BlockPos::new(7, 2, z), BlockKind::HoneyBlock.into()),
            ]
        });
        let (nodes, partial, world) = plan(&blocks);
        assert!(!partial && !nodes.is_empty());
        assert_clear_of_walls(&nodes, &world);
        for &n in &nodes {
            for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                let side = BlockPos::new(n.x + dx, n.y, n.z + dz);
                assert_ne!(BlockKind::from(world.get_block_state_at_pos(side)), BlockKind::HoneyBlock, "{n:?}");
            }
        }
        // piston faces toward the start
        let blocks = line(&|z| vec![(BlockPos::new(7, 1, z), piston_facing(BlockKind::Piston, FacingCubic::West, false))]);
        let (nodes, partial, world) = plan(&blocks);
        assert!(!partial && !nodes.is_empty());
        assert_clear_of_walls(&nodes, &world);
        for &n in &nodes {
            assert!(!(n.x == 6 && n.y == 1 && (4..=12).contains(&n.z)), "front cell {n:?}");
        }
        // and a wall with no gap and no way round: no path (partial), never through
        let blocks: Vec<_> = (0..16).map(|z| (BlockPos::new(7, 1, z), BlockKind::BubbleColumn.into())).collect();
        let (nodes, partial, world) = plan(&blocks);
        assert!(partial);
        assert_clear_of_walls(&nodes, &world);
        assert!(nodes.iter().all(|n| n.x < 7));
    }
}
