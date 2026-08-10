//! The room graph: what the client has learned about the world's shape,
//! and how to path across it (docs/ARCHITECTURE.md §16).
//!
//! A dependency-free leaf, like [`crate::scrollback`]: it takes the already
//! flattened server-data map as a plain `HashMap` and does no I/O, so it
//! stays inside §4's boundary rule. Persistence lives in `config`, the map
//! pane lives in `ui`, and neither is visible from here.
//!
//! **Rooms come from whichever protocol supplied them.** GMCP and MSDP both
//! flatten into one server-data namespace (§6.3), so extraction reads that
//! merged store rather than a protocol's own event — the same reasoning
//! §7.5 gives for naming a key prefix and not a protocol, so that a server
//! switching protocols without changing its data does not break anything
//! downstream. Text-inferred mapping for MUDs that send neither is
//! deliberately out of scope: it is MUD-specific guesswork, where this is
//! structured data the server already vouches for.

// The graph lands before anything consumes it: `session` feeds it and `ui`
// draws it, and until they do its public surface is exercised only by this
// module's own tests. Comes out as soon as they call in — a module-wide
// allow that outlives its reason is how genuinely dead code hides.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

/// A room's identity: the server's own vnum. Not a synthesized id — two
/// runs, two characters, and a reloaded map file all have to agree on what
/// "that room" means, and only the server can say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoomId(pub i64);

/// The directions the map understands, longest first so `ne` is preferred
/// over `n` when both could match a prefix.
///
/// Kept here rather than reusing `engine`'s `SPEEDWALK_DIRECTIONS`, which
/// answers a different question: that list parses packed `.3n2e` paths, so
/// it is abbreviations only. This one canonicalises what a *server* calls a
/// direction, where `north` and `n` are the same exit. Anything else that
/// has to recognise a movement command should delegate here rather than
/// growing a third vocabulary.
const DIRECTIONS: &[(&str, &str)] = &[
    ("northeast", "ne"),
    ("northwest", "nw"),
    ("southeast", "se"),
    ("southwest", "sw"),
    ("north", "n"),
    ("south", "s"),
    ("east", "e"),
    ("west", "w"),
    ("up", "u"),
    ("down", "d"),
    ("in", "in"),
    ("out", "out"),
    ("ne", "ne"),
    ("nw", "nw"),
    ("se", "se"),
    ("sw", "sw"),
    ("n", "n"),
    ("s", "s"),
    ("e", "e"),
    ("w", "w"),
    ("u", "u"),
    ("d", "d"),
];

/// Canonicalises a server's spelling of a direction (`North`, `north`, `n`)
/// to the short form the map stores and sends. `None` for anything that is
/// not a direction at all.
pub fn canonical_direction(raw: &str) -> Option<&'static str> {
    let lower = raw.trim().to_ascii_lowercase();
    DIRECTIONS
        .iter()
        .find(|(spelling, _)| *spelling == lower)
        .map(|(_, canonical)| *canonical)
}

/// How a direction moves a room on the rendered grid. `None` for exits with
/// no 2D meaning (`u`/`d`/`in`/`out`) — they are real exits and stay
/// pathable, they just cannot be drawn as a displacement on a flat map.
fn direction_vector(direction: &str) -> Option<(i32, i32)> {
    // Screen coordinates: y grows downward, so north is -1.
    Some(match direction {
        "n" => (0, -1),
        "s" => (0, 1),
        "e" => (1, 0),
        "w" => (-1, 0),
        "ne" => (1, -1),
        "nw" => (-1, -1),
        "se" => (1, 1),
        "sw" => (-1, 1),
        _ => return None,
    })
}

/// One room-data update, as extracted from the server-data store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomInfo {
    pub id: RoomId,
    pub name: Option<String>,
    pub area: Option<String>,
    /// Direction to destination. `None` where the server named an exit but
    /// not where it goes — most do, which is why edges are also learned by
    /// walking them.
    pub exits: BTreeMap<String, Option<RoomId>>,
}

impl RoomInfo {
    /// Reads a room out of the merged server-data store, or `None` if the
    /// server has not identified one.
    ///
    /// The alias table is ordered **GMCP first**. §6.3's store-level
    /// precedence only settles keys that collide *exactly*, and these are
    /// different keys carrying the same fact (`Room.Info.num` vs
    /// `ROOM.VNUM`), so a server speaking both would otherwise leave the
    /// winner to `HashMap` iteration order. Ordering the lookup is where
    /// that precedence actually lives for room data.
    pub fn from_server_data(data: &HashMap<String, String>) -> Option<Self> {
        fn find_ci<'a>(data: &'a HashMap<String, String>, keys: &[&str]) -> Option<&'a str> {
            keys.iter().find_map(|key| {
                data.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(key))
                    .map(|(_, v)| v.as_str())
            })
        }

        // Zero is not a room. The DikuMUD family reserves the vnum, and
        // servers use it to say outright that a room has no stable
        // identity — a maze built to defeat mapping, where every room
        // reports the same nothing. Believing it collapses the whole maze
        // into one room and hangs the real rooms around it off that.
        let vnum = find_ci(data, &["Room.Info.num", "ROOM.VNUM", "ROOM_VNUM"])?;
        let id = RoomId(vnum.parse::<i64>().ok().filter(|num| *num != 0)?);

        let name = find_ci(data, &["Room.Info.name", "ROOM.NAME", "ROOM_NAME"]).map(str::to_string);
        let area = find_ci(
            data,
            &["Room.Info.area", "Room.Info.zone", "ROOM.AREA", "ROOM_AREA"],
        )
        .map(str::to_string);

        // Only one prefix's worth of exits is read: mixing two protocols'
        // exit lists would double-count exits that both happen to name.
        let exit_prefixes = [
            "Room.Info.exits.",
            "Room.Exits.",
            "ROOM.EXITS.",
            "ROOM_EXITS.",
        ];
        let mut exits = BTreeMap::new();
        for prefix in exit_prefixes {
            let matches: Vec<_> = data
                .iter()
                // `get`, not a slice: a key is server data (§13), and
                // byte-slicing one whose multibyte character straddles the
                // prefix's length panics. `get` returning `Some` also
                // proves the split is on a character boundary, which is
                // what makes the suffix slice below safe.
                .filter(|(k, _)| {
                    k.get(..prefix.len())
                        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
                })
                .collect();
            if matches.is_empty() {
                continue;
            }
            for (key, value) in matches {
                let suffix = &key[prefix.len()..];
                if let Some(direction) = canonical_direction(suffix) {
                    // key is the direction; value is the destination, if
                    // known. Zero means the same here as above — the exit
                    // is real, where it leads is not something to record.
                    let destination = value.parse::<i64>().ok().filter(|num| *num != 0);
                    exits.insert(direction.to_string(), destination.map(RoomId));
                } else if suffix.parse::<usize>().is_ok() {
                    // GMCP array form: key is the index, value is the direction.
                    if let Some(direction) = canonical_direction(value) {
                        exits.insert(direction.to_string(), None);
                    }
                }
            }
            break;
        }

        Some(RoomInfo {
            id,
            name,
            area,
            exits,
        })
    }
}

/// One room as the map knows it — the accumulation of every update and
/// every walked edge, not just the latest sighting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Room {
    pub id: RoomId,
    pub name: Option<String>,
    pub area: Option<String>,
    pub exits: BTreeMap<String, Option<RoomId>>,
}

/// Everywhere the client has been, and how those places connect.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "MapFile", into = "MapFile")]
pub struct Map {
    pub rooms: BTreeMap<RoomId, Room>,
}

/// The shape `Map` actually (de)serializes as. JSON object keys must be
/// strings, so the natural `BTreeMap<RoomId, Room>` cannot go straight to
/// disk as an object keyed by id — and a `Room` already carries its own
/// `id` field, so keying by it again out here would just be a second,
/// possibly-disagreeing copy of the same fact. A flat list sidesteps both.
#[derive(Serialize, Deserialize)]
struct MapFile {
    rooms: Vec<Room>,
}

impl From<Map> for MapFile {
    fn from(map: Map) -> Self {
        MapFile {
            rooms: map.rooms.into_values().collect(),
        }
    }
}

impl From<MapFile> for Map {
    fn from(file: MapFile) -> Self {
        Map {
            rooms: file.rooms.into_iter().map(|room| (room.id, room)).collect(),
        }
    }
}

/// The merge rule shared by `observe` (one freshly-parsed sighting) and
/// `merge` (a whole other map's room): a `None` never erases a fact this
/// room already holds, and a known exit destination is never overwritten by
/// an unknown one.
fn merge_room_facts(
    room: &mut Room,
    name: Option<&str>,
    area: Option<&str>,
    exits: &BTreeMap<String, Option<RoomId>>,
) {
    if let Some(name) = name {
        room.name = Some(name.to_string());
    }
    if let Some(area) = area {
        room.area = Some(area.to_string());
    }
    for (direction, dest) in exits {
        let existing = room.exits.entry(direction.clone()).or_insert(None);
        if dest.is_some() {
            *existing = *dest;
        }
    }
}

impl Map {
    /// Records a sighting. Merges rather than replaces: a later update that
    /// omits a field, or lists an exit without its destination, must not
    /// erase what walking already taught us.
    pub fn observe(&mut self, info: &RoomInfo) {
        let room = self.rooms.entry(info.id).or_insert_with(|| Room {
            id: info.id,
            name: None,
            area: None,
            exits: BTreeMap::new(),
        });
        merge_room_facts(
            room,
            info.name.as_deref(),
            info.area.as_deref(),
            &info.exits,
        );
    }

    /// Unions another map's rooms into this one, room by room, under the
    /// same never-erase rule `observe` uses — the case this exists for is
    /// two sessions on one profile exploring in parallel, where either
    /// side's facts are worth keeping and neither should be able to blank
    /// out the other's.
    pub fn merge(&mut self, other: Map) {
        for (id, other_room) in other.rooms {
            let room = self.rooms.entry(id).or_insert_with(|| Room {
                id,
                name: None,
                area: None,
                exits: BTreeMap::new(),
            });
            merge_room_facts(
                room,
                other_room.name.as_deref(),
                other_room.area.as_deref(),
                &other_room.exits,
            );
        }
    }

    /// Records an edge learned by walking it — the destination a server's
    /// own exit list usually leaves out.
    pub fn connect(&mut self, from: RoomId, direction: &str, to: RoomId) {
        let room = self.rooms.entry(from).or_insert_with(|| Room {
            id: from,
            name: None,
            area: None,
            exits: BTreeMap::new(),
        });
        room.exits.insert(direction.to_string(), Some(to));
    }

    /// The directions to walk from `from` to `to`, or `None` if no known
    /// route exists. BFS, not Dijkstra: every edge is one movement command,
    /// so all edges weigh the same.
    pub fn path(&self, from: RoomId, to: RoomId) -> Option<Vec<String>> {
        if from == to {
            return Some(Vec::new());
        }
        let mut visited = HashSet::new();
        visited.insert(from);
        let mut queue = VecDeque::new();
        queue.push_back((from, Vec::new()));
        while let Some((current, steps)) = queue.pop_front() {
            let Some(room) = self.rooms.get(&current) else {
                continue;
            };
            for (direction, dest) in &room.exits {
                let Some(next) = dest else { continue };
                let mut next_steps = steps.clone();
                next_steps.push(direction.clone());
                if *next == to {
                    return Some(next_steps);
                }
                if visited.insert(*next) {
                    queue.push_back((*next, next_steps));
                }
            }
        }
        None
    }

    /// Grid coordinates for drawing the area `origin` sits in.
    ///
    /// Scoped to one area on purpose: MUD geography is not Euclidean, and
    /// coordinates accumulated across a whole world diverge into nonsense.
    /// Collisions happen even inside an area — first placement wins, and a
    /// room that loses stays in the graph and stays pathable, it just is
    /// not drawn. `u`/`d` are not followed: they are marked on the room
    /// they leave (§16), not rendered as a third axis.
    pub fn layout_area(&self, origin: RoomId) -> HashMap<RoomId, (i32, i32)> {
        let mut coords = HashMap::new();
        let Some(origin_room) = self.rooms.get(&origin) else {
            return coords;
        };
        let area = &origin_room.area;

        let mut occupied = HashSet::new();
        coords.insert(origin, (0, 0));
        occupied.insert((0, 0));

        let mut queue = VecDeque::new();
        queue.push_back(origin);
        while let Some(current) = queue.pop_front() {
            let current_coord = coords[&current];
            let Some(room) = self.rooms.get(&current) else {
                continue;
            };
            for (direction, dest) in &room.exits {
                let Some(next) = dest else { continue };
                if coords.contains_key(next) {
                    continue;
                }
                let Some((dx, dy)) = direction_vector(direction) else {
                    continue;
                };
                let Some(next_room) = self.rooms.get(next) else {
                    continue;
                };
                if next_room.area != *area {
                    continue;
                }
                let candidate = (current_coord.0 + dx, current_coord.1 + dy);
                if !occupied.insert(candidate) {
                    continue;
                }
                coords.insert(*next, candidate);
                queue.push_back(*next);
            }
        }
        coords
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GMCP `Room.Info {"num":…}` as `gmcp::flatten` leaves it in the store.
    fn gmcp_ire() -> HashMap<String, String> {
        HashMap::from([
            ("Room.Info.num".to_string(), "12345".to_string()),
            ("Room.Info.name".to_string(), "Temple Square".to_string()),
            ("Room.Info.area".to_string(), "Midgaard".to_string()),
            ("Room.Info.exits.n".to_string(), "12346".to_string()),
            ("Room.Info.exits.e".to_string(), "12350".to_string()),
        ])
    }

    #[test]
    fn reads_an_ire_style_gmcp_room() {
        let info = RoomInfo::from_server_data(&gmcp_ire()).expect("a room");
        assert_eq!(info.id, RoomId(12345));
        assert_eq!(info.name.as_deref(), Some("Temple Square"));
        assert_eq!(info.area.as_deref(), Some("Midgaard"));
        assert_eq!(info.exits.get("n"), Some(&Some(RoomId(12346))));
        assert_eq!(info.exits.get("e"), Some(&Some(RoomId(12350))));
    }

    /// Aardwolf calls the area `zone` and spells directions out.
    #[test]
    fn reads_an_aardwolf_style_gmcp_room() {
        let data = HashMap::from([
            ("Room.Info.num".to_string(), "3001".to_string()),
            (
                "Room.Info.name".to_string(),
                "The Common Square".to_string(),
            ),
            ("Room.Info.zone".to_string(), "midgaard".to_string()),
            ("Room.Info.exits.north".to_string(), "3002".to_string()),
        ]);
        let info = RoomInfo::from_server_data(&data).expect("a room");
        assert_eq!(info.id, RoomId(3001));
        assert_eq!(info.area.as_deref(), Some("midgaard"));
        assert_eq!(
            info.exits.get("n"),
            Some(&Some(RoomId(3002))),
            "a spelled-out direction should canonicalise to its short form"
        );
    }

    /// MSDP's table form, as `msdp::flatten` leaves it.
    #[test]
    fn reads_an_msdp_table_room() {
        let data = HashMap::from([
            ("ROOM.VNUM".to_string(), "700".to_string()),
            ("ROOM.NAME".to_string(), "A Dark Alley".to_string()),
            ("ROOM.AREA".to_string(), "Slums".to_string()),
            ("ROOM.EXITS.w".to_string(), "701".to_string()),
        ]);
        let info = RoomInfo::from_server_data(&data).expect("a room");
        assert_eq!(info.id, RoomId(700));
        assert_eq!(info.name.as_deref(), Some("A Dark Alley"));
        assert_eq!(info.area.as_deref(), Some("Slums"));
        assert_eq!(info.exits.get("w"), Some(&Some(RoomId(701))));
    }

    /// MSDP's flat-variable form.
    #[test]
    fn reads_an_msdp_flat_room() {
        let data = HashMap::from([
            ("ROOM_VNUM".to_string(), "42".to_string()),
            ("ROOM_NAME".to_string(), "Nowhere".to_string()),
        ]);
        let info = RoomInfo::from_server_data(&data).expect("a room");
        assert_eq!(info.id, RoomId(42));
        assert_eq!(info.name.as_deref(), Some("Nowhere"));
        assert!(info.exits.is_empty());
    }

    /// Plenty of servers list which way you *can* go without saying where
    /// it lands — as a GMCP array, which flattens to numeric indices whose
    /// values are the direction names. The exit is real and must be
    /// recorded; only its destination is unknown.
    #[test]
    fn reads_exits_that_name_no_destination() {
        let data = HashMap::from([
            ("Room.Info.num".to_string(), "5".to_string()),
            ("Room.Exits.0".to_string(), "north".to_string()),
            ("Room.Exits.1".to_string(), "south".to_string()),
        ]);
        let info = RoomInfo::from_server_data(&data).expect("a room");
        assert_eq!(info.exits.get("n"), Some(&None));
        assert_eq!(info.exits.get("s"), Some(&None));
    }

    /// A server speaking both protocols has the same fact under two
    /// spellings. §6.3 prefers GMCP; different keys mean the store cannot
    /// enforce that, so the lookup order has to.
    #[test]
    fn gmcp_wins_when_both_protocols_name_a_room() {
        let mut data = gmcp_ire();
        data.insert("ROOM.VNUM".to_string(), "999".to_string());
        data.insert("ROOM.NAME".to_string(), "Stale MSDP Name".to_string());

        let info = RoomInfo::from_server_data(&data).expect("a room");
        assert_eq!(info.id, RoomId(12345), "GMCP's vnum should win");
        assert_eq!(info.name.as_deref(), Some("Temple Square"));
    }

    /// §13: server data is untrusted, and a GMCP object key is server data.
    /// Prefix matching that byte-slices on a length check alone panics when
    /// a multibyte character straddles the prefix's byte offset — here `é`
    /// spans bytes 15-16 of a 17-byte key, and `Room.Info.exits.` is 16.
    #[test]
    fn a_multibyte_key_does_not_panic_the_prefix_match() {
        let data = HashMap::from([
            ("Room.Info.num".to_string(), "1".to_string()),
            ("Room.Info.exitsé".to_string(), "nonsense".to_string()),
        ]);
        let info = RoomInfo::from_server_data(&data).expect("a room");
        assert!(
            info.exits.is_empty(),
            "a key that merely resembles the prefix is not an exit"
        );
    }

    #[test]
    fn no_room_data_at_all_is_none() {
        let data = HashMap::from([("Char.Vitals.hp".to_string(), "90".to_string())]);
        assert_eq!(RoomInfo::from_server_data(&data), None);
    }

    /// A vnum of zero is not a room. The DikuMUD family reserves it, and
    /// servers use it as an explicit "this room has no stable identity"
    /// signal for areas built to defeat mapping — identical names, one-way
    /// exits — where reporting them honestly would corrupt a map rather
    /// than fill it in. Taken at face value it collapses every such room
    /// into one and wires the real rooms around it to that fiction.
    #[test]
    fn a_zero_vnum_is_not_a_room() {
        let data = HashMap::from([
            ("ROOM.VNUM".to_string(), "0".to_string()),
            ("ROOM.NAME".to_string(), "A twisty passage".to_string()),
            ("ROOM.EXITS.n".to_string(), "0".to_string()),
        ]);
        assert_eq!(RoomInfo::from_server_data(&data), None);
    }

    /// Some MUDs withhold the vnum for maze rooms on purpose, to stop
    /// exactly this. A room with a name and exits but no number is
    /// therefore not half a room to be salvaged — it is the server
    /// declining to be mapped, and the answer is to leave it unmapped.
    /// Guessing an identity from the description would be inferring around
    /// a decision the MUD's author made deliberately, and §16 rules that
    /// out for the same reason it rules out text-inferred mapping.
    #[test]
    fn a_room_with_no_vnum_is_not_mapped() {
        let data = HashMap::from([
            ("Room.Info.name".to_string(), "A Twisty Passage".to_string()),
            ("Room.Info.exits.n".to_string(), "".to_string()),
            ("Room.Info.exits.s".to_string(), "".to_string()),
        ]);
        assert_eq!(RoomInfo::from_server_data(&data), None);
    }

    /// A later sighting that omits what an earlier one knew must not erase
    /// it — servers routinely send a sparser update than the first.
    #[test]
    fn observing_merges_rather_than_replaces() {
        let mut map = Map::default();
        map.observe(&RoomInfo::from_server_data(&gmcp_ire()).unwrap());
        map.connect(RoomId(12345), "n", RoomId(12346));

        let sparse = HashMap::from([("Room.Info.num".to_string(), "12345".to_string())]);
        map.observe(&RoomInfo::from_server_data(&sparse).unwrap());

        let room = map.rooms.get(&RoomId(12345)).expect("still there");
        assert_eq!(room.name.as_deref(), Some("Temple Square"));
        assert_eq!(
            room.exits.get("n"),
            Some(&Some(RoomId(12346))),
            "a walked edge must survive a sparser sighting"
        );
    }

    /// `merge` unions two maps room-by-room under the same never-erase rule
    /// as `observe`: a room known to both sides keeps every fact either side
    /// had, and a room known to only one side comes across whole.
    #[test]
    fn merge_unions_rooms_without_erasing_either_sides_facts() {
        let mut mine = Map::default();
        mine.observe(&RoomInfo::from_server_data(&gmcp_ire()).unwrap());
        mine.connect(RoomId(12345), "n", RoomId(12346));

        let mut theirs = Map::default();
        // Same room, sparser — must not blank out what `mine` already has.
        theirs.observe(&RoomInfo {
            id: RoomId(12345),
            name: None,
            area: None,
            exits: BTreeMap::new(),
        });
        theirs.connect(RoomId(12345), "e", RoomId(12350));
        // A room `mine` has never seen at all.
        theirs.observe(&RoomInfo {
            id: RoomId(999),
            name: Some("Elsewhere".to_string()),
            area: None,
            exits: BTreeMap::new(),
        });

        mine.merge(theirs);

        let room = mine.rooms.get(&RoomId(12345)).expect("still there");
        assert_eq!(
            room.name.as_deref(),
            Some("Temple Square"),
            "the incoming side's None must not erase the existing name"
        );
        assert_eq!(room.exits.get("n"), Some(&Some(RoomId(12346))));
        assert_eq!(
            room.exits.get("e"),
            Some(&Some(RoomId(12350))),
            "a new exit from the incoming side is added"
        );
        assert_eq!(
            mine.rooms.get(&RoomId(999)).map(|r| r.name.as_deref()),
            Some(Some("Elsewhere")),
            "a room known only to the other side comes across whole"
        );
    }

    /// The file shape is a flat list, not an id-keyed object — this proves
    /// the round trip actually goes through it and lands back on an
    /// equivalent map, ids included.
    #[test]
    fn map_round_trips_through_json() {
        let map = line_of_three();
        let json = serde_json::to_string(&map).unwrap();
        assert!(
            json.starts_with(r#"{"rooms":["#),
            "the file shape is a list under `rooms`, not an id-keyed object"
        );
        let restored: Map = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, map);
    }

    /// Three rooms in a row: the path is the directions, in order.
    fn line_of_three() -> Map {
        let mut map = Map::default();
        for (id, name) in [(1, "One"), (2, "Two"), (3, "Three")] {
            map.observe(&RoomInfo {
                id: RoomId(id),
                name: Some(name.to_string()),
                area: Some("Test".to_string()),
                exits: BTreeMap::new(),
            });
        }
        map.connect(RoomId(1), "n", RoomId(2));
        map.connect(RoomId(2), "e", RoomId(3));
        map
    }

    #[test]
    fn paths_across_known_edges() {
        let map = line_of_three();
        assert_eq!(
            map.path(RoomId(1), RoomId(3)),
            Some(vec!["n".to_string(), "e".to_string()])
        );
    }

    #[test]
    fn a_room_paths_to_itself_in_no_steps() {
        assert_eq!(line_of_three().path(RoomId(1), RoomId(1)), Some(Vec::new()));
    }

    #[test]
    fn no_path_between_disconnected_rooms() {
        let mut map = line_of_three();
        map.observe(&RoomInfo {
            id: RoomId(99),
            name: None,
            area: Some("Test".to_string()),
            exits: BTreeMap::new(),
        });
        assert_eq!(map.path(RoomId(1), RoomId(99)), None);
        assert_eq!(map.path(RoomId(1), RoomId(1234)), None, "unknown room");
    }

    /// Edges are one-way as learned: walking north does not prove south
    /// comes back, and plenty of MUDs have exits that don't.
    #[test]
    fn edges_are_directional() {
        let map = line_of_three();
        assert_eq!(map.path(RoomId(3), RoomId(1)), None);
    }

    #[test]
    fn lays_out_an_area_on_a_grid() {
        let map = line_of_three();
        let coords = map.layout_area(RoomId(1));
        assert_eq!(coords.get(&RoomId(1)), Some(&(0, 0)));
        assert_eq!(coords.get(&RoomId(2)), Some(&(0, -1)), "north is up");
        assert_eq!(coords.get(&RoomId(3)), Some(&(1, -1)), "then east");
    }

    /// MUD geography folds back on itself: n, n, then s from the second
    /// room lands a *third* room on the first's coordinate. The loser keeps
    /// its place in the graph and stays pathable; it just is not drawn.
    #[test]
    fn a_coordinate_collision_drops_only_the_drawing() {
        let mut map = Map::default();
        for id in 1..=3 {
            map.observe(&RoomInfo {
                id: RoomId(id),
                name: None,
                area: Some("Test".to_string()),
                exits: BTreeMap::new(),
            });
        }
        map.connect(RoomId(1), "n", RoomId(2));
        map.connect(RoomId(2), "s", RoomId(3));

        let coords = map.layout_area(RoomId(1));
        assert_eq!(coords.get(&RoomId(1)), Some(&(0, 0)));
        assert_eq!(coords.get(&RoomId(2)), Some(&(0, -1)));
        assert_eq!(coords.get(&RoomId(3)), None, "collided, so not drawn");
        assert_eq!(
            map.path(RoomId(1), RoomId(3)),
            Some(vec!["n".to_string(), "s".to_string()]),
            "but still reachable"
        );
    }

    /// Coordinates are per-area: a neighbouring area's rooms would carry
    /// their own origin and overlap this one's if they were mixed in.
    #[test]
    fn layout_stops_at_the_area_boundary() {
        let mut map = line_of_three();
        map.observe(&RoomInfo {
            id: RoomId(4),
            name: None,
            area: Some("Elsewhere".to_string()),
            exits: BTreeMap::new(),
        });
        map.connect(RoomId(3), "e", RoomId(4));

        let coords = map.layout_area(RoomId(1));
        assert_eq!(coords.get(&RoomId(4)), None);
    }

    /// `u`/`d` are real exits — pathable, and never a grid displacement.
    #[test]
    fn vertical_exits_path_but_do_not_displace() {
        let mut map = Map::default();
        for id in 1..=2 {
            map.observe(&RoomInfo {
                id: RoomId(id),
                name: None,
                area: Some("Test".to_string()),
                exits: BTreeMap::new(),
            });
        }
        map.connect(RoomId(1), "u", RoomId(2));

        assert_eq!(
            map.path(RoomId(1), RoomId(2)),
            Some(vec!["u".to_string()]),
            "up is walkable"
        );
        let coords = map.layout_area(RoomId(1));
        assert_eq!(coords.get(&RoomId(2)), None, "but not placed on the grid");
    }

    #[test]
    fn canonicalises_direction_spellings() {
        assert_eq!(canonical_direction("North"), Some("n"));
        assert_eq!(canonical_direction(" ne "), Some("ne"));
        assert_eq!(canonical_direction("southwest"), Some("sw"));
        assert_eq!(canonical_direction("look"), None);
    }
}
