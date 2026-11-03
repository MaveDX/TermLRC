use crate::matcher::TrackQuery;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use zbus::blocking::{fdo::DBusProxy, Connection, Proxy};
use zbus::zvariant::{OwnedValue, Value};

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerState {
    Playing,
    Paused,
    Stopped,
    Disconnected,
}

#[derive(Debug, Clone)]
pub struct TrackSnapshot {
    pub state: PlayerState,
    pub query: TrackQuery,
    pub length: Option<Duration>,
    pub position: Duration,
}

impl Default for TrackSnapshot {
    fn default() -> Self {
        Self {
            state: PlayerState::Disconnected,
            query: TrackQuery::default(),
            length: None,
            position: Duration::ZERO,
        }
    }
}

#[derive(Debug, Clone)]
struct DiscoveredPlayer {
    bus_name: String,
    state: PlayerState,
}

/// Read-only MPRIS D-Bus detector for identifying the currently playing song and position.
pub struct MprisTracker {
    conn: Option<Connection>,
    preferred_player: Option<String>,
    last_poll: Instant,
    poll_interval: Duration,
    snapshot: TrackSnapshot,
    anchor_pos: Duration,
    anchor_time: Instant,
    last_raw_dbus_pos: Option<Duration>,
    last_track_key: String,
}

impl MprisTracker {
    pub fn new(preferred_player: Option<String>, poll_interval_ms: u64) -> Self {
        let now = Instant::now();
        let conn = Connection::session().ok();
        let mut tracker = Self {
            conn,
            preferred_player,
            last_poll: now.checked_sub(Duration::from_secs(10)).unwrap_or(now),
            poll_interval: Duration::from_millis(poll_interval_ms.clamp(50, 2000)),
            snapshot: TrackSnapshot::default(),
            anchor_pos: Duration::ZERO,
            anchor_time: now,
            last_raw_dbus_pos: None,
            last_track_key: String::new(),
        };
        tracker.poll_now();
        tracker
    }

    pub fn update_config(&mut self, preferred_player: Option<String>, poll_interval_ms: u64) {
        self.preferred_player = preferred_player;
        self.poll_interval = Duration::from_millis(poll_interval_ms.clamp(50, 2000));
    }

    /// Poll MPRIS if `poll_interval` has elapsed, and return an updated snapshot
    /// with smoothly interpolated playback position.
    pub fn update(&mut self) -> &TrackSnapshot {
        if self.last_poll.elapsed() >= self.poll_interval {
            self.poll_now();
        }
        self.snapshot.position = self.current_position();
        &self.snapshot
    }

    /// Compute the smoothly interpolated playback position at this exact instant.
    pub fn current_position(&self) -> Duration {
        let pos = match self.snapshot.state {
            PlayerState::Playing => self.anchor_pos + self.anchor_time.elapsed(),
            PlayerState::Paused => self.anchor_pos,
            PlayerState::Stopped | PlayerState::Disconnected => Duration::ZERO,
        };
        if let Some(len) = self.snapshot.length {
            if !len.is_zero() && pos > len {
                return len;
            }
        }
        pos
    }

    /// Force an immediate D-Bus MPRIS poll.
    pub fn poll_now(&mut self) {
        self.last_poll = Instant::now();
        if self.conn.is_none() {
            self.conn = Connection::session().ok();
        }

        let Some(target) = self.find_target_player() else {
            self.snapshot = TrackSnapshot::default();
            self.anchor_pos = Duration::ZERO;
            self.anchor_time = Instant::now();
            self.last_raw_dbus_pos = None;
            self.last_track_key.clear();
            return;
        };

        let Some((query, length, pos_micros_res)) =
            self.player_proxy(&target.bus_name).map(|proxy| {
                let (q, l) = read_metadata(&proxy);
                let pos = proxy.get_property::<i64>("Position");
                (q, l, pos)
            })
        else {
            return;
        };

        let state = target.state;
        let track_key = format!(
            "{}|{}|{}",
            target.bus_name,
            query.joined_artists(),
            query.title
        );
        let track_changed = track_key != self.last_track_key;
        if track_changed {
            self.last_track_key = track_key;
            self.anchor_pos = Duration::ZERO;
            self.anchor_time = Instant::now();
            self.last_raw_dbus_pos = None;
        }

        let now = Instant::now();
        if let Ok(pos_micros) = pos_micros_res {
            let dbus_pos = Duration::from_micros(pos_micros.max(0) as u64);
            let predicted = self.current_position();
            let raw_changed = self.last_raw_dbus_pos != Some(dbus_pos);
            let drift_secs = (predicted.as_secs_f64() - dbus_pos.as_secs_f64()).abs();

            if track_changed
                || state != PlayerState::Playing
                || raw_changed
                || drift_secs > 1.25
            {
                self.anchor_pos = dbus_pos;
                self.anchor_time = now;
                self.last_raw_dbus_pos = Some(dbus_pos);
            }
        } else if state != self.snapshot.state {
            self.anchor_pos = self.current_position();
            self.anchor_time = now;
        }

        self.snapshot = TrackSnapshot {
            state,
            query,
            length,
            position: self.current_position(),
        };
    }

    fn player_proxy<'a>(&'a self, bus_name: &'a str) -> Option<Proxy<'a>> {
        let conn = self.conn.as_ref()?;
        Proxy::new(conn, bus_name, MPRIS_PATH, PLAYER_IFACE).ok()
    }

    fn find_target_player(&self) -> Option<DiscoveredPlayer> {
        let conn = self.conn.as_ref()?;
        let dbus = DBusProxy::new(conn).ok()?;
        let names = dbus.list_names().ok()?;

        let mut players: Vec<DiscoveredPlayer> = Vec::new();
        for name in names {
            let bus_str = name.as_str();
            if !bus_str.starts_with(MPRIS_PREFIX) {
                continue;
            }

            let state = Proxy::new(conn, bus_str, MPRIS_PATH, PLAYER_IFACE)
                .ok()
                .and_then(|p| p.get_property::<String>("PlaybackStatus").ok())
                .map(|status| match status.as_str() {
                    "Playing" => PlayerState::Playing,
                    "Paused" => PlayerState::Paused,
                    "Stopped" => PlayerState::Stopped,
                    _ => PlayerState::Paused,
                })
                .unwrap_or(PlayerState::Stopped);

            players.push(DiscoveredPlayer {
                bus_name: bus_str.to_string(),
                state,
            });
        }

        if players.is_empty() {
            return None;
        }

        if let Some(ref pref) = self.preferred_player {
            let pref_lower = pref.to_ascii_lowercase();
            let mut matching: Vec<DiscoveredPlayer> = players
                .into_iter()
                .filter(|p| p.bus_name.to_ascii_lowercase().contains(&pref_lower))
                .collect();
            if !matching.is_empty() {
                matching.sort_by_key(|p| state_rank(p.state));
                return matching.into_iter().next();
            }
            return None;
        }

        players.sort_by_key(|p| state_rank(p.state));
        players.into_iter().next()
    }
}

fn state_rank(state: PlayerState) -> u8 {
    match state {
        PlayerState::Playing => 0,
        PlayerState::Paused => 1,
        PlayerState::Stopped => 2,
        PlayerState::Disconnected => 3,
    }
}

fn read_metadata(proxy: &Proxy<'_>) -> (TrackQuery, Option<Duration>) {
    let Ok(map) = proxy.get_property::<HashMap<String, OwnedValue>>("Metadata") else {
        return (TrackQuery::default(), None);
    };

    let title = map
        .get("xesam:title")
        .and_then(extract_string)
        .unwrap_or_default()
        .trim()
        .to_string();

    let artists = map
        .get("xesam:artist")
        .map(extract_string_list)
        .unwrap_or_default();

    let album = map
        .get("xesam:album")
        .and_then(extract_string)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let xesam_url = map
        .get("xesam:url")
        .and_then(extract_string)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let track_id = map
        .get("mpris:trackid")
        .and_then(extract_string)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let url = xesam_url.or(track_id);

    let length = map.get("mpris:length").and_then(extract_duration_micros);

    (
        TrackQuery {
            title,
            artists,
            album,
            url,
        },
        length,
    )
}

fn extract_string(val: &OwnedValue) -> Option<String> {
    if let Ok(s) = <&str>::try_from(val) {
        return Some(s.to_string());
    }
    match &**val {
        Value::Str(s) => Some(s.to_string()),
        Value::ObjectPath(p) => Some(p.as_str().to_string()),
        Value::Value(inner) => match &**inner {
            Value::Str(s) => Some(s.to_string()),
            Value::ObjectPath(p) => Some(p.as_str().to_string()),
            _ => None,
        },
        _ => None,
    }
}

fn extract_string_list(val: &OwnedValue) -> Vec<String> {
    if let Ok(vec) = Vec::<String>::try_from(val.clone()) {
        return vec
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    if let Some(single) = extract_string(val) {
        if !single.trim().is_empty() {
            return vec![single.trim().to_string()];
        }
    }
    if let Value::Array(arr) = &**val {
        let mut out = Vec::new();
        for item in arr.iter() {
            if let Value::Str(s) = item {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
            }
        }
        return out;
    }
    Vec::new()
}

fn extract_duration_micros(val: &OwnedValue) -> Option<Duration> {
    if let Ok(v) = i64::try_from(val) {
        if v > 0 {
            return Some(Duration::from_micros(v as u64));
        }
    }
    if let Ok(v) = u64::try_from(val) {
        if v > 0 {
            return Some(Duration::from_micros(v));
        }
    }
    match &**val {
        Value::I64(v) if *v > 0 => Some(Duration::from_micros(*v as u64)),
        Value::U64(v) if *v > 0 => Some(Duration::from_micros(*v)),
        Value::I32(v) if *v > 0 => Some(Duration::from_micros(*v as u64)),
        Value::U32(v) if *v > 0 => Some(Duration::from_micros(*v as u64)),
        _ => None,
    }
}
