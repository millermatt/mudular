//! What to draw, in grid space, with no idea what will draw it (§16).
//!
//! `ui::draw_map` used to hold both halves of the map picture: which rooms
//! and corridors exist where, and which characters to put in which terminal
//! cell. Those answer different questions and change for different reasons —
//! the first is about the world, the second about the display — so the first
//! lives here, beside `path`, `describe` and `layout_area`.
//!
//! This is the same split §16 already made for prose: `Map::describe` says
//! what the map *knows* about a room and the renderer is one consumer of
//! that knowledge, not the only place it exists. A [`Scene`] is that for the
//! drawn form. It carries grid coordinates rather than terminal cells, so a
//! renderer that paints pixels rather than characters consumes exactly what
//! the character renderer does.

use super::{Map, RoomId};

/// Why a room is drawn the way it is. Deliberately about *meaning* rather
/// than appearance — a renderer decides whether "the room you are in" is a
/// bright block, an inverse-video cell, or a marker on a canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomRole {
    /// Where the character is standing.
    Here,
    /// Where a `corpse:` trigger last said they died (§16).
    Corpse,
    /// Everywhere else that is on the map.
    Known,
}

/// A room, placed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedRoom {
    pub id: RoomId,
    /// Grid position relative to the centred room, x right and y *down* —
    /// screen sense, matching `direction_vector`.
    pub at: (i32, i32),
    pub role: RoomRole,
    /// Whether this room also leads off the flat grid, and which way. `u`
    /// and `d` are real, pathable exits with no 2D displacement (§16), so
    /// they are a property of the room rather than a corridor.
    pub up: bool,
    pub down: bool,
}

/// A corridor between two placed rooms, one grid step long.
///
/// Only exits whose two rooms actually landed where the direction says they
/// lie become links — see `Map::scene`. A corridor that disagrees with its
/// own direction is not drawn at all, so every link here can be trusted to
/// point the way it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub from: (i32, i32),
    /// One of the eight unit steps `direction_vector` produces.
    pub step: (i32, i32),
}

/// Everything a renderer needs to draw the map around a room.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scene {
    pub rooms: Vec<PlacedRoom>,
    pub links: Vec<Link>,
    /// The area being drawn, for a renderer that wants to title itself.
    pub area: Option<String>,
}

impl Map {
    /// Builds the scene around `current`, from `layout_area`'s coordinates.
    ///
    /// Rooms that lost a coordinate collision, or sit in another area, are
    /// simply absent — they stay in the graph and stay pathable, which is
    /// why the drawn map is never what `/goto` consults (§16).
    pub fn scene(&self, current: RoomId, corpse: Option<RoomId>) -> Scene {
        let coords = self.layout_area(current);
        let mut rooms: Vec<PlacedRoom> = Vec::with_capacity(coords.len());
        let mut links = Vec::new();

        for (id, at) in &coords {
            let Some(room) = self.rooms.get(id) else {
                continue;
            };
            rooms.push(PlacedRoom {
                id: *id,
                at: *at,
                role: match Some(*id) {
                    _ if *id == current => RoomRole::Here,
                    c if c == corpse => RoomRole::Corpse,
                    _ => RoomRole::Known,
                },
                up: room.exits.contains_key("u"),
                down: room.exits.contains_key("d"),
            });

            for (direction, dest) in &room.exits {
                let Some(dest) = dest else { continue };
                let Some(there) = coords.get(dest) else {
                    continue;
                };
                let Some(step) = super::direction_vector(direction) else {
                    continue;
                };
                // The layout has to have honoured this exit's geometry, or
                // there is no corridor to draw — only a gap. See
                // `a_cardinal_exit_can_still_land_two_rooms_diagonally_apart`.
                if (there.0 - at.0, there.1 - at.1) == step {
                    links.push(Link { from: *at, step });
                }
            }
        }

        // Coordinates come out of a `HashMap`, so fix an order: two runs of
        // the same map must produce the same picture, and a test that
        // compares scenes needs something stable to compare.
        rooms.sort_by_key(|room| (room.at.1, room.at.0));
        links.sort_by_key(|link| (link.from.1, link.from.0, link.step));

        Scene {
            rooms,
            links,
            area: self.rooms.get(&current).and_then(|room| room.area.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::map::RoomInfo;

    fn map_of(rooms: &[i64], edges: &[(i64, &str, i64)]) -> Map {
        let mut map = Map::default();
        for id in rooms {
            map.observe(&RoomInfo {
                id: RoomId(*id),
                name: None,
                area: Some("Test".to_string()),
                exits: BTreeMap::new(),
            });
        }
        for (from, dir, to) in edges {
            map.connect(RoomId(*from), dir, RoomId(*to));
        }
        map
    }

    #[test]
    fn places_rooms_and_links_around_the_centred_room() {
        let map = map_of(&[1, 2], &[(1, "e", 2)]);

        let scene = map.scene(RoomId(1), None);

        assert_eq!(scene.rooms.len(), 2);
        assert_eq!(
            scene.rooms.iter().find(|r| r.id == RoomId(1)).unwrap().at,
            (0, 0)
        );
        assert_eq!(
            scene.rooms.iter().find(|r| r.id == RoomId(2)).unwrap().at,
            (1, 0)
        );
        assert_eq!(
            scene.links,
            vec![Link {
                from: (0, 0),
                step: (1, 0)
            }]
        );
    }

    /// The reason the scene exists as its own thing: this filtering is a
    /// fact about the *map*, not about characters or pixels, so every
    /// renderer inherits it rather than each re-deriving it.
    #[test]
    fn an_exit_the_layout_could_not_honour_becomes_no_link_at_all() {
        // Cardinal exits only, but room 4 is sited by 1's `n` before room
        // 2's `s` is considered, so that `s` connects two rooms lying
        // diagonally apart.
        let map = map_of(&[1, 2, 4], &[(1, "e", 2), (1, "n", 4), (2, "s", 4)]);

        let scene = map.scene(RoomId(1), None);

        assert!(
            scene.links.iter().all(|l| l.step.0 == 0 || l.step.1 == 0),
            "no diagonal link may come out of a map with no diagonal exits: {:?}",
            scene.links
        );
        assert!(
            !scene.links.contains(&Link {
                from: (1, 0),
                step: (0, 1)
            }),
            "and the unhonourable `s` is not drawn pointing somewhere else either"
        );
    }

    #[test]
    fn roles_mark_the_current_room_and_the_corpse() {
        let map = map_of(&[1, 2], &[(1, "e", 2)]);

        let scene = map.scene(RoomId(1), Some(RoomId(2)));

        let role = |id: i64| {
            scene
                .rooms
                .iter()
                .find(|r| r.id == RoomId(id))
                .unwrap()
                .role
        };
        assert_eq!(role(1), RoomRole::Here);
        assert_eq!(role(2), RoomRole::Corpse);
    }

    /// Standing on your own corpse: you are still the thing that needs
    /// finding on screen, so `Here` wins.
    #[test]
    fn standing_on_the_corpse_still_reads_as_here() {
        let map = map_of(&[1], &[]);

        let scene = map.scene(RoomId(1), Some(RoomId(1)));

        assert_eq!(scene.rooms[0].role, RoomRole::Here);
    }

    /// `u`/`d` have no displacement, so they are a property of the room
    /// rather than a corridor (§16) — and they must not become links.
    #[test]
    fn vertical_exits_mark_the_room_and_draw_no_corridor() {
        let map = map_of(&[1, 2], &[(1, "u", 2)]);

        let scene = map.scene(RoomId(1), None);

        let here = scene.rooms.iter().find(|r| r.id == RoomId(1)).unwrap();
        assert!(here.up, "the room it leaves is marked");
        assert!(!here.down);
        assert!(scene.links.is_empty(), "and nothing is drawn as a corridor");
    }

    #[test]
    fn a_scene_is_stable_across_runs() {
        let map = map_of(&[1, 2, 3], &[(1, "e", 2), (1, "s", 3)]);

        assert_eq!(map.scene(RoomId(1), None), map.scene(RoomId(1), None));
    }
}
