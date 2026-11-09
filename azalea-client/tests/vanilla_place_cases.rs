//! Place prediction against vanilla's own output (Null fork, place oracle).
//!
//! `tests/data/vanilla_place_cases.tsv` is written by `harness/vanilla-place/` in the Null repo: it
//! runs vanilla 1.21.11's `BlockItem#place` steps (`BlockPlaceContext#canPlace`,
//! `Block#getStateForPlacement`, `canSurvive`, `Level#isUnobstructed`) on a real `ServerLevel`, one
//! scene per case, and records the state vanilla would place, or `none` when it places nothing.
//! This test runs `predict_place` on the same scene and compares. A family the fork does not
//! predict (`Verdict::Unknown`) is skipped and counted, never counted as a pass.
//!
//! Columns: id, item, family, clicked, face, hit, yaw, pitch, sneak, setup, result, why. Names
//! lack the `minecraft:` prefix. Positions are offsets from the origin cell, where the placed block
//! goes. `why` is `at=x,y,z` when vanilla's target cell is not the origin.
//!
//! Set `PLACE_ORACLE_REPORT=<path>` to write the mismatch list to a file as well.
//!
//! Spec: specs/B6-placement-breaking.md §4.10 (the vanilla oracle)

use std::collections::BTreeMap;
use std::fmt::Write as _;

use azalea_block::{BlockState, BlockTrait};
use azalea_client::place_prediction::{PlaceRequest, UnknownWhy, Verdict, predict_place};
use azalea_core::{
    direction::Direction,
    game_type::GameMode,
    position::{BlockPos, ChunkPos, Vec3},
};
use azalea_registry::builtin::BlockKind;
use azalea_world::{Chunk, ChunkStorage, PartialChunkStorage};

/// The case origin; the harness uses the same one.
const ORIGIN: (i32, i32, i32) = (8, 100, 8);

/// `name[prop=value,...]` (no namespace) to a state; properties not named keep their defaults, as
/// vanilla's block state parser does.
fn parse_state(s: &str) -> Result<BlockState, String> {
    let (name, props) = match s.split_once('[') {
        Some((n, p)) => (n, p.trim_end_matches(']')),
        None => (s, ""),
    };
    let kind: BlockKind = format!("minecraft:{name}").parse().map_err(|_| format!("unknown block {name}"))?;
    let mut block: Box<dyn BlockTrait> = kind.into();
    for kv in props.split(',').filter(|p| !p.is_empty()) {
        let (k, v) = kv.split_once('=').ok_or_else(|| format!("bad property {kv}"))?;
        block.set_property(k, v).map_err(|_| format!("{name}: bad property {k}={v}"))?;
    }
    Ok(block.as_block_state())
}

fn show(state: BlockState) -> String {
    let block: &dyn BlockTrait = state.into();
    let mut props: Vec<String> = block.property_map().iter().map(|(k, v)| format!("{k}={v}")).collect();
    props.sort();
    let name = block.id();
    if props.is_empty() { name.to_string() } else { format!("{name}[{}]", props.join(",")) }
}

fn direction(name: &str) -> Direction {
    match name {
        "down" => Direction::Down,
        "up" => Direction::Up,
        "north" => Direction::North,
        "south" => Direction::South,
        "west" => Direction::West,
        "east" => Direction::East,
        other => panic!("bad face {other}"),
    }
}

fn triple<T: std::str::FromStr>(s: &str) -> (T, T, T)
where
    T::Err: std::fmt::Debug,
{
    let mut it = s.split(',').map(|n| n.parse::<T>().unwrap());
    (it.next().unwrap(), it.next().unwrap(), it.next().unwrap())
}

fn cell(offset: (i32, i32, i32)) -> BlockPos {
    BlockPos::new(ORIGIN.0 + offset.0, ORIGIN.1 + offset.1, ORIGIN.2 + offset.2)
}

// Spec: specs/B6-placement-breaking.md §4.10 (the vanilla oracle)
#[test]
fn place_prediction_matches_vanilla_output() {
    let text = include_str!("data/vanilla_place_cases.tsv");
    let air = BlockState::from(BlockKind::Air);

    // one loaded chunk; keep `_partial` alive, the storage holds chunks weakly
    let mut chunks = ChunkStorage::new(384, -64);
    let mut partial = PartialChunkStorage::new(2);
    partial.set(&ChunkPos::new(0, 0), Some(Chunk::default()), &mut chunks);

    let mut total = 0usize;
    let mut passed = 0usize;
    let mut passed_none = 0usize;
    let mut skipped: BTreeMap<String, BTreeMap<&'static str, usize>> = BTreeMap::new();
    let mut by_family: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new(); // cases, compared, mismatches
    let mut mismatches: Vec<String> = Vec::new();
    let mut dirty: Vec<BlockPos> = Vec::new();

    for line in text.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
        let f: Vec<&str> = line.split('\t').collect();
        assert_eq!(f.len(), 12, "bad line: {line}");
        let (id, item, family) = (f[0], f[1], f[2]);
        total += 1;
        by_family.entry(family.to_string()).or_default().0 += 1;

        for p in dirty.drain(..) {
            chunks.set_block_state(p, air);
        }
        if f[9] != "-" {
            for entry in f[9].split(';') {
                let (pos, state) = entry.split_once('=').unwrap();
                let p = cell(triple(pos));
                let st = parse_state(state).unwrap_or_else(|e| panic!("{id}: {e}"));
                chunks.set_block_state(p, st);
                dirty.push(p);
            }
        }

        let (cx, cy, cz) = triple::<i32>(f[3]);
        let (hx, hy, hz) = triple::<f64>(f[5]);
        let (yaw, pitch): (f32, f32) = (f[6].parse().unwrap(), f[7].parse().unwrap());
        let held = format!("minecraft:{item}");
        let req = PlaceRequest {
            clicked: cell((cx, cy, cz)),
            face: direction(f[4]),
            hit: Vec3::new(ORIGIN.0 as f64 + hx, ORIGIN.1 as f64 + hy, ORIGIN.2 as f64 + hz),
            held: &held,
            has_block_state_component: false,
            game_mode: GameMode::Survival,
            yaw,
            sent_yaw: yaw,
            pitch,
            sent_pitch: pitch,
            secondary_use: Some(f[8] == "1"),
            obstacles: &[],
        };
        let verdict = predict_place(&chunks, &req);

        let vanilla = f[10];
        let target = f[11].strip_prefix("at=").map(|s| cell(triple(s))).unwrap_or_else(|| cell((0, 0, 0)));
        let entry = by_family.get_mut(family).unwrap();
        let mismatch = match verdict {
            Verdict::Unknown(why) => {
                *skipped.entry(family.to_string()).or_default().entry(unknown_name(why)).or_default() += 1;
                continue;
            }
            Verdict::Nothing(reason) => {
                entry.1 += 1;
                if vanilla == "none" {
                    passed += 1;
                    passed_none += 1;
                    None
                } else {
                    Some(format!("fork places nothing ({reason}), vanilla places {vanilla}"))
                }
            }
            Verdict::Predict { pos, state } => {
                entry.1 += 1;
                if vanilla == "none" {
                    Some(format!("fork places {} at {pos:?}, vanilla places nothing ({})", show(state), f[11]))
                } else {
                    let expected = parse_state(vanilla).unwrap_or_else(|e| panic!("{id}: {e}"));
                    if pos == target && state == expected {
                        passed += 1;
                        None
                    } else {
                        Some(format!("fork {} at {pos:?}, vanilla {vanilla} at {target:?}", show(state)))
                    }
                }
            }
        };
        if let Some(m) = mismatch {
            entry.2 += 1;
            mismatches.push(format!("{family}\t{id}\t{item}\t{}\t{m}\tclick {} {} hit {} yaw {yaw} pitch {pitch} sneak {} setup {}", f[4], f[3], f[4], f[5], f[8], f[9]));
        }
    }

    let skipped_total: usize = skipped.values().flat_map(|m| m.values()).sum();
    let mut report = String::new();
    writeln!(report, "cases {total}: pass {passed} (of which vanilla places nothing {passed_none}), mismatch {}, skipped Unknown {skipped_total}", mismatches.len()).unwrap();
    writeln!(report, "family\tcases\tcompared\tmismatches\tskipped (reason: count)").unwrap();
    for (fam, (n, compared, bad)) in &by_family {
        let sk = skipped
            .get(fam)
            .map(|m| m.iter().map(|(w, c)| format!("{w}: {c}")).collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        writeln!(report, "{fam}\t{n}\t{compared}\t{bad}\t{sk}").unwrap();
    }
    writeln!(report, "mismatches (family, id, item, face, detail, input):").unwrap();
    for m in &mismatches {
        writeln!(report, "{m}").unwrap();
    }
    if let Ok(path) = std::env::var("PLACE_ORACLE_REPORT") {
        std::fs::write(path, &report).unwrap();
    }
    println!("{report}");
    assert!(mismatches.is_empty(), "{} of {} compared cases differ from vanilla (list above)", mismatches.len(), total - skipped_total);
    // The skip count may only go down: a change that turns predicted cases into `Unknown` must fail here,
    // not pass with fewer cases compared. Lower the cap when a patch predicts more.
    assert!(skipped_total <= MAX_SKIPPED, "{skipped_total} cases skipped as Unknown, more than the {MAX_SKIPPED} allowed");
}

/// Cases the fork answers `Unknown` on at 2026-11-15 (Support 220, Signal 129, FacingYaw 288).
const MAX_SKIPPED: usize = 667;

fn unknown_name(why: UnknownWhy) -> &'static str {
    match why {
        UnknownWhy::Adventure => "Adventure",
        UnknownWhy::Family => "Family",
        UnknownWhy::BlockStateComponent => "BlockStateComponent",
        UnknownWhy::Unloaded => "Unloaded",
        UnknownWhy::ReplaceRule => "ReplaceRule",
        UnknownWhy::EntityUnsure => "EntityUnsure",
        UnknownWhy::FacingYaw => "FacingYaw",
        UnknownWhy::Support => "Support",
        UnknownWhy::Signal => "Signal",
        UnknownWhy::SecondaryUse => "SecondaryUse",
    }
}
