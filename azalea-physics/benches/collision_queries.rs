//! Collision queries with and without pending cells (Null fork patches 53 and 76).
// Spec: specs/B6-placement-breaking.md §4.10
//
// "nothing_pending" is the case every physics tick pays for. "pending_elsewhere" and
// "pending_in_box" are the new slow-path costs. The baseline (head 62972ef) runs the same
// queries on `&World`, see `query`.

use std::hint::black_box;

use azalea_core::{
    aabb::Aabb,
    position::{BlockPos, ChunkBlockPos, ChunkPos, Vec3},
};
use azalea_physics::collision::world_collisions::get_block_collisions;
use azalea_registry::builtin::BlockKind;
use azalea_world::{BlockStatePredictionHandler, Chunk, PartialChunkStorage, World};
use criterion::{Criterion, criterion_group, criterion_main};

fn make_world() -> (World, PartialChunkStorage) {
    let mut world = World::default();
    world.chunks = azalea_world::ChunkStorage::new(384, -64);
    let mut partial = PartialChunkStorage::new(8);
    for cx in -2..2 {
        for cz in -2..2 {
            partial.set(&ChunkPos::new(cx, cz), Some(Chunk::default()), &mut world.chunks);
        }
    }
    let min_y = world.chunks.min_y();
    for cx in -2..2 {
        for cz in -2..2 {
            let chunk = world.chunks.get(&ChunkPos::new(cx, cz)).unwrap();
            let mut chunk = chunk.write();
            for x in 0..16_u8 {
                for z in 0..16_u8 {
                    chunk.set_block_state(&ChunkBlockPos::new(x, 1, z), BlockKind::Stone.into(), min_y);
                    // some stairs and fences so the partial-shape path runs too
                    if (x + z) % 5 == 0 {
                        chunk.set_block_state(&ChunkBlockPos::new(x, 2, z), BlockKind::OakFence.into(), min_y);
                    }
                }
            }
        }
    }
    (world, partial)
}

#[inline(never)]
fn query(world: &World, aabb: &Aabb) -> usize {
    get_block_collisions(world, aabb).len()
}

fn boxes() -> Vec<Aabb> {
    let mut v = Vec::new();
    for i in 0..64 {
        let x = -20.0 + i as f64 * 0.55;
        let z = -20.0 + (i * 7 % 64) as f64 * 0.55;
        v.push(Aabb { min: Vec3::new(x, 2.0, z), max: Vec3::new(x + 0.6, 3.8, z + 0.6) });
    }
    v
}

fn bench(c: &mut Criterion) {
    let (world, _keep) = make_world();
    let boxes = boxes();

    c.bench_function("collision_queries/nothing_pending", |b| {
        b.iter(|| {
            let mut n = 0;
            for aabb in &boxes {
                n += query(black_box(&world), aabb);
            }
            black_box(n)
        })
    });

    // eight predicted cells in the middle of the walk: they are in the world, so the query
    // reads them as it reads any block
    let mut inside = BlockStatePredictionHandler::default();
    inside.start_predicting();
    for i in 0..8 {
        let p = BlockPos::new(-10 + i, 2, -10 + i);
        inside.predict_block(&world, p, BlockKind::Stone.into(), Vec3::ZERO);
    }
    c.bench_function("collision_queries/pending_in_box", |b| {
        b.iter(|| {
            let mut n = 0;
            for aabb in &boxes {
                n += query(black_box(&world), aabb);
            }
            black_box(n)
        })
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
