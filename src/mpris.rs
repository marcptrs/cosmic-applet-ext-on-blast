// SPDX-License-Identifier: GPL-3.0

use std::cell::RefCell;
use std::time::Duration;

use dbus::blocking::Connection;
use mpris::{PlaybackStatus, Player, PlayerFinder};

/// Proxy meta-player mirroring whichever player is active. Its metadata is
/// briefly empty between hand-offs and blanks the panel, so it's always skipped.
const PLAYERCTLD: &str = "playerctld";

/// D-Bus well-known name prefix every MPRIS player advertises.
const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";

/// Timeout for the cheap `ListNames` bus enumeration each poll.
const LIST_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq)]
pub struct PlayerInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub year: Option<u32>,
    pub status: PlaybackStatus,
    pub art_url: Option<String>,
    pub bus_name: String,
    pub position_us: u64,
    pub length_us: u64,
    pub can_go_next: bool,
    pub can_go_previous: bool,
    pub can_pause: bool,
    pub can_play: bool,
    pub can_seek: bool,
}

impl Default for PlayerInfo {
    fn default() -> Self {
        Self {
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            year: None,
            status: PlaybackStatus::Stopped,
            art_url: None,
            bus_name: String::new(),
            position_us: 0,
            length_us: 0,
            can_go_next: false,
            can_go_previous: false,
            can_pause: false,
            can_play: false,
            can_seek: false,
        }
    }
}

/// Enough to label and identify a player in the picker, without its full state.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerSummary {
    pub bus_name: String,
    pub identity: String,
    pub status: PlaybackStatus,
    pub title: String,
}

/// One tick: the player to display (pinned, else auto-picked) and all of them.
#[derive(Debug, Clone, Default)]
pub struct Poll {
    pub player: Option<PlayerInfo>,
    pub players: Vec<PlayerSummary>,
}

/// Transport capabilities; cached per-player since they change rarely and each
/// field is its own D-Bus round trip.
#[derive(Debug, Clone, Copy, Default)]
struct Caps {
    next: bool,
    prev: bool,
    pause: bool,
    play: bool,
    seek: bool,
}

/// Per-thread MPRIS state. A fresh D-Bus connection per poll was never reclaimed
/// and grew the host panel by GBs, so connections and players are long-lived.
struct Cache {
    finder: PlayerFinder,
    conn: Connection,
    players: Vec<Player>,
    bus_set: Vec<String>,
    caps: Option<(String, Caps)>,
}

thread_local! {
    static CACHE: RefCell<Option<Cache>> = const { RefCell::new(None) };
}

/// `selected` pins a player by trimmed bus name, else the most likely active one
/// wins. `popup_open` gates the extras; `player` is `None` when none resolved.
pub fn poll(selected: Option<&str>, popup_open: bool) -> Poll {
    CACHE.with(|cell| {
        let mut slot = cell.borrow_mut();
        match poll_cached(&mut slot, selected, popup_open) {
            Some(poll) => poll,
            None => {
                // Possibly a stale connection: reconnect on the next poll.
                *slot = None;
                Poll::default()
            }
        }
    })
}

fn poll_cached(
    slot: &mut Option<Cache>,
    selected: Option<&str>,
    popup_open: bool,
) -> Option<Poll> {
    let cache = ensure_cache(slot)?;

    // Cheap enumeration: only rebuild the (expensive) `Player` objects when the
    // set of live MPRIS buses actually changed.
    let current = mpris_bus_set(&cache.conn)?;
    if current != cache.bus_set {
        let all = cache.finder.find_all().ok()?;
        cache.players = all
            .into_iter()
            .filter(|p| p.bus_name_trimmed() != PLAYERCTLD)
            .collect();
        cache.bus_set = sorted_trimmed(&cache.players);
    }

    let chosen_idx = selected
        .and_then(|sel| cache.players.iter().position(|p| p.bus_name_trimmed() == sel));

    // Status only matters for auto-picking and titles only for the picker, so a
    // pinned player with the popup closed needs neither read.
    let need_summaries = popup_open || chosen_idx.is_none();
    let players: Vec<PlayerSummary> = cache
        .players
        .iter()
        .map(|p| PlayerSummary {
            bus_name: p.bus_name_trimmed().to_string(),
            identity: p.identity().to_string(),
            status: if need_summaries {
                p.get_playback_status().unwrap_or(PlaybackStatus::Stopped)
            } else {
                PlaybackStatus::Stopped
            },
            title: if popup_open {
                p.get_metadata()
                    .ok()
                    .and_then(|m| m.title().map(str::to_string))
                    .unwrap_or_default()
            } else {
                String::new()
            },
        })
        .collect();

    let chosen = chosen_idx.or_else(|| pick_active_index(&players));

    let player = chosen.map(|i| {
        // Reuse the status we already read in the summary pass when we have it.
        let status = if need_summaries {
            Some(players[i].status)
        } else {
            None
        };
        player_info(&cache.players[i], status, popup_open, &mut cache.caps)
    });

    Some(Poll { player, players })
}

/// Borrow the per-thread cache, creating the D-Bus connections on first use.
fn ensure_cache(slot: &mut Option<Cache>) -> Option<&mut Cache> {
    if slot.is_none() {
        let finder = PlayerFinder::new().ok()?;
        let conn = Connection::new_session().ok()?;
        *slot = Some(Cache {
            finder,
            conn,
            players: Vec::new(),
            bus_set: Vec::new(),
            caps: None,
        });
    }
    slot.as_mut()
}

/// Enumerate live MPRIS buses (trimmed, sorted, `playerctld` excluded) with a
/// single `ListNames` call — far cheaper than constructing a `Player` per bus.
fn mpris_bus_set(conn: &Connection) -> Option<Vec<String>> {
    let proxy = conn.with_proxy(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        LIST_TIMEOUT,
    );
    let (names,): (Vec<String>,) = proxy
        .method_call("org.freedesktop.DBus", "ListNames", ())
        .ok()?;
    let mut set: Vec<String> = names
        .into_iter()
        .filter_map(|n| n.strip_prefix(MPRIS_PREFIX).map(str::to_string))
        .filter(|n| n != PLAYERCTLD)
        .collect();
    set.sort();
    Some(set)
}

/// Sorted trimmed bus names, to compare against a fresh enumeration.
fn sorted_trimmed(players: &[Player]) -> Vec<String> {
    let mut set: Vec<String> =
        players.iter().map(|p| p.bus_name_trimmed().to_string()).collect();
    set.sort();
    set
}

/// Pick the index of the most likely active player from the summaries,
/// mirroring the MPRIS convention: Playing > Paused > has-a-track > first.
fn pick_active_index(players: &[PlayerSummary]) -> Option<usize> {
    let mut first_paused = None;
    let mut first_with_track = None;

    for (i, s) in players.iter().enumerate() {
        match s.status {
            PlaybackStatus::Playing => return Some(i),
            PlaybackStatus::Paused if first_paused.is_none() => first_paused = Some(i),
            _ if first_with_track.is_none() && !s.title.is_empty() => {
                first_with_track = Some(i)
            }
            _ => {}
        }
    }

    first_paused
        .or(first_with_track)
        .or_else(|| (!players.is_empty()).then_some(0))
}

/// `status` reuses a value already read this poll when available; `popup_open`
/// gates position and capabilities, which only the popup shows.
fn player_info(
    player: &Player,
    status: Option<PlaybackStatus>,
    popup_open: bool,
    caps_memo: &mut Option<(String, Caps)>,
) -> PlayerInfo {
    let metadata = player.get_metadata().unwrap_or_default();
    let status = status.unwrap_or_else(|| {
        player.get_playback_status().unwrap_or(PlaybackStatus::Stopped)
    });

    let title = metadata.title().unwrap_or("Unknown").to_string();
    let artist = metadata.artists().map(|a| a.join(", ")).unwrap_or_default();
    let album = metadata.album_name().unwrap_or("").to_string();

    let year = metadata
        .get("xesam:year")
        .and_then(|v| v.as_i32().map(|n| n as u32).or_else(|| v.as_u32()))
        .or_else(|| {
            metadata
                .get("xesam:contentCreated")
                .and_then(|v| v.as_str())
                .and_then(|s| s.get(..4))
                .and_then(|s| s.parse::<u32>().ok())
        });

    let art_url = metadata.art_url().map(|u| u.to_string());
    let bus_name = player.bus_name_trimmed().to_string();
    let length_us = metadata.length_in_microseconds().unwrap_or(0);

    let position_us = if popup_open {
        player.get_position_in_microseconds().unwrap_or(0)
    } else {
        0
    };

    let caps = if popup_open {
        caps_for(player, &bus_name, caps_memo)
    } else {
        Caps::default()
    };

    PlayerInfo {
        title,
        artist,
        album,
        year,
        status,
        art_url,
        bus_name,
        position_us,
        length_us,
        can_go_next: caps.next,
        can_go_previous: caps.prev,
        can_pause: caps.pause,
        can_play: caps.play,
        can_seek: caps.seek,
    }
}

/// Return the player's capabilities, reusing the memo when the chosen player is
/// unchanged (each field is a separate D-Bus round trip).
fn caps_for(player: &Player, bus_name: &str, memo: &mut Option<(String, Caps)>) -> Caps {
    if let Some((bus, caps)) = memo {
        if bus == bus_name {
            return *caps;
        }
    }
    let caps = Caps {
        next: player.can_go_next().unwrap_or(false),
        prev: player.can_go_previous().unwrap_or(false),
        pause: player.can_pause().unwrap_or(false),
        play: player.can_play().unwrap_or(false),
        seek: player.can_seek().unwrap_or(false),
    };
    *memo = Some((bus_name.to_string(), caps));
    caps
}

pub fn seek_to(bus_name: &str, position_us: u64, current_position_us: u64) {
    with_player(bus_name, |p| {
        let delta = position_us as i64 - current_position_us as i64;
        let _ = p.seek(delta);
    });
}

pub fn play_pause(bus_name: &str) {
    with_player(bus_name, |p| {
        let _ = p.play_pause();
    });
}

pub fn next(bus_name: &str) {
    with_player(bus_name, |p| {
        let _ = p.next();
    });
}

pub fn previous(bus_name: &str) {
    with_player(bus_name, |p| {
        let _ = p.previous();
    });
}

/// Run `f` against the cached `Player`, refreshing the cache if it isn't known.
/// Reuses polling's connection rather than opening one per control action.
fn with_player<F: FnOnce(&Player)>(bus_name: &str, f: F) {
    CACHE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(cache) = ensure_cache(&mut slot) else {
            return;
        };
        if !cache.players.iter().any(|p| p.bus_name_trimmed() == bus_name) {
            if let Ok(all) = cache.finder.find_all() {
                cache.players = all
                    .into_iter()
                    .filter(|p| p.bus_name_trimmed() != PLAYERCTLD)
                    .collect();
                cache.bus_set = sorted_trimmed(&cache.players);
            }
        }
        if let Some(p) = cache.players.iter().find(|p| p.bus_name_trimmed() == bus_name) {
            f(p);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(status: PlaybackStatus, title: &str) -> PlayerSummary {
        PlayerSummary {
            bus_name: title.to_string(),
            identity: title.to_string(),
            status,
            title: title.to_string(),
        }
    }

    #[test]
    fn pick_prefers_playing() {
        let players = vec![
            summary(PlaybackStatus::Paused, "a"),
            summary(PlaybackStatus::Playing, "b"),
            summary(PlaybackStatus::Stopped, "c"),
        ];
        assert_eq!(pick_active_index(&players), Some(1));
    }

    #[test]
    fn pick_falls_back_to_paused_then_track_then_first() {
        assert_eq!(
            pick_active_index(&[
                summary(PlaybackStatus::Stopped, ""),
                summary(PlaybackStatus::Paused, "b"),
            ]),
            Some(1)
        );
        assert_eq!(
            pick_active_index(&[
                summary(PlaybackStatus::Stopped, ""),
                summary(PlaybackStatus::Stopped, "has-track"),
            ]),
            Some(1)
        );
        assert_eq!(
            pick_active_index(&[
                summary(PlaybackStatus::Stopped, ""),
                summary(PlaybackStatus::Stopped, ""),
            ]),
            Some(0)
        );
    }

    #[test]
    fn pick_empty_is_none() {
        assert_eq!(pick_active_index(&[]), None);
    }
}
