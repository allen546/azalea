//! The fork's `collide` against vanilla 1.21.11's own `Entity#collide`. The
//! data file is printed by `harness/vanilla-move` in the Null repo: scene
//! blocks, a player-sized entity's start box, `onGround` and the move, and the
//! vector vanilla returned. Row kinds: `P` (a block state's collision boxes),
//! `S` (a scene: cells), `C` (a case).
// Spec: specs/B1-movement.md §4.2

use std::collections::HashMap;

use azalea_block::{BlockState, BlockTrait};
use azalea_core::{
    cursor3d::{Cursor3d, CursorIterationType},
    position::BlockPos,
};
use azalea_entity::dimensions::EntityDimensions;
use azalea_registry::builtin::BlockKind;

use super::*;

const DATA: &str = include_str!("../../tests/data/vanilla_move_cases.tsv");

/// `name[prop=value,...]` (no namespace) to a state; properties not named keep
/// their defaults.
fn parse_state(s: &str) -> Result<BlockState, String> {
    let (name, props) = match s.split_once('[') {
        Some((n, p)) => (n, p.trim_end_matches(']')),
        None => (s, ""),
    };
    let kind: BlockKind = format!("minecraft:{name}")
        .parse()
        .map_err(|_| format!("unknown block {name}"))?;
    let mut block: Box<dyn BlockTrait> = kind.into();
    for kv in props.split(',').filter(|p| !p.is_empty()) {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| format!("bad property {kv}"))?;
        block
            .set_property(k, v)
            .map_err(|_| format!("{name}: bad property {k}={v}"))?;
    }
    Ok(block.as_block_state())
}

fn nums(s: &str) -> Vec<f64> {
    s.split(',').map(|n| n.parse().unwrap()).collect()
}

fn sorted_boxes(s: &str) -> Vec<[f64; 6]> {
    let mut v: Vec<[f64; 6]> = s
        .split(';')
        .filter(|b| *b != "-")
        .map(|b| {
            let n = nums(b);
            [n[0], n[1], n[2], n[3], n[4], n[5]]
        })
        .collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v
}

/// Block colliders of a box, as vanilla's block collision query gathers them:
/// every cell the box touches, each shape moved to its cell, kept when it
/// overlaps the box.
fn block_colliders(cells: &HashMap<(i32, i32, i32), BlockState>, aabb: &Aabb) -> Vec<VoxelShape> {
    |v: f64| (v - EPSILON).floor() as i32;
    let hi = |v: f64| (v + EPSILON).floor() as i32;
    let entity_shape = VoxelShape::from(aabb);
    let mut out = Vec::new();
    // The same cell walk as the fork's query and vanilla's (`BlockCollisions`):
    // the order decides which shape trims a move first, and a trim under
    // 1.0E-7 ends the axis.
    let origin = BlockPos::new(
        (aabb.min.x - EPSILON).floor() as i32 - 1,
        (aabb.min.y - EPSILON).floor() as i32 - 1,
        (aabb.min.z - EPSILON).floor() as i32 - 1,
    );
    let end = BlockPos::new(
        (aabb.max.x + EPSILON).floor() as i32 + 1,
        (aabb.max.y + EPSILON).floor() as i32 + 1,
        (aabb.max.z + EPSILON).floor() as i32 + 1,
    );
    let mut cursor = Cursor3d::new(origin, end);
    while let Some(item) = cursor.next() {
        if item.iteration_type == CursorIterationType::Corner {
            continue;
        }
        let Some(state) = cells.get(&(item.pos.x, item.pos.y, item.pos.z)) else {
            continue;
        };
        if state.is_air() {
            continue;
        }
        let shape = BlockShapeAt::collision_shape_at(state, item.pos);
        let moved = shape.move_relative(item.pos.to_vec3_floored());
        if Shapes::matches_anywhere(&moved, &entity_shape, |a, b| a && b) {
            out.push(moved);
        }
    }
    out
}

fn bits_eq(a: f64, b: f64) -> bool {
    a == b // exact; -0.0 equals 0.0 (a move component of zero has no direction)
}

/// Each row of the file is checked against the fork; a summary and every
/// mismatch print with `--nocapture`. The test fails on any mismatch.
// Spec: specs/B1-movement.md §4.2
#[test]
fn collide_matches_vanilla_on_every_case() {
    let mut palette: HashMap<String, BlockState> = HashMap::new();
    let mut bad_palette: Vec<String> = Vec::new();
    let mut scenes: HashMap<u32, Option<HashMap<(i32, i32, i32), BlockState>>> = HashMap::new();
    let (mut cases, mut mismatches, mut skipped, mut stepped, mut box_bad) =
        (0, Vec::new(), 0, 0, 0);
    let mut pushed_blocked = 0;

    for line in DATA.lines().filter(|l| !l.starts_with('#')) {
        let c: Vec<&str> = line.split('\t').collect();
        match c[0] {
            "P" => match parse_state(c[1]) {
                Ok(state) => {
                    let ours: Vec<[f64; 6]> = {
                        let mut v: Vec<[f64; 6]> = state
                            .collision_shape()
                            .to_aabbs()
                            .iter()
                            .map(|a| [a.min.x, a.min.y, a.min.z, a.max.x, a.max.y, a.max.z])
                            .collect();
                        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                        v
                    };
                    let want = sorted_boxes(c[2]);
                    let same = ours.len() == want.len()
                        && ours
                            .iter()
                            .zip(&want)
                            .all(|(p, q)| p.iter().zip(q).all(|(x, y)| (x - y).abs() < 1e-9));
                    if same {
                        palette.insert(c[1].to_string(), state);
                    } else {
                        bad_palette.push(format!("{} shape: fork {ours:?} vanilla {want:?}", c[1]));
                    }
                }
                Err(e) => bad_palette.push(format!("{}: {e}", c[1])),
            },
            "S" => {
                let mut map = HashMap::new();
                let mut ok = true;
                for cell in c[3].split(';') {
                    let (p, st) = cell.split_once('=').unwrap();
                    let p: Vec<i32> = p.split(',').map(|n| n.parse().unwrap()).collect();
                    match palette
                        .get(st)
                        .copied()
                        .or_else(|| (st == "stone").then(|| parse_state(st).unwrap()))
                    {
                        Some(s) => {
                            map.insert((p[0], p[1], p[2]), s);
                        }
                        None => ok = false,
                    }
                }
                scenes.insert(c[1].parse().unwrap(), ok.then_some(map));
            }
            "C" => {
                let id = c[1];
                let Some(Some(cells)) = scenes.get(&c[2].parse::<u32>().unwrap()) else {
                    skipped += 1;
                    continue;
                };
                let p = nums(c[3]);
                let on_ground = c[4] == "1";
                let m = nums(c[5]);
                let vb = nums(c[6]);
                let want = nums(c[7]);
                let aabb =
                    EntityDimensions::new(0.6, 1.8).make_bounding_box(Vec3::new(p[0], p[1], p[2]));
                let ours_box = [
                    aabb.min.x, aabb.min.y, aabb.min.z, aabb.max.x, aabb.max.y, aabb.max.z,
                ];
                if ours_box.iter().zip(&vb).any(|(a, b)| !bits_eq(*a, *b)) {
                    box_bad += 1;
                    mismatches.push(format!("case {id}: box fork {ours_box:?} vanilla {vb:?}"));
                }
                let movement = Vec3::new(m[0], m[1], m[2]);
                let got = collide_with(&aabb, on_ground, 0.6, movement, &[], &|b| {
                    block_colliders(cells, b)
                });
                cases += 1;
                if want[1] > 0.0 && want[1] > m[1] {
                    stepped += 1;
                }
                if m[1] < 0.0 && want[1] != m[1] && (want[0] != m[0] || want[2] != m[2]) {
                    pushed_blocked += 1;
                }
                if !(bits_eq(got.x, want[0]) && bits_eq(got.y, want[1]) && bits_eq(got.z, want[2]))
                {
                    mismatches.push(format!(
                        "case {id} scene {} pos {} ground {on_ground} move {} : fork {:?} vanilla {:?}",
                        c[2], c[3], c[5], [got.x, got.y, got.z], want
                    ));
                }
            }
            _ => {}
        }
    }

    println!(
        "cases compared {cases}, skipped (unsupported palette) {skipped}, step-ups in vanilla {stepped}, falling moves cut both down and sideways {pushed_blocked}, box differences {box_bad}, mismatches {}",
        mismatches.len()
    );
    for b in &bad_palette {
        println!("palette: {b}");
    }
    for m in mismatches.iter().take(60) {
        println!("MISMATCH {m}");
    }
    assert!(mismatches.is_empty(), "{} mismatches", mismatches.len());
    assert!(cases > 3000, "too few cases compared: {cases}");
}
