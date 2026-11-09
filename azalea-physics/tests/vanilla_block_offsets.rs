//! Position-based shape offsets against vanilla 1.21.11 (`BlockStateBase#getOffset`, `#getShape`,
//! `#getCollisionShape`). The data file is printed by vanilla itself: one row per offset block
//! state and position, columns `S, state id, block, x,y,z, offset, outline boxes, collision boxes`.
// Spec: specs/F5-version-layer.md §4.6

use azalea_block::BlockState;
use azalea_core::position::BlockPos;
use azalea_physics::collision::{BlockShapeAt, BlockWithShape, block_offset};

const DATA: &str = include_str!("data/vanilla_block_offsets.tsv");
const TOL: f64 = 2e-9;

fn boxes(s: &str) -> Vec<[f64; 6]> {
    s.split(';')
        .filter(|b| !b.is_empty() && *b != "-")
        .map(|b| {
            let v: Vec<f64> = b.split(',').map(|n| n.parse().unwrap()).collect();
            [v[0], v[1], v[2], v[3], v[4], v[5]]
        })
        .collect()
}

fn ours(shape: &azalea_physics::collision::VoxelShape) -> Vec<[f64; 6]> {
    let mut v: Vec<[f64; 6]> = shape
        .to_aabbs()
        .iter()
        .map(|a| [a.min.x, a.min.y, a.min.z, a.max.x, a.max.y, a.max.z])
        .collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v
}

fn same(a: &[[f64; 6]], b: &[[f64; 6]]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.iter().zip(y).all(|(p, q)| (p - q).abs() < TOL))
}

#[test]
fn offset_and_shapes_match_vanilla_at_every_position() {
    let mut rows = 0;
    let mut bad = Vec::new();
    for line in DATA.lines() {
        let c: Vec<&str> = line.split('\t').collect();
        let state = BlockState::try_from(c[1].parse::<u32>().unwrap()).unwrap();
        let p: Vec<i32> = c[3].split(',').map(|n| n.parse().unwrap()).collect();
        let pos = BlockPos::new(p[0], p[1], p[2]);
        let o: Vec<f64> = c[4].split(',').map(|n| n.parse().unwrap()).collect();
        let off = block_offset(state, pos);
        let mut want_outline = boxes(c[5]);
        want_outline.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mut want_coll = boxes(c[6]);
        want_coll.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let ok = (off.x - o[0]).abs() < 1e-11
            && (off.y - o[1]).abs() < 1e-11
            && (off.z - o[2]).abs() < 1e-11
            && same(&ours(&state.outline_shape_at(pos)), &want_outline)
            && same(&ours(&state.collision_shape_at(pos)), &want_coll);
        if !ok {
            bad.push(format!("{} {} {:?} {:?} want {:?}", c[2], c[3], off, ours(&state.outline_shape_at(pos)), want_outline));
        }
        rows += 1;
    }
    assert!(rows > 1000);
    assert!(bad.is_empty(), "{} of {rows} rows differ, first: {:?}", bad.len(), &bad[..bad.len().min(5)]);
}

/// The test data tells a table shape from a moved one: at a non-zero position the table shape
/// alone must differ from vanilla's, or the test above proves nothing.
#[test]
fn table_shape_alone_differs_from_vanilla_away_from_origin() {
    let mut differing = 0;
    for line in DATA.lines() {
        let c: Vec<&str> = line.split('\t').collect();
        if c[3] == "0,0,0" || c[2] != "poppy" {
            continue;
        }
        let state = BlockState::try_from(c[1].parse::<u32>().unwrap()).unwrap();
        if !same(&ours(state.outline_shape()), &boxes(c[5])) {
            differing += 1;
        }
    }
    assert!(differing >= 8);
}
