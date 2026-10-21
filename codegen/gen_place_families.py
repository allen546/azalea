#!/usr/bin/env python3
"""Null fork: generate azalea-block/src/place_families.rs, how a block item places (patch 33).

For each block, from the vanilla class chain (Mojang names):

- `place_family`: how `BlockItem#place` computes the placed state, for the stage 1 families
  only: Plain (`Block#getStateForPlacement` returns the default state), Axis
  (`RotatedPillarBlock`), Waterloggable (default plus water from the cell), Slab (`SlabBlock`),
  HorizontalFacing (`getHorizontalDirection()`, as is or opposite). A block is in a family only
  when its item is a plain `BlockItem` named like the block, its first `getStateForPlacement`
  override in the class chain has exactly the family's body, and no class in the chain overrides
  `canSurvive` (so the survive check is trivially true). Everything else is Unknown.
- `shape_inert`: blocks whose `updateShape` (the neighbour shape update that `Level#setBlock` with
  flags 11 runs) never changes the block: no override in the chain, or only the waterlogged
  schedule-tick one. A block not listed may react to a neighbour.
- `plain_replace_rule`: blocks whose class chain does not override `canBeReplaced`.
- `indirect_shape_blocks`: blocks whose class overrides `updateIndirectNeighbourShapes`.

Usage (Python through uv):
    uv run --no-project codegen/gen_place_families.py <vanilla-src-dir> <version> > azalea-block/src/place_families.rs
where <vanilla-src-dir> holds the decompiled, Mojang-named sources (it has net/minecraft/...).
"""
import re
import sys
from pathlib import Path

src, version = Path(sys.argv[1]), sys.argv[2]
block_dir = src / "net/minecraft/world/level/block"
item_dir = src / "net/minecraft/world/item"


def norm(s):
    return re.sub(r"\s+", " ", s).strip()


def method_body(text, pattern):
    """The normalised body of the first method matching `pattern` (up to the closing tab brace)."""
    m = re.search(pattern + r"\s*\{(.*?)\n\t\}\n", text, re.S)
    return norm(m.group(m.lastindex)) if m else None


# class name -> info
classes = {}
for f in block_dir.rglob("*.java"):
    text = f.read_text()
    name = f.stem
    m = re.search(r"\b(?:class|interface)\s+(\w+)(?:<[^{]*?>)?\s+extends\s+(\w+)", text, re.S)
    sup = m.group(2) if m and m.group(1) == name else None
    gsp = method_body(text, r"getStateForPlacement\(BlockPlaceContext (\w+)\)")
    if gsp is not None:
        ctx = re.search(r"getStateForPlacement\(BlockPlaceContext (\w+)\)", text).group(1)
        gsp = re.sub(r"\b%s\b" % ctx, "CTX", gsp)
    us = method_body(text, r"BlockState updateShape\([^)]*\)")
    if us is not None:
        us = re.sub(r"\b(blockState\d?|levelReader|scheduledTickAccess|blockPos\d?|direction\d?|randomSource)\b", "X", us)
    classes[name] = dict(
        sup=sup,
        gsp=gsp,
        us=us,
        survive=bool(re.search(r"boolean canSurvive\(", text)),
        replace=bool(re.search(r"boolean canBeReplaced\(BlockState \w+, BlockPlaceContext", text))
        or "implements SegmentableBlock" in text
        or re.search(r"\bSegmentableBlock\b", text) is not None and name != "SegmentableBlock",
        indirect=bool(re.search(r"updateIndirectNeighbourShapes\(", text)),
    )



def trivial_update_shape(body):
    """The override only schedules ticks and calls super: every `return` is the super call and it
    builds no new state, so the block's own state never changes from a neighbour update."""
    rets = re.findall(r"\breturn\b(?! super\.updateShape\()", body)
    return not rets and not re.search(r"setValue|Blocks\.|defaultBlockState|\.is\(", body)

FACING = "return this.defaultBlockState().setValue(FACING, CTX.getHorizontalDirection().getOpposite());"
FACING_SAME = "return this.defaultBlockState().setValue(FACING, CTX.getHorizontalDirection());"
AXIS = "return this.defaultBlockState().setValue(AXIS, CTX.getClickedFace().getAxis());"
WATER_BODIES = {
    "return this.defaultBlockState() .setValue(WATERLOGGED, CTX.getLevel().getFluidState(CTX.getClickedPos()).getType() == Fluids.WATER);",
    "FluidState fluidState = CTX.getLevel().getFluidState(CTX.getClickedPos()); return this.defaultBlockState().setValue(WATERLOGGED, fluidState.getType() == Fluids.WATER);",
    "return this.defaultBlockState().setValue(WATERLOGGED, CTX.getLevel().getFluidState(CTX.getClickedPos()).getType() == Fluids.WATER);",
}


def chain(cls):
    seen = []
    while cls in classes and cls not in seen and cls not in ("Block", "BlockBehaviour"):
        seen.append(cls)
        cls = classes[cls]["sup"]
    return seen


def family(cls):
    ch = chain(cls)
    if any(classes[c]["survive"] for c in ch):
        return "Unknown"
    first = classes_first_gsp(ch)
    if first == "SlabBlock":
        return "Slab"
    if first is None:
        return "Plain"
    body = classes[first]["gsp"]
    if body == AXIS:
        return "Axis"
    if body in WATER_BODIES:
        return "Waterloggable"
    if body == FACING:
        return "HorizontalFacing"
    if body == FACING_SAME:
        return "HorizontalFacingSame"
    return "Unknown"


def classes_first_gsp(ch):
    for c in ch:
        if classes[c]["gsp"] is not None:
            return c
    return None


def inert(cls):
    for c in chain(cls):
        us = classes[c]["us"]
        if us is not None and not trivial_update_shape(us):
            return False
    return True


# Registration helpers in Blocks.java that build a fixed class.
helper_class = {
    "registerBed": "BedBlock",
    "registerStainedGlass": "StainedGlassBlock",
    "registerStair": "StairBlock",
    "registerLegacyStair": "StairBlock",
}
blocks_text = (block_dir / "Blocks.java").read_text()
stmt = re.compile(r"public static final Block (\w+) = (\w+)\((.*?)\);\n", re.S)
registered = []  # (id, class)
for m in stmt.finditer(blocks_text):
    call, args = m.group(2), m.group(3)
    ident = re.match(r'\s*(?:"(\w+)"|net\.minecraft\.references\.Blocks\.(\w+))', args)
    if not ident:
        continue
    block_id = ident.group(1) or ident.group(2).lower()
    if call in helper_class:
        cls = helper_class[call]
    elif call == "register":
        first = r'\s*(?:"\w+"|net\.minecraft\.references\.Blocks\.\w+)\s*,\s*'
        ref = re.match(first + r"(\w+)::new", args)
        lam = re.match(first + r"\w+\s*->\s*new\s+(\w+)", args, re.S)
        cls = ref.group(1) if ref else lam.group(1) if lam else "Block"
    else:
        continue
    registered.append((block_id, cls))

# Items that are a plain BlockItem named like the block.
items_text = (item_dir / "Items.java").read_text()
plain_items = set(
    re.findall(r"= registerBlock\(Blocks\.(\w+)\);", items_text)
    + re.findall(r"= registerBlock\(Blocks\.(\w+), new Item\.Properties\(\)[^;]*\);", items_text)
)
plain_items = {i.lower() for i in plain_items}

camel = lambda s: "".join(p.capitalize() for p in s.split("_"))
by_family = {}
inert_ids, plain_replace, indirect = [], [], []
for bid, cls in registered:
    if cls not in classes and cls != "Block":
        fam = "Unknown"
    else:
        fam = family(cls) if cls != "Block" else "Plain"
    if bid not in plain_items:
        fam = "Unknown"
    by_family.setdefault(fam, []).append(bid)
    ch = chain(cls) if cls in classes else None
    if cls == "Block" or (ch is not None and inert(cls)):
        inert_ids.append(bid)
    if cls == "Block" or (ch is not None and not any(classes[c]["replace"] for c in ch)):
        plain_replace.append(bid)
    if ch is not None and any(classes[c]["indirect"] for c in ch):
        indirect.append(bid)


def arms(ids):
    return " |\n        ".join(f"BlockKind::{camel(i)}" for i in sorted(ids))


out = [
    f"// Generated by codegen/gen_place_families.py from the Minecraft {version} sources. Do not edit.",
    f"// Regenerate: uv run --no-project codegen/gen_place_families.py <vanilla-src-dir> {version} > azalea-block/src/place_families.rs",
    "// How a block item computes the state it places (BlockItem#place -> Block#getStateForPlacement),",
    "// for the stage 1 families of null patch 33. Counts of "
    f"{len(registered)} parsed blocks: "
    + ", ".join(f"{k} {len(v)}" for k, v in sorted(by_family.items()))
    + f"; shape inert {len(inert_ids)}; plain replace rule {len(plain_replace)}.",
    "",
    "use azalea_registry::builtin::BlockKind;",
    "",
    "/// How placing the block's item computes the placed state. Anything the generator cannot prove is",
    "/// [`PlaceFamily::Unknown`] and is never predicted.",
    "#[derive(Clone, Copy, Debug, Eq, PartialEq)]",
    "pub enum PlaceFamily {",
    "    /// The default state (`Block#getStateForPlacement`).",
    "    Plain,",
    "    /// `RotatedPillarBlock`: the axis of the clicked face.",
    "    Axis,",
    "    /// The default state, waterlogged when the cell holds water.",
    "    Waterloggable,",
    "    /// `SlabBlock`: bottom, top or double.",
    "    Slab,",
    "    /// Facing the opposite of the player's horizontal direction.",
    "    HorizontalFacing,",
    "    /// Facing the player's horizontal direction.",
    "    HorizontalFacingSame,",
    "    Unknown,",
    "}",
    "",
]
out.append("/// The family of this block's item when it is a plain `BlockItem` of the same name.")
out.append("pub fn place_family(kind: BlockKind) -> PlaceFamily {")
out.append("    match kind {")
for fam in ["Plain", "Axis", "Waterloggable", "Slab", "HorizontalFacing", "HorizontalFacingSame"]:
    ids = by_family.get(fam, [])
    if ids:
        out.append(f"        {arms(ids)} => PlaceFamily::{fam},")
out.append("        _ => PlaceFamily::Unknown,")
out.append("    }")
out.append("}")
out.append("")
out.append("/// A neighbour shape update (`updateShape`, run by `Level#setBlock` flags 11) never changes this")
out.append("/// block. A block not listed may react to a placed neighbour (fences, panes, stairs, vines, ...).")
out.append("pub fn shape_inert(kind: BlockKind) -> bool {")
out.append(f"    matches!(\n        kind,\n        {arms(inert_ids)}\n    )")
out.append("}")
out.append("")
out.append("/// The class chain does not override `canBeReplaced`, so a block of this kind is replaced when")
out.append("/// it is replaceable and the held item is another block.")
out.append("pub fn plain_replace_rule(kind: BlockKind) -> bool {")
out.append(f"    matches!(\n        kind,\n        {arms(plain_replace)}\n    )")
out.append("}")
out.append("")
out.append("/// The class overrides `updateIndirectNeighbourShapes` (redstone wire), so a place changes")
out.append("/// shapes two cells away.")
out.append("pub fn indirect_shape_block(kind: BlockKind) -> bool {")
out.append(f"    matches!(\n        kind,\n        {arms(indirect)}\n    )" if indirect else "    false")
out.append("}")
print("\n".join(out))
