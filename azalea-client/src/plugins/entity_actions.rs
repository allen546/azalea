//! Right-click an entity and learn whether a screen opened (storage on entities: chest minecart,
//! hopper minecart, chest boat). The entity twin of [`UseBlockQueued`].
//!
//! Before anything is sent, the click is checked as a vanilla client would have to make it: the
//! crosshair ray (`LocalPlayer#raycastHitResult`: blocks occlude, the entity interaction range
//! limits it) hits that entity, and the entity's hitbox is within reach (`Minecraft#startUseItem`,
//! `Player#isWithinEntityInteractionRange`). If not, the click is refused on the client with an
//! [`EntityUseRefusal`] and nothing goes out. Aim with [`LookAtEntityQueued`] first, as for blocks.
//!
//! What the client sends is what `MultiPlayerGameMode#interactAt` and `#interact` send, in that
//! order, in one tick before the movement packets: the carried slot if it changed, INTERACT_AT with
//! the hit point relative to the entity's position, INTERACT, then the arm swing the predicted
//! `InteractionResult.SUCCESS` asks for. Both packets carry the crouch state.
//!
//! The fork predicts the client's `InteractionResult` for a few entity kinds only
//! ([`entity_use_plan`]); for every other kind it refuses. Riding is not supported, so a standing
//! click that would mount (a chest boat's, `AbstractBoat#interact`) is refused, or, with
//! `auto_crouch`, the bot crouches first (the crouch reaches the server before the click, as for
//! `PlaceBlockQueued`) and stands up again after the click.
//!
//! The server's `ServerGamePacketListenerImpl#handleInteract` accepts the click when the eye is
//! within the interaction range plus 3.0 of the hitbox (Paper: `clientInteractionLeniencyDistance`),
//! so the client's own limit (range, no extra) is stricter. GrimAC checks the packets' order
//! (`PacketOrderC`: INTERACT_AT then INTERACT in one tick, same entity, hand and crouch), the
//! crouch input order (`PacketOrderF`), one target per tick (`MultiInteractA`/`B`) and the
//! actions' state (`MultiActionsC`/`D`). Its `Reach` and `Hitboxes` checks skip boats and
//! non-living entities such as minecarts.
//!
//! [`UseBlockQueued`]: crate::block_actions::UseBlockQueued

use azalea_core::{
    aabb::Aabb,
    entity_id::MinecraftEntityId,
    game_type::GameMode,
    hit_result::{EntityHitResult, HitResult},
    position::Vec3,
    tick::GameTick,
};
use azalea_entity::{
    Attributes, Crouching, LookDirection, Physics, Position, dimensions::EntityDimensions,
    indexing::EntityIdIndex, inventory::Inventory, EntityKindComponent, view_vector,
};
use azalea_physics::{
    PhysicsSystems, client_movement::ClientMovementState, collision::entity_collisions::{
        AabbQuery, LastBoundingBox,
    },
};
use azalea_protocol::packets::game::{
    ServerboundInteract,
    s_interact::{self, InteractionHand},
};
use azalea_registry::builtin::{EntityKind, MenuKind};
use azalea_world::World;
use bevy_app::{App, Plugin};
use bevy_ecs::prelude::*;

use crate::{
    block_actions::{
        AutoCrouchHold, CROUCH_EYE_HEIGHT, ClickEvidence, EvidenceEntities, handle_look_at_block_queued, handle_use_block_queued,
        is_crouching, settle_use_blocks,
    },
    interact::{
        SwingArmEvent,
        pick::{MaybePickableEntityQuery, PickOpts, pick},
    },
    inventory::{OpenScreen, ensure_has_sent_carried_item},
    local_player::{LocalGameMode, WorldHolder},
    mining::{Mining, MiningSystems},
    movement::ItemInUse,
    packet::game::SendGamePacketEvent,
    respawn::{DeadAction, DeathScreen, RespawnLog},
    tick_counter::TicksConnected,
};

/// Ticks after the click with no screen before the result is `NoScreen`. An entity click has no
/// acknowledgement; the server opens the screen in the tick that handles the packet, so this is
/// ample.
const NO_SCREEN_TICKS: u32 = 10;

// Spec: specs/B3-inventory-crafting.md §4.7 (open a container on an entity by right-clicking it)
pub struct EntityActionsPlugin;
impl Plugin for EntityActionsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            GameTick,
            refuse_entity_actions_while_dead
                .after(crate::respawn::tick_death_screen)
                .after(crate::block_actions::refuse_block_actions_while_dead)
                .before(crate::respawn::refuse_input_while_dead)
                .before(handle_look_at_entity_queued),
        )
        .add_systems(
            GameTick,
            handle_look_at_entity_queued
                .after(handle_look_at_block_queued)
                .before(crate::block_actions::handle_place_block_queued)
                .before(handle_use_entity_queued)
                .before(crate::interact::handle_start_use_item_queued)
                .before(crate::item_use::handle_use_item_queued)
                .before(PhysicsSystems),
        )
        .add_systems(
            GameTick,
            (handle_use_entity_queued, settle_use_entities)
                .chain()
                .after(MiningSystems)
                .after(ensure_has_sent_carried_item)
                .after(handle_use_block_queued)
                .after(settle_use_blocks)
                .before(crate::interact::handle_start_use_item_queued)
                .before(PhysicsSystems),
        );
    }
}

/// Why an entity click was refused on the client; nothing was sent.
// Spec: specs/B3-inventory-crafting.md §4.7 (entity clicks: refused clicks)
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntityUseRefusal {
    /// No entity with that id is loaded.
    UnknownEntity,
    /// The fork does not predict the client's reaction to a right-click on this kind (or the
    /// click would do nothing).
    UnsupportedEntity,
    /// The entity's hitbox is out of the interaction range (`Player#isWithinEntityInteractionRange`).
    OutOfReach,
    /// The crosshair does not hit that entity: look at it first.
    NotLookingAtIt,
    /// A block is in the way of the crosshair ray, nearer than the entity.
    BehindBlock,
    /// A standing click would make the player ride it (`AbstractBoat#interact`). Null does not
    /// ride: crouch first, or set `auto_crouch`.
    WouldMount,
    /// A screen is open; a vanilla client has no crosshair then.
    ScreenOpen,
    /// Mining a block or using an item.
    Busy,
    /// Spectators do not use entities (`Player#interactOn` only opens menus for them).
    Spectator,
    /// The death screen is open: a vanilla client has no mouse then (patch 22).
    Dead,
}

impl EntityUseRefusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnknownEntity => "no such entity is loaded",
            Self::UnsupportedEntity => "the client's reaction to a click on this entity is not ported",
            Self::OutOfReach => "out of reach",
            Self::NotLookingAtIt => "crosshair does not hit it",
            Self::BehindBlock => "a block is in the way of the crosshair",
            Self::WouldMount => "a standing click would ride it: crouch first (or auto_crouch)",
            Self::ScreenOpen => "a screen is open",
            Self::Busy => "mining or using an item",
            Self::Spectator => "spectators cannot use entities",
            Self::Dead => "the death screen is open: a vanilla client has no mouse then",
        }
    }
}

/// What a right-click on an entity does on the client, as far as the fork predicts it.
// Spec: specs/B3-inventory-crafting.md §4.7 (the client's InteractionResult for entity clicks)
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntityUsePlan {
    /// The client predicts `InteractionResult.SUCCESS` and swings: send the click.
    Send,
    /// It opens only while crouching; standing it would mount.
    NeedsCrouch,
    /// It would mount, and crouching does not open anything.
    WouldMount,
    /// Not ported.
    Unsupported,
}

// Spec: specs/B3-inventory-crafting.md §4.7 (open a container on an entity by right-clicking it)
/// The client's prediction for a main-hand click on `kind` with the crouch state `secondary`
/// (`Player#isSecondaryUseActive`). Vanilla 1.21.11, Mojang names:
/// - `AbstractMinecartContainer#interact` (chest and hopper minecart): `interactWithContainerVehicle`,
///   which returns SUCCESS on the client whatever the crouch;
/// - `AbstractChestBoat#interact`: first `AbstractBoat#interact`, which on the client is SUCCESS when
///   standing (a mount is predicted) and PASS when crouching; then, crouching, the container
///   interaction (SUCCESS). A chest boat that cannot take a passenger opens standing too; this
///   function does not model that;
/// - a plain boat or minecart: standing is a mount, crouching is PASS.
pub fn entity_use_plan(kind: EntityKind, secondary: bool) -> EntityUsePlan {
    use EntityKind as K;
    match kind {
        K::ChestMinecart | K::HopperMinecart => EntityUsePlan::Send,
        K::AcaciaChestBoat
        | K::BambooChestRaft
        | K::BirchChestBoat
        | K::CherryChestBoat
        | K::DarkOakChestBoat
        | K::JungleChestBoat
        | K::MangroveChestBoat
        | K::OakChestBoat
        | K::PaleOakChestBoat
        | K::SpruceChestBoat => {
            if secondary {
                EntityUsePlan::Send
            } else {
                EntityUsePlan::NeedsCrouch
            }
        }
        K::AcaciaBoat
        | K::BambooRaft
        | K::BirchBoat
        | K::CherryBoat
        | K::DarkOakBoat
        | K::JungleBoat
        | K::MangroveBoat
        | K::OakBoat
        | K::PaleOakBoat
        | K::SpruceBoat
        | K::Minecart => {
            if secondary {
                EntityUsePlan::Unsupported
            } else {
                EntityUsePlan::WouldMount
            }
        }
        _ => EntityUsePlan::Unsupported,
    }
}

// Spec: specs/B3-inventory-crafting.md §4.7 (open a container on an entity by right-clicking it)
/// The hit point as the packet carries it: relative to the entity's position
/// (`MultiPlayerGameMode#interactAt` subtracts the entity's coordinates).
pub fn interact_at_location(hit: Vec3, entity_position: Vec3) -> Vec3 {
    hit - entity_position
}

// Spec: specs/B3-inventory-crafting.md §4.7 (open a container on an entity by right-clicking it)
/// The two packets of one main-hand click, in the order the client sends them.
pub fn interact_packets(
    entity_id: MinecraftEntityId,
    secondary: bool,
    hit: Vec3,
    entity_position: Vec3,
) -> [ServerboundInteract; 2] {
    [
        ServerboundInteract {
            entity_id,
            action: s_interact::ActionType::InteractAt {
                location: interact_at_location(hit, entity_position),
                hand: InteractionHand::MainHand,
            },
            using_secondary_action: secondary,
        },
        ServerboundInteract {
            entity_id,
            action: s_interact::ActionType::Interact { hand: InteractionHand::MainHand },
            using_secondary_action: secondary,
        },
    ]
}

// Spec: specs/B3-inventory-crafting.md §4.7 (open a container on an entity by right-clicking it)
/// Squared distance from `point` to the nearest point of `aabb` (vanilla `AABB#distanceToSqr`).
pub fn distance_sq_to_aabb(aabb: &Aabb, point: Vec3) -> f64 {
    let d = |lo: f64, hi: f64, p: f64| (lo - p).max(0.0).max(p - hi);
    let (dx, dy, dz) =
        (d(aabb.min.x, aabb.max.x, point.x), d(aabb.min.y, aabb.max.y, point.y), d(aabb.min.z, aabb.max.z, point.z));
    dx * dx + dy * dy + dz * dz
}

// Spec: specs/B3-inventory-crafting.md §4.7 (open a container on an entity by right-clicking it)
/// The client's checks for a click on `target`: reach to the hitbox first
/// (`Player#isWithinEntityInteractionRange` with no extra), then the crosshair ray. Returns the
/// crosshair's entity hit to send.
pub fn check_entity_click<'a>(
    eye: Vec3,
    view: Vec3,
    range: f64,
    target: Entity,
    target_box: &Aabb,
    hit: &'a HitResult,
) -> Result<&'a EntityHitResult, EntityUseRefusal> {
    if distance_sq_to_aabb(target_box, eye) >= range * range {
        return Err(EntityUseRefusal::OutOfReach);
    }
    match hit {
        HitResult::Entity(r) if r.entity == target => Ok(r),
        HitResult::Block(b) if !b.miss => {
            // A block nearer than the hitbox along the ray hides the entity.
            let behind = target_box
                .clip(eye, eye + view * (range + 1.0))
                .is_some_and(|p| p.distance_squared_to(eye) > b.location.distance_squared_to(eye));
            Err(if behind { EntityUseRefusal::BehindBlock } else { EntityUseRefusal::NotLookingAtIt })
        }
        _ => Err(EntityUseRefusal::NotLookingAtIt),
    }
}

/// The look whose crosshair ray hits `target`, found by trying points inside its hitbox (centre
/// first, then the others nearest first). `pick_at` runs the real pick for a look; a look update
/// snaps to 0.15 degree steps from where we face now, so that is the look tested.
// Spec: specs/B3-inventory-crafting.md §4.7 (aim at a point of the entity's real hitbox)
pub fn aim_at_entity(
    eye: Vec3,
    target_box: &Aabb,
    current: LookDirection,
    target: Entity,
    pick_at: impl Fn(LookDirection) -> HitResult,
) -> Option<LookDirection> {
    let size = Vec3::new(
        target_box.max.x - target_box.min.x,
        target_box.max.y - target_box.min.y,
        target_box.max.z - target_box.min.z,
    );
    let mut spots = Vec::with_capacity(27);
    for fx in [0.5, 0.25, 0.75] {
        for fy in [0.5, 0.25, 0.75] {
            for fz in [0.5, 0.25, 0.75] {
                let p = Vec3::new(
                    target_box.min.x + size.x * fx,
                    target_box.min.y + size.y * fy,
                    target_box.min.z + size.z * fz,
                );
                spots.push((fx == 0.5 && fy == 0.5 && fz == 0.5, p));
            }
        }
    }
    spots.sort_by(|a, b| {
        b.0.cmp(&a.0).then(a.1.distance_squared_to(eye).total_cmp(&b.1.distance_squared_to(eye)))
    });
    for (_, to) in spots {
        let d = to - eye;
        let flat = (d.x * d.x + d.z * d.z).sqrt();
        let look = LookDirection::new(
            (-d.x).atan2(d.z).to_degrees() as f32,
            (-d.y).atan2(flat).to_degrees() as f32,
        );
        let mut landed = current;
        landed.update(look);
        if matches!(pick_at(landed), HitResult::Entity(r) if r.entity == target) {
            return Some(look);
        }
    }
    None
}

// Spec: specs/B3-inventory-crafting.md §4.7 (aim at a point of the entity's real hitbox)
/// Turn to a point of the entity's hitbox where the crosshair hits it, on the next `GameTick`.
/// Does nothing if no such point is reachable from here; the click that follows is then refused.
/// Allow two ticks for the look to land before clicking.
#[derive(Component, Debug)]
pub struct LookAtEntityQueued {
    /// The entity's id on the server.
    pub entity_id: i32,
}

// Spec: specs/B3-inventory-crafting.md §4.7 (open a container on an entity by right-clicking it)
/// Right-click the entity with the main hand as a vanilla client does
/// (`Minecraft#startUseItem`, entity branch), on the next `GameTick`; the result appears in
/// [`EntityUseResults::last_use`]. Look at the entity first with [`LookAtEntityQueued`].
///
/// With `auto_crouch`, a click that opens only while crouching (a chest boat) crouches first,
/// clicks on the next tick and stands up again; without it that click is refused with
/// [`EntityUseRefusal::WouldMount`].
#[derive(Component, Debug)]
pub struct UseEntityQueued {
    pub entity_id: i32,
    pub auto_crouch: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UseEntityOutcome {
    /// Sent; nothing came back yet.
    Pending,
    /// The server opened a screen (the container's window id and menu kind).
    Opened { window_id: i32, menu: MenuKind },
    /// Sent, and no screen opened within [`NO_SCREEN_TICKS`].
    NoScreen,
    /// The client's own checks refused it; nothing was sent.
    Refused,
}

// Spec: specs/B3-inventory-crafting.md §4.7 (open a container on an entity by right-clicking it)
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UseEntityResult {
    /// Counts up per client, so a caller can tell a new click from the previous one.
    pub id: u32,
    pub entity_id: i32,
    pub outcome: UseEntityOutcome,
    pub refusal: Option<EntityUseRefusal>,
    /// What the crosshair check saw, when it refused after the ray was cast (none for refusals
    /// decided before: screen, busy, spectator, unknown or unsupported entity, dead).
    pub evidence: Option<ClickEvidence>,
    /// The hit point sent in INTERACT_AT, relative to the entity (when sent).
    pub sent_location: Option<Vec3>,
    /// Ticks since it was sent; stops counting when settled.
    pub elapsed_ticks: u32,
    /// Ticks from sending to the open (when it opened).
    pub open_ticks: Option<u32>,
}

// Spec: specs/B3-inventory-crafting.md §4.7 (open a container on an entity by right-clicking it)
/// The latest entity click result of a client.
#[derive(Clone, Component, Debug, Default)]
pub struct EntityUseResults {
    pub last_use: Option<UseEntityResult>,
    next_id: u32,
}

impl EntityUseResults {
    fn finish(
        &mut self,
        entity_id: i32,
        refusal: Option<EntityUseRefusal>,
        sent: Option<Vec3>,
        evidence: Option<ClickEvidence>,
    ) {
        self.next_id += 1;
        self.last_use = Some(UseEntityResult {
            id: self.next_id,
            entity_id,
            outcome: if refusal.is_some() { UseEntityOutcome::Refused } else { UseEntityOutcome::Pending },
            refusal,
            evidence,
            sent_location: sent,
            elapsed_ticks: 0,
            open_ticks: None,
        });
    }
}

// Spec: specs/F1-swarm-runtime.md §4.4 (a dead bot sends nothing a death screen cannot)
/// Answers entity commands that arrive with the death screen open with a refused result and sends
/// nothing (`Minecraft#handleKeybinds` does not run then).
#[allow(clippy::type_complexity)]
pub fn refuse_entity_actions_while_dead(
    mut commands: Commands,
    mut query: Query<
        (
            Entity,
            &mut EntityUseResults,
            &mut RespawnLog,
            Option<&UseEntityQueued>,
            Option<&LookAtEntityQueued>,
        ),
        With<DeathScreen>,
    >,
) {
    for (entity, mut results, mut log, use_q, look_q) in &mut query {
        if let Some(q) = use_q {
            results.finish(q.entity_id, Some(EntityUseRefusal::Dead), None, None);
            log.refuse(DeadAction::UseEntity);
            commands.entity(entity).remove::<UseEntityQueued>();
        }
        if look_q.is_some() {
            log.refuse(DeadAction::LookAtEntity);
            commands.entity(entity).remove::<LookAtEntityQueued>();
        }
    }
}

// The crosshair for `look` from `eye`: blocks occlude, entity range from the attribute.
#[allow(clippy::too_many_arguments)]
fn crosshair(
    source: Entity,
    look: LookDirection,
    eye: Vec3,
    bbox: &Aabb,
    world: &World,
    attrs: &Attributes,
    aabb_query: &AabbQuery,
    pickable: &MaybePickableEntityQuery,
) -> HitResult {
    pick(PickOpts {
        source_entity: source,
        look_direction: look,
        eye_position: eye,
        aabb: bbox,
        world,
        entity_pick_range: attrs.entity_interaction_range.calculate(),
        block_pick_range: attrs.block_interaction_range.calculate(),
        aabb_query,
        pickable_query: pickable,
    })
}

// Spec: specs/B3-inventory-crafting.md §4.7 (aim at a point of the entity's real hitbox)
#[allow(clippy::type_complexity)]
pub fn handle_look_at_entity_queued(
    mut commands: Commands,
    mut query: Query<(
        Entity,
        &LookAtEntityQueued,
        &mut LookDirection,
        &WorldHolder,
        &Position,
        &EntityDimensions,
        &Attributes,
        &Physics,
        &EntityIdIndex,
    )>,
    boxes: Query<&LastBoundingBox>,
    aabb_query: AabbQuery,
    pickable: MaybePickableEntityQuery,
) {
    for (entity, queued, mut look, world, position, dims, attrs, physics, ids) in &mut query {
        commands.entity(entity).remove::<LookAtEntityQueued>();
        let Some(target) = ids.get_by_minecraft_entity(MinecraftEntityId(queued.entity_id)) else {
            continue;
        };
        let Ok(target_box) = boxes.get(target) else { continue };
        let eye = position.up(dims.eye_height.into());
        let world = world.shared.read();
        let current = *look;
        let aim = aim_at_entity(eye, &target_box.0, current, target, |l| {
            crosshair(entity, l, eye, &physics.bounding_box, &world, attrs, &aabb_query, &pickable)
        });
        if let Some(aim) = aim {
            look.update(aim);
        }
    }
}

// Ends a queued click with a refusal; a held auto-crouch is let go.
fn refuse(
    commands: &mut Commands,
    entity: Entity,
    holding: bool,
    mv: &mut ClientMovementState,
    results: &mut EntityUseResults,
    id: i32,
    why: EntityUseRefusal,
    evidence: Option<ClickEvidence>,
) {
    commands.entity(entity).remove::<UseEntityQueued>();
    if holding {
        mv.trying_to_crouch = false;
        commands.entity(entity).remove::<AutoCrouchHold>();
    }
    results.finish(id, Some(why), None, evidence);
}

// Spec: specs/B3-inventory-crafting.md §4.7 (right-click like `Minecraft#startUseItem`, entity branch)
#[allow(clippy::type_complexity)]
pub fn handle_use_entity_queued(
    mut commands: Commands,
    mut query: Query<(
        Entity,
        &UseEntityQueued,
        &mut EntityUseResults,
        &WorldHolder,
        &Position,
        &EntityDimensions,
        &Attributes,
        &Physics,
        &EntityIdIndex,
        (&mut ClientMovementState, Option<&Crouching>, Has<AutoCrouchHold>),
        &mut LookDirection,
        (Has<OpenScreen>, Has<Mining>, Has<ItemInUse>, Has<DeathScreen>),
        (&Inventory, Option<&LocalGameMode>),
        &TicksConnected,
    )>,
    entities: EvidenceEntities,
    targets: Query<(&EntityKindComponent, &Position, &LastBoundingBox)>,
    aabb_query: AabbQuery,
    pickable: MaybePickableEntityQuery,
) {
    for (
        entity,
        queued,
        mut results,
        world,
        position,
        dims,
        attrs,
        physics,
        ids,
        (mut mv, crouching, holding),
        mut look,
        (screen, mining, using, dead),
        (inv, game_mode),
        connected,
    ) in &mut query
    {
        let id = queued.entity_id;
        // Same states as `UseBlockQueued` (a vanilla client has no crosshair with a screen open,
        // does not start a use while mining or using an item), plus spectators.
        if dead || screen || inv.id != 0 {
            refuse(&mut commands, entity, holding, &mut mv, &mut results, id, if dead { EntityUseRefusal::Dead } else { EntityUseRefusal::ScreenOpen }, None);
            continue;
        }
        if mining || using {
            refuse(&mut commands, entity, holding, &mut mv, &mut results, id, EntityUseRefusal::Busy, None);
            continue;
        }
        if game_mode.is_some_and(|g| g.current == GameMode::Spectator) {
            refuse(&mut commands, entity, holding, &mut mv, &mut results, id, EntityUseRefusal::Spectator, None);
            continue;
        }
        let Some((target, (kind, target_position, target_box))) = ids
            .get_by_minecraft_entity(MinecraftEntityId(id))
            .and_then(|e| targets.get(e).ok().map(|(k, p, b)| (e, (k.0, **p, b.0))))
        else {
            refuse(&mut commands, entity, holding, &mut mv, &mut results, id, EntityUseRefusal::UnknownEntity, None);
            continue;
        };
        let secondary = is_crouching(&mv, crouching);
        let plan = entity_use_plan(kind, secondary);
        if plan == EntityUsePlan::Unsupported {
            refuse(&mut commands, entity, holding, &mut mv, &mut results, id, EntityUseRefusal::UnsupportedEntity, None);
            continue;
        }
        let world = world.shared.read();
        let eye = position.up(dims.eye_height.into());
        let hit = crosshair(entity, *look, eye, &physics.bounding_box, &world, attrs, &aabb_query, &pickable);
        let range = attrs.entity_interaction_range.calculate();
        let checked = check_entity_click(eye, view_vector(*look), range, target, &target_box, &hit);
        // Patch 28: what this check saw, kept for a refusal. The pick above is fresh, so the hit
        // cannot be older than the look.
        let evidence = ClickEvidence::capture(
            connected.0,
            eye,
            dims.eye_height.into(),
            *look,
            secondary,
            &hit,
            false,
            &entities,
        );
        let hit = match checked {
            Ok(h) => h.clone(),
            Err(why) => {
                refuse(&mut commands, entity, holding, &mut mv, &mut results, id, why, Some(evidence));
                continue;
            }
        };
        // A click that mounts when standing: crouch first (the crouch input goes out this tick,
        // the click follows next tick), or refuse. The eye drops 0.35, so aim again from it.
        if plan != EntityUsePlan::Send {
            // Holding the crouch and still not crouching: something let go of it; never click
            // standing.
            if holding || plan == EntityUsePlan::WouldMount || !queued.auto_crouch {
                refuse(&mut commands, entity, holding, &mut mv, &mut results, id, EntityUseRefusal::WouldMount, Some(evidence));
                continue;
            }
            mv.trying_to_crouch = true;
            let crouched_eye = position.up(CROUCH_EYE_HEIGHT);
            let current = *look;
            let aim = aim_at_entity(crouched_eye, &target_box, current, target, |l| {
                crosshair(entity, l, crouched_eye, &physics.bounding_box, &world, attrs, &aabb_query, &pickable)
            });
            if let Some(aim) = aim {
                look.update(aim);
            }
            commands.entity(entity).insert(AutoCrouchHold);
            continue;
        }
        commands.entity(entity).remove::<UseEntityQueued>();
        if holding {
            // The click goes out below; stand up again after it (the earlier state was standing).
            mv.trying_to_crouch = false;
            commands.entity(entity).remove::<AutoCrouchHold>();
        }
        // `Minecraft#startUseItem` (entity branch): `interactAt` then `interact` for the main hand,
        // then the arm swing for the predicted SUCCESS.
        let packets = interact_packets(MinecraftEntityId(id), secondary, hit.location, target_position);
        results.finish(id, None, Some(interact_at_location(hit.location, target_position)), None);
        for p in packets {
            commands.trigger(SendGamePacketEvent::new(entity, p));
        }
        commands.trigger(SwingArmEvent { entity });
    }
}

// Spec: specs/B3-inventory-crafting.md §4.7 (a screen opening, or not, after an entity click)
/// Settles a pending entity click: opened when the server's open-screen arrives; no screen when
/// [`NO_SCREEN_TICKS`] pass without one.
pub fn settle_use_entities(
    mut query: Query<(&mut EntityUseResults, &Inventory, Option<&OpenScreen>)>,
) {
    for (mut results, inv, screen) in &mut query {
        let Some(r) = results.last_use.as_mut().filter(|r| r.outcome == UseEntityOutcome::Pending)
        else {
            continue;
        };
        r.elapsed_ticks += 1;
        if let (Some(kind), true) = (screen.and_then(|s| s.kind), inv.id != 0) {
            r.outcome = UseEntityOutcome::Opened { window_id: inv.id, menu: kind };
            r.open_ticks = Some(r.elapsed_ticks);
        } else if r.elapsed_ticks >= NO_SCREEN_TICKS {
            r.outcome = UseEntityOutcome::NoScreen;
        }
    }
}

#[cfg(test)]
mod tests {
    use azalea_core::{direction::Direction, hit_result::BlockHitResult, position::BlockPos};

    use super::*;

    fn bx(x0: f64, y0: f64, z0: f64, x1: f64, y1: f64, z1: f64) -> Aabb {
        Aabb { min: Vec3::new(x0, y0, z0), max: Vec3::new(x1, y1, z1) }
    }

    fn target() -> Entity {
        Entity::from_raw_u32(7).unwrap()
    }

    fn other() -> Entity {
        Entity::from_raw_u32(8).unwrap()
    }

    #[test]
    fn location_is_relative_to_the_entity() {
        let rel = interact_at_location(Vec3::new(10.25, 64.5, -3.0), Vec3::new(10.0, 64.0, -3.5));
        assert_eq!((rel.x, rel.y, rel.z), (0.25, 0.5, 0.5));
    }

    #[test]
    fn packets_are_interact_at_then_interact_with_the_crouch_flag() {
        let id = MinecraftEntityId(42);
        for secondary in [false, true] {
            let [a, b] = interact_packets(id, secondary, Vec3::new(5.5, 70.0, 5.5), Vec3::new(5.0, 69.5, 5.0));
            match a.action {
                s_interact::ActionType::InteractAt { location, hand } => {
                    assert_eq!((location.x, location.y, location.z), (0.5, 0.5, 0.5));
                    assert_eq!(hand, InteractionHand::MainHand);
                }
                other => panic!("first packet is {other:?}"),
            }
            assert!(matches!(
                b.action,
                s_interact::ActionType::Interact { hand: InteractionHand::MainHand }
            ));
            assert_eq!((a.entity_id, b.entity_id), (id, id));
            assert_eq!((a.using_secondary_action, b.using_secondary_action), (secondary, secondary));
        }
    }

    #[test]
    fn plans_follow_the_vanilla_interact_methods() {
        use EntityKind as K;
        use EntityUsePlan as P;
        // container minecarts open whatever the crouch
        for k in [K::ChestMinecart, K::HopperMinecart] {
            assert_eq!(entity_use_plan(k, false), P::Send);
            assert_eq!(entity_use_plan(k, true), P::Send);
        }
        // a chest boat opens only crouched; standing it is a mount
        for k in [K::OakChestBoat, K::BambooChestRaft, K::PaleOakChestBoat] {
            assert_eq!(entity_use_plan(k, false), P::NeedsCrouch);
            assert_eq!(entity_use_plan(k, true), P::Send);
        }
        // a plain boat or minecart: a mount standing, nothing crouched
        for k in [K::OakBoat, K::Minecart] {
            assert_eq!(entity_use_plan(k, false), P::WouldMount);
            assert_eq!(entity_use_plan(k, true), P::Unsupported);
        }
        assert_eq!(entity_use_plan(K::Zombie, false), P::Unsupported);
    }

    #[test]
    fn reach_is_measured_to_the_hitbox() {
        let b = bx(-0.49, 0.0, 3.2, 0.49, 0.7, 4.2);
        // the entity's position (0, 0, 3.7) is 3.7 from the eye at the origin, but the box is 3.2
        let d = distance_sq_to_aabb(&b, Vec3::new(0.0, 0.0, 0.0)).sqrt();
        assert!((d - 3.2).abs() < 1e-9);
        let near = bx(-0.49, 0.0, 2.9, 0.49, 0.7, 3.9);
        assert!(distance_sq_to_aabb(&near, Vec3::ZERO).sqrt() < 3.0);
        assert_eq!(distance_sq_to_aabb(&near, Vec3::new(0.0, 0.3, 3.0)), 0.0);
    }

    #[test]
    fn click_checks() {
        let eye = Vec3::ZERO;
        let view = Vec3::new(0.0, 0.0, 1.0);
        let b = bx(-0.49, -0.2, 2.0, 0.49, 0.5, 3.0);
        let hit_entity = HitResult::Entity(EntityHitResult { location: Vec3::new(0.0, 0.0, 2.0), entity: target() });
        assert!(check_entity_click(eye, view, 3.0, target(), &b, &hit_entity).is_ok());
        // another entity is hit
        let wrong = HitResult::Entity(EntityHitResult { location: Vec3::new(0.0, 0.0, 2.0), entity: other() });
        assert_eq!(check_entity_click(eye, view, 3.0, target(), &b, &wrong).unwrap_err(), EntityUseRefusal::NotLookingAtIt);
        // a miss
        let miss = HitResult::new_miss(Vec3::new(0.0, 0.0, 3.0), Direction::South, BlockPos::new(0, 0, 3));
        assert_eq!(check_entity_click(eye, view, 3.0, target(), &b, &miss).unwrap_err(), EntityUseRefusal::NotLookingAtIt);
        // a block nearer than the box on the ray
        let block = |z: f64| {
            HitResult::Block(BlockHitResult {
                location: Vec3::new(0.0, 0.0, z),
                direction: Direction::North,
                block_pos: BlockPos::new(0, 0, z as i32),
                inside: false,
                miss: false,
                world_border: false,
            })
        };
        assert_eq!(check_entity_click(eye, view, 3.0, target(), &b, &block(1.0)).unwrap_err(), EntityUseRefusal::BehindBlock);
        // a block behind the box does not hide it, but this ray did not hit the entity
        assert_eq!(check_entity_click(eye, view, 3.0, target(), &b, &block(5.0)).unwrap_err(), EntityUseRefusal::NotLookingAtIt);
        // box out of reach (reach is tested first)
        let far = bx(-0.49, -0.2, 3.5, 0.49, 0.5, 4.5);
        assert_eq!(check_entity_click(eye, view, 3.0, target(), &far, &hit_entity).unwrap_err(), EntityUseRefusal::OutOfReach);
        // exactly at the range is out (vanilla: strictly less)
        let edge = bx(-0.49, -0.2, 3.0, 0.49, 0.5, 4.0);
        assert_eq!(check_entity_click(eye, view, 3.0, target(), &edge, &hit_entity).unwrap_err(), EntityUseRefusal::OutOfReach);
    }

    #[test]
    fn aim_finds_a_ray_that_hits_the_box() {
        let eye = Vec3::new(0.5, 1.62, 0.5);
        let b = bx(0.0, 1.0, 2.0, 1.0, 1.7, 3.0);
        // a stand-in for the real pick: a ray of 3 blocks against the box
        let pick_at = |look: LookDirection| {
            let end = eye + view_vector(look) * 3.0;
            match b.clip(eye, end) {
                Some(p) => HitResult::Entity(EntityHitResult { location: p, entity: target() }),
                None => HitResult::new_miss(end, Direction::South, BlockPos::new(0, 0, 0)),
            }
        };
        let aim = aim_at_entity(eye, &b, LookDirection::new(90.0, 0.0), target(), pick_at).expect("aim");
        let mut landed = LookDirection::new(90.0, 0.0);
        landed.update(aim);
        assert!(matches!(pick_at(landed), HitResult::Entity(_)));
        // nothing reachable: the box is 5 blocks away
        let far = bx(0.0, 1.0, 5.0, 1.0, 1.7, 6.0);
        let pick_far = |look: LookDirection| {
            let end = eye + view_vector(look) * 3.0;
            match far.clip(eye, end) {
                Some(p) => HitResult::Entity(EntityHitResult { location: p, entity: target() }),
                None => HitResult::new_miss(end, Direction::South, BlockPos::new(0, 0, 0)),
            }
        };
        assert!(aim_at_entity(eye, &far, LookDirection::new(0.0, 0.0), target(), pick_far).is_none());
    }
}
