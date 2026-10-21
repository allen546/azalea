//! The client's scoreboard teams and the collision-rule table (Null fork, patch 32).
//!
//! A vanilla client keeps its own scoreboard, filled by `ClientboundSetPlayerTeam`
//! (`ClientPacketListener#handleSetPlayerTeamPacket`, `Scoreboard#addPlayerToTeam`,
//! `#removePlayerFromTeam`, `#removePlayerTeam`). Who may push whom reads it
//! (`EntitySelector#pushableBy`, `Team#isAlliedTo`). The server can send different teams to
//! different players, so each bot keeps its own [`Scoreboard`].
//!
//! Grim holds the same table (`EntityPredicates#canBePushedBy`, check `Simulation` through
//! `MovementTicker#handleEntityCollisions`).
//!
//! Spec: specs/B1-movement.md §4.2 (Pushes from entities)

use std::collections::HashMap;

use bevy_ecs::component::Component;

/// `Team.CollisionRule`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CollisionRule {
    Always,
    Never,
    PushOtherTeams,
    PushOwnTeam,
}

/// One team: its collision rule. Membership is kept in [`Scoreboard::team_of`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Team {
    pub collision_rule: CollisionRule,
}

/// A team as the push rule sees it: the team's name (for the "same team" test) and its rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TeamRef<'a> {
    pub name: &'a str,
    pub rule: CollisionRule,
}

/// One bot's scoreboard: the teams by name, and the team of each member name. A member name is
/// the profile name for a player and the UUID as text for every other entity
/// (`Entity#getScoreboardName`).
// Spec: specs/B1-movement.md §4.2 (Pushes from entities)
#[derive(Clone, Component, Debug, Default)]
pub struct Scoreboard {
    teams: HashMap<String, Team>,
    team_of: HashMap<String, String>,
}

impl Scoreboard {
    /// `Scoreboard#addPlayerTeam` for an Add packet: an existing name is reused.
    pub fn add_team(&mut self, name: &str, rule: CollisionRule) {
        self.teams
            .entry(name.to_owned())
            .and_modify(|t| t.collision_rule = rule)
            .or_insert(Team { collision_rule: rule });
    }

    /// Change packet. Returns false when the team is unknown (vanilla ignores it with a warning).
    pub fn set_rule(&mut self, name: &str, rule: CollisionRule) -> bool {
        match self.teams.get_mut(name) {
            Some(t) => {
                t.collision_rule = rule;
                true
            }
            None => false,
        }
    }

    /// Remove packet: the team goes and so does every membership. False when unknown.
    pub fn remove_team(&mut self, name: &str) -> bool {
        if self.teams.remove(name).is_none() {
            return false;
        }
        self.team_of.retain(|_, t| t != name);
        true
    }

    /// Join packet, one name: `Scoreboard#addPlayerToTeam` takes the name off its old team first.
    /// False when the team is unknown.
    pub fn join(&mut self, team: &str, member: &str) -> bool {
        if !self.teams.contains_key(team) {
            return false;
        }
        self.team_of.insert(member.to_owned(), team.to_owned());
        true
    }

    /// Leave packet, one name. Vanilla throws when the name is on another team; the fork ignores
    /// it. Returns whether the name was on that team.
    pub fn leave(&mut self, team: &str, member: &str) -> bool {
        if self.team_of.get(member).is_some_and(|t| t == team) {
            self.team_of.remove(member);
            true
        } else {
            false
        }
    }

    pub fn has_team(&self, name: &str) -> bool {
        self.teams.contains_key(name)
    }
    pub fn rule_of_team(&self, name: &str) -> Option<CollisionRule> {
        self.teams.get(name).map(|t| t.collision_rule)
    }
    /// The team a member name is on.
    pub fn team_of(&self, member: &str) -> Option<&str> {
        self.team_of.get(member).map(String::as_str)
    }
    /// The member's team with its rule (`Entity#getTeam`).
    pub fn team_ref(&self, member: &str) -> Option<TeamRef<'_>> {
        let name = self.team_of.get(member)?;
        let team = self.teams.get(name)?;
        Some(TeamRef { name, rule: team.collision_rule })
    }
    /// No teams at all: every pair is `Always`, so a caller can skip the name lookup.
    pub fn is_empty(&self) -> bool {
        self.teams.is_empty()
    }
    pub fn team_count(&self) -> usize {
        self.teams.len()
    }
    pub fn members_of(&self, team: &str) -> usize {
        self.team_of.values().filter(|t| *t == team).count()
    }
}

/// `EntitySelector#pushableBy`, team part: may `pusher` push `target`? No team counts as
/// `Always`. `Team#isAlliedTo` compares team identity, so a missing team is never allied.
// Spec: specs/B1-movement.md §4.2 (Pushes from entities)
pub fn team_allows_push(pusher: Option<TeamRef<'_>>, target: Option<TeamRef<'_>>) -> bool {
    let rule_a = pusher.map_or(CollisionRule::Always, |t| t.rule);
    let rule_b = target.map_or(CollisionRule::Always, |t| t.rule);
    if rule_a == CollisionRule::Never || rule_b == CollisionRule::Never {
        return false;
    }
    let allied = matches!((pusher, target), (Some(a), Some(b)) if a.name == b.name);
    if allied && (rule_a == CollisionRule::PushOwnTeam || rule_b == CollisionRule::PushOwnTeam) {
        return false;
    }
    let other_teams = rule_a == CollisionRule::PushOtherTeams || rule_b == CollisionRule::PushOtherTeams;
    !other_teams || allied
}

#[cfg(test)]
mod tests {
    use super::*;
    use CollisionRule::*;

    const ALL: [CollisionRule; 4] = [Always, Never, PushOtherTeams, PushOwnTeam];

    /// The expected answer written as a case list from the vanilla text, not from the function:
    /// never on either side blocks; allied teams push unless one is push-own-team; teams that
    /// are not allied push unless one is push-other-teams.
    fn expected(a: CollisionRule, b: CollisionRule, allied: bool) -> bool {
        if a == Never || b == Never {
            return false;
        }
        if allied {
            return a != PushOwnTeam && b != PushOwnTeam;
        }
        a != PushOtherTeams && b != PushOtherTeams
    }

    #[test]
    fn the_table_holds_for_all_32_cases() {
        let mut n = 0;
        for a in ALL {
            for b in ALL {
                for allied in [false, true] {
                    let ta = TeamRef { name: "t1", rule: a };
                    let tb = TeamRef { name: if allied { "t1" } else { "t2" }, rule: b };
                    assert_eq!(team_allows_push(Some(ta), Some(tb)), expected(a, b, allied), "{a:?} {b:?} allied {allied}");
                    n += 1;
                }
            }
        }
        assert_eq!(n, 32);
    }

    #[test]
    fn a_missing_team_is_always_and_never_allied() {
        for r in ALL {
            let t = Some(TeamRef { name: "t", rule: r });
            assert_eq!(team_allows_push(None, t), expected(Always, r, false));
            assert_eq!(team_allows_push(t, None), expected(r, Always, false));
        }
        assert!(team_allows_push(None, None));
    }

    #[test]
    fn join_replaces_the_old_team_and_leave_of_a_non_member_is_ignored() {
        let mut sb = Scoreboard::default();
        sb.add_team("a", Always);
        sb.add_team("b", Never);
        assert!(sb.join("a", "x"));
        assert_eq!(sb.team_of("x"), Some("a"));
        assert!(sb.join("b", "x"));
        assert_eq!(sb.team_of("x"), Some("b"));
        assert_eq!(sb.members_of("a"), 0);
        assert!(!sb.leave("a", "x"), "x is on b");
        assert_eq!(sb.team_of("x"), Some("b"));
        assert!(sb.leave("b", "x"));
        assert_eq!(sb.team_of("x"), None);
        assert!(!sb.join("nope", "x"), "unknown team");
    }

    #[test]
    fn remove_clears_members_and_unknown_teams_are_ignored() {
        let mut sb = Scoreboard::default();
        sb.add_team("a", PushOwnTeam);
        sb.join("a", "x");
        sb.join("a", "y");
        assert!(!sb.set_rule("zzz", Never));
        assert!(!sb.remove_team("zzz"));
        assert!(sb.set_rule("a", Never));
        assert_eq!(sb.team_ref("x").unwrap().rule, Never);
        assert!(sb.remove_team("a"));
        assert_eq!(sb.team_of("x"), None);
        assert_eq!(sb.team_of("y"), None);
        assert!(sb.is_empty());
    }

    #[test]
    fn add_reuses_an_existing_team() {
        let mut sb = Scoreboard::default();
        sb.add_team("a", Always);
        sb.join("a", "x");
        sb.add_team("a", Never);
        assert_eq!(sb.team_count(), 1);
        assert_eq!(sb.team_ref("x").unwrap().rule, Never);
    }
}
