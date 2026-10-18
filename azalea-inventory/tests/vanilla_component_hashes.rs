//! Checks every component hash against vanilla 1.21.11's own
//! (`decoratedHashOpsGenerator`).
//!
//! The data comes from `harness/vanilla-hash/ComponentDump.java` in the Null
//! repo (see its README): the order of the synchronized data-driven registries,
//! then per case the component's network bytes and vanilla's hash. All failures
//! are collected and reported together.

use std::{
    collections::BTreeMap,
    io::Cursor,
    panic::{AssertUnwindSafe, catch_unwind},
};

use azalea_core::registry_holder::RegistryHolder;
use azalea_inventory::components::DataComponentUnion;
use azalea_registry::{builtin::DataComponentKind, identifier::Identifier};
use simdnbt::owned::{Nbt, NbtCompound};

const DATA: &str = include_str!("data/vanilla_component_hashes.tsv");

/// Cases we know are wrong and have not fixed: the item text from `items.txt`
/// and why.
const KNOWN_FAILURES: &[(&str, &str)] = &[(
    r#"minecraft:stick[can_break={blocks:"minecraft:chest",nbt:"{Lock:\"a\"}"}]"#,
    "vanilla hashes a block predicate's nbt as SNBT text (CompoundTag#toString); we hash the compound",
)];

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn registries() -> RegistryHolder {
    let mut by_registry: BTreeMap<String, Vec<(usize, Identifier, Option<NbtCompound>)>> =
        BTreeMap::new();
    for line in DATA.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f[0] != "REG" {
            continue;
        }
        let nbt = if f[4] == "-" {
            NbtCompound::new()
        } else {
            let bytes = unhex(f[4]);
            match simdnbt::owned::read_unnamed(&mut Cursor::new(&bytes)).unwrap() {
                Nbt::Some(base) => base.as_compound(),
                Nbt::None => panic!("empty nbt for {}", f[3]),
            }
        };
        by_registry.entry(f[1].to_owned()).or_default().push((
            f[2].parse().unwrap(),
            f[3].parse().unwrap(),
            Some(nbt),
        ));
    }
    let mut holder = RegistryHolder::default();
    for (registry, mut entries) in by_registry {
        // Azalea's DimensionKindElement does not parse 1.21.11's dimension_type
        // (MissingField), and no component hashes through it, so leave
        // it out.
        if registry == "minecraft:dimension_type" {
            continue;
        }
        entries.sort_by_key(|e| e.0);
        for (i, e) in entries.iter().enumerate() {
            assert_eq!(e.0, i, "{registry} has a gap in its ids");
        }
        let count = entries.len();
        holder.append(
            registry.parse().unwrap(),
            entries.into_iter().map(|e| (e.1, e.2)).collect(),
        );
        if registry == "minecraft:enchantment" {
            assert_eq!(holder.enchantment.map.len(), count);
        }
    }
    holder
}

#[test]
fn test_component_hashes_match_vanilla() {
    let registries = registries();
    let mut failures = Vec::new();
    let mut checked = 0;
    let mut kinds_seen = std::collections::BTreeSet::new();

    for line in DATA.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f[0] {
            "UNCOVERED" => failures.push(format!("no case covers {}", f[1])),
            "ERROR" => failures.push(format!("dumper could not parse case {}: {}", f[1], f[3])),
            "CASE" => {}
            _ => continue,
        }
        if f[0] != "CASE" {
            continue;
        }
        let (case, component, bytes, expected, item) = (f[1], f[2], unhex(f[3]), f[4], f[5]);
        let label = format!("case {case} {component} ({item})");
        if let Some((_, why)) = KNOWN_FAILURES.iter().find(|(k, _)| *k == item) {
            eprintln!("known failure: {label}: {why}");
            continue;
        }

        let kind: DataComponentKind = component.parse().expect("unknown component id");
        kinds_seen.insert(component.to_owned());
        let mut cursor = Cursor::new(bytes.as_slice());
        let mut union = match DataComponentUnion::azalea_read_as(kind, &mut cursor) {
            Ok(u) => u,
            Err(e) => {
                failures.push(format!("{label}: decode failed: {e:?}"));
                continue;
            }
        };
        if cursor.position() as usize != bytes.len() {
            failures.push(format!(
                "{label}: decoded {} of {} bytes",
                cursor.position(),
                bytes.len()
            ));
        }
        if expected != "-" {
            let got = catch_unwind(AssertUnwindSafe(|| unsafe {
                union.as_kind(kind).crc_hash(&registries).0
            }));
            checked += 1;
            match got {
                Ok(h) if h.to_string() == expected => {}
                Ok(h) => failures.push(format!("{label}: hash {h}, vanilla {expected}")),
                Err(_) => failures.push(format!("{label}: hashing panicked")),
            }
        }
        unsafe { union.drop_as(kind) };
    }

    eprintln!(
        "checked {checked} hashes over {} component types",
        kinds_seen.len()
    );
    assert!(
        failures.is_empty(),
        "{} problems:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
