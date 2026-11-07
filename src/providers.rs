use crate::config::{data_cache_file_path, FallbackProviderKind, Settings};
use crate::lrc::{LrcMetadata, LyricLine, ParsedLrc, TimedSegment};
use crate::matcher::{clean_video_metadata, normalize, CleanedTrackMetadata, TrackQuery};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

const MAX_LYRIC_CACHE_ENTRIES: usize = 128;
const MAX_CANDIDATE_LYRIC_FETCHES: usize = 3;
/// Recordings up to 30 seconds longer or shorter than the playing track are still matched.
pub const MAX_DURATION_DIFF_SECS: f64 = 30.0;
/// Tolerance (in seconds) for considering a recording's length to match the playing track.
pub const LENGTH_MATCH_TOLERANCE_SECS: f64 = 3.5;
/// Line timestamp agreement tolerance (in seconds) when comparing synced lyrics across sources.
const LINE_CONSENSUS_TOLERANCE_SECS: f64 = 1.8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ServiceEndpoint {
    Provider(FallbackProviderKind),
    Deezer,
    MusicBrainz,
}

/// Live status of the lyrics lookup / download pipeline.
#[derive(Debug, Clone, PartialEq)]
pub enum LookupStatus {
    Idle,
    Searching,
    Downloading { progress: f32 },
    RateLimited,
    NotFound,
}

#[derive(Debug, Clone, Default)]
struct RateLimitState {
    last_request: Option<Instant>,
    cooldown_until: Option<Instant>,
    is_rate_limit_429: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct DiskCacheFile {
    entries: Vec<(String, ParsedLrc)>,
}

pub struct ProviderCaches {
    rate_limits: HashMap<ServiceEndpoint, RateLimitState>,
    lyric_cache_order: VecDeque<String>,
    lyric_cache: HashMap<String, ParsedLrc>,
    disk_cache_path: Option<PathBuf>,
}

impl Default for ProviderCaches {
    fn default() -> Self {
        let mut caches = Self {
            rate_limits: HashMap::new(),
            lyric_cache_order: VecDeque::new(),
            lyric_cache: HashMap::new(),
            disk_cache_path: data_cache_file_path(),
        };
        caches.load_from_disk();
        caches
    }
}

impl ProviderCaches {
    fn min_interval(endpoint: ServiceEndpoint) -> Duration {
        match endpoint {
            ServiceEndpoint::Provider(FallbackProviderKind::LrcMux) => Duration::from_millis(300),
            ServiceEndpoint::Provider(FallbackProviderKind::LyricsPlus) => Duration::from_millis(350),
            ServiceEndpoint::Provider(FallbackProviderKind::LrcLib) => Duration::from_millis(350),
            ServiceEndpoint::Provider(FallbackProviderKind::KuGou) => Duration::from_millis(450),
            ServiceEndpoint::Provider(FallbackProviderKind::NetEase) => Duration::from_millis(450),
            ServiceEndpoint::Deezer => Duration::from_millis(300),
            ServiceEndpoint::MusicBrainz => Duration::from_millis(1050),
        }
    }

    fn is_on_cooldown(&self, endpoint: ServiceEndpoint) -> bool {
        self.rate_limits
            .get(&endpoint)
            .and_then(|s| s.cooldown_until)
            .map(|until| Instant::now() < until)
            .unwrap_or(false)
    }

    fn is_rate_limited_429(&self, endpoint: ServiceEndpoint) -> bool {
        self.rate_limits
            .get(&endpoint)
            .filter(|s| s.is_rate_limit_429)
            .and_then(|s| s.cooldown_until)
            .map(|until| Instant::now() < until)
            .unwrap_or(false)
    }

    fn set_cooldown(&mut self, endpoint: ServiceEndpoint, duration: Duration, is_429: bool) {
        let state = self.rate_limits.entry(endpoint).or_default();
        state.cooldown_until = Some(Instant::now() + duration);
        state.is_rate_limit_429 = is_429;
    }

    fn wait_turn(&mut self, endpoint: ServiceEndpoint) {
        let min_gap = Self::min_interval(endpoint);
        let state = self.rate_limits.entry(endpoint).or_default();
        if let Some(last) = state.last_request {
            let elapsed = last.elapsed();
            if elapsed < min_gap {
                thread::sleep(min_gap - elapsed);
            }
        }
        state.last_request = Some(Instant::now());
    }

    fn get_cached_lyrics(&self, key: &str) -> Option<ParsedLrc> {
        self.lyric_cache.get(key).cloned()
    }

    fn insert_cached_lyrics(&mut self, key: String, lrc: ParsedLrc) {
        if !self.lyric_cache.contains_key(&key) {
            if self.lyric_cache_order.len() >= MAX_LYRIC_CACHE_ENTRIES {
                if let Some(oldest) = self.lyric_cache_order.pop_front() {
                    self.lyric_cache.remove(&oldest);
                }
            }
            self.lyric_cache_order.push_back(key.clone());
        }
        self.lyric_cache.insert(key, lrc);
        self.save_to_disk();
    }

    fn load_from_disk(&mut self) {
        let Some(ref path) = self.disk_cache_path else {
            return;
        };
        let Ok(bytes) = fs::read(path) else {
            return;
        };
        let Ok(parsed) = serde_json::from_slice::<DiskCacheFile>(&bytes) else {
            return;
        };
        for (k, v) in parsed.entries.into_iter().take(MAX_LYRIC_CACHE_ENTRIES) {
            if !self.lyric_cache.contains_key(&k) {
                self.lyric_cache_order.push_back(k.clone());
                self.lyric_cache.insert(k, v);
            }
        }
    }

    fn save_to_disk(&self) {
        let Some(ref path) = self.disk_cache_path else {
            return;
        };
        let entries: Vec<(String, ParsedLrc)> = self
            .lyric_cache_order
            .iter()
            .filter_map(|k| self.lyric_cache.get(k).map(|v| (k.clone(), v.clone())))
            .collect();
        let payload = DiskCacheFile { entries };
        if let Ok(json) = serde_json::to_vec(&payload) {
            let tmp = path.with_extension("json.tmp");
            if fs::write(&tmp, json).is_ok() {
                let _ = fs::rename(&tmp, path);
            }
        }
    }
}

/// Build a stable cache key from the cleaned track identity so small duration or album updates
/// hit the same entry without invalidating found lyrics.
pub fn build_query_cache_key(query: &TrackQuery, settings: &Settings) -> String {
    let cleaned = clean_video_metadata(query);
    format!(
        "{}|{}|{}",
        normalize(&cleaned.core_artist),
        normalize(&cleaned.core_title),
        settings.prefer_uncensored
    )
}

fn endpoint_http_get_string(
    agent: &ureq::Agent,
    endpoint: ServiceEndpoint,
    url: &str,
    headers: &[(&str, &str)],
    caches: &mut ProviderCaches,
) -> Option<String> {
    if caches.is_on_cooldown(endpoint) {
        return None;
    }
    caches.wait_turn(endpoint);

    let mut req = agent.get(url);
    for &(k, v) in headers {
        req = req.set(k, v);
    }

    match req.call() {
        Ok(resp) => resp.into_string().ok(),
        Err(ureq::Error::Status(code, resp)) => {
            if code == 429 || code == 503 {
                let default_wait = if code == 429 { 60 } else { 30 };
                let retry_secs = resp
                    .header("Retry-After")
                    .and_then(|h| h.trim().parse::<u64>().ok())
                    .unwrap_or(default_wait)
                    .clamp(5, 600);
                caches.set_cooldown(endpoint, Duration::from_secs(retry_secs), code == 429);
            } else if code >= 500 || code == 403 || code == 401 {
                caches.set_cooldown(endpoint, Duration::from_secs(30), false);
            }
            None
        }
        Err(ureq::Error::Transport(_)) => {
            // Provider failed (DNS/connection/timeout); skip it for a while
            caches.set_cooldown(endpoint, Duration::from_secs(30), false);
            None
        }
    }
}

fn endpoint_http_get_json(
    agent: &ureq::Agent,
    endpoint: ServiceEndpoint,
    url: &str,
    headers: &[(&str, &str)],
    caches: &mut ProviderCaches,
) -> Option<Value> {
    let body = endpoint_http_get_string(agent, endpoint, url, headers, caches)?;
    serde_json::from_str(&body).ok()
}

pub struct FallbackRequest {
    pub query: TrackQuery,
    pub track_length: Option<Duration>,
    pub settings: Settings,
}

pub enum FallbackEvent {
    Status {
        query: TrackQuery,
        status: LookupStatus,
    },
    Completed {
        query: TrackQuery,
        lrc: Option<ParsedLrc>,
        rate_limited: bool,
    },
}

/// Asynchronous background worker that queries remote fallback providers in priority order
/// without blocking the TUI rendering loop.
pub struct FallbackWorker {
    tx: Sender<FallbackRequest>,
    rx: Receiver<FallbackEvent>,
}

impl FallbackWorker {
    pub fn new() -> Self {
        let (req_tx, req_rx) = mpsc::channel::<FallbackRequest>();
        let (res_tx, res_rx) = mpsc::channel::<FallbackEvent>();

        thread::spawn(move || {
            let mut caches = ProviderCaches::default();
            while let Ok(mut req) = req_rx.recv() {
                // Drain any queued requests so we only fetch for the latest active song
                while let Ok(newer) = req_rx.try_recv() {
                    req = newer;
                }

                let _ = res_tx.send(FallbackEvent::Status {
                    query: req.query.clone(),
                    status: LookupStatus::Searching,
                });

                let (lrc, rate_limited) = fetch_from_fallback_chain(
                    &req.query,
                    req.track_length,
                    &req.settings,
                    &mut caches,
                    &res_tx,
                );
                let _ = res_tx.send(FallbackEvent::Completed {
                    query: req.query,
                    lrc,
                    rate_limited,
                });
            }
        });

        Self {
            tx: req_tx,
            rx: res_rx,
        }
    }

    pub fn request(&self, query: TrackQuery, track_length: Option<Duration>, settings: Settings) {
        let _ = self.tx.send(FallbackRequest {
            query,
            track_length,
            settings,
        });
    }

    pub fn poll_event(&self) -> Option<FallbackEvent> {
        self.rx.try_recv().ok()
    }
}

/// A candidate lyric result returned by a single provider.
#[derive(Debug, Clone)]
pub struct ProviderCandidate {
    pub provider: FallbackProviderKind,
    pub provider_order: usize,
    pub synced_lrc: Option<ParsedLrc>,
    pub plain_lrc: Option<ParsedLrc>,
    #[allow(dead_code)]
    pub duration_secs: Option<f64>,
    pub matches_playing_length: bool,
    pub score: f64,
}

impl ProviderCandidate {
    pub fn has_synced(&self) -> bool {
        self.synced_lrc.is_some()
    }
}

/// Canonical recording metadata identified via Deezer / MusicBrainz or cleaned from video tags.
#[derive(Debug, Clone)]
pub struct LookupMetadata {
    pub cleaned: CleanedTrackMetadata,
    pub effective_query: TrackQuery,
    pub target_duration: Option<Duration>,
}

/// Compute the exponential retry delay after `attempt` failed lookups (0-indexed):
/// 30s -> 60s -> 120s -> 240s -> 480s -> capped at 600s (10 minutes).
pub fn retry_backoff_duration(attempt: u32) -> Duration {
    let shift = attempt.min(5);
    let secs = (30u64.saturating_mul(1u64 << shift)).min(600);
    Duration::from_secs(secs)
}

/// Check whether a candidate recording duration matches the playing track's length closely.
pub fn is_length_matched(track_length: Option<Duration>, cand_secs: Option<f64>) -> bool {
    let (Some(t_dur), Some(c_secs)) = (track_length, cand_secs) else {
        return false;
    };
    let t_secs = t_dur.as_secs_f64();
    if t_secs <= 5.0 || c_secs <= 5.0 {
        return false;
    }
    (t_secs - c_secs).abs() <= LENGTH_MATCH_TOLERANCE_SECS
}

/// Check whether a candidate recording duration is within the allowed ±30s window of the playing track.
pub fn is_within_duration_bounds(track_length: Option<Duration>, cand_secs: Option<f64>) -> bool {
    let (Some(t_dur), Some(c_secs)) = (track_length, cand_secs) else {
        return true;
    };
    let t_secs = t_dur.as_secs_f64();
    if t_secs <= 5.0 || c_secs <= 5.0 {
        return true;
    }
    (t_secs - c_secs).abs() <= MAX_DURATION_DIFF_SECS
}

/// Estimate the effective recording duration of a parsed LRC if the provider did not supply an explicit duration.
fn estimate_lrc_duration_secs(lrc: &ParsedLrc, explicit_secs: Option<f64>) -> Option<f64> {
    if let Some(s) = explicit_secs.filter(|&s| s > 5.0) {
        return Some(s);
    }
    lrc.lines
        .last()
        .map(|l| l.timestamp.as_secs_f64() + 5.0)
        .filter(|&s| s > 10.0)
}

/// Execute the full remote lyrics lookup pipeline:
/// 1. Clean up video-style metadata (`Artist x Guest - Song (Official Video)`, `Song by Artist`, channel names).
/// 2. Identify the recording through Deezer and MusicBrainz.
/// 3. Query `lrcmux`, `LyricsPlus`, and `LRCLIB` in order.
/// 4. Query `KuGou` and `NetEase Cloud Music` ONLY when none of the above has synced lyrics timed for a recording of the playing length.
/// 5. Prefer synced lyrics timed for a recording whose length matches the playing track; otherwise compare synced lyrics across all sources line by line and pick the version whose line timings the other sources confirm (ties go to the earlier source).
/// 6. Use plain text only when nothing is synced.
fn fetch_from_fallback_chain(
    raw_query: &TrackQuery,
    track_length: Option<Duration>,
    settings: &Settings,
    caches: &mut ProviderCaches,
    event_tx: &Sender<FallbackEvent>,
) -> (Option<ParsedLrc>, bool) {
    if raw_query.title.trim().is_empty() {
        return (None, false);
    }

    let cache_key = build_query_cache_key(raw_query, settings);
    if let Some(cached) = caches.get_cached_lyrics(&cache_key) {
        return (Some(cached), false);
    }

    let lookup = prepare_lookup_metadata(raw_query, track_length, settings, caches);
    let enabled = settings.enabled_fallback_providers();
    if enabled.is_empty() {
        return (None, false);
    }

    let mut candidates: Vec<ProviderCandidate> = Vec::new();
    let mut have_length_matched_synced = false;
    let mut any_rate_limited = false;

    for (order_idx, provider) in enabled.iter().copied().enumerate() {
        // KuGou and NetEase Cloud Music are queried ONLY when none of the earlier providers
        // has synced lyrics timed for a recording of the playing length.
        if provider.is_secondary_cjk_provider() && have_length_matched_synced {
            continue;
        }
        let ep = ServiceEndpoint::Provider(provider);
        if caches.is_on_cooldown(ep) {
            if caches.is_rate_limited_429(ep) {
                any_rate_limited = true;
            }
            continue;
        }

        let cand = match provider {
            FallbackProviderKind::LrcMux => fetch_lrcmux(&lookup, order_idx, settings, caches),
            FallbackProviderKind::LyricsPlus => {
                fetch_lyricsplus(&lookup, order_idx, settings, caches)
            }
            FallbackProviderKind::LrcLib => {
                fetch_lrclib_with_retry(&lookup, raw_query, order_idx, settings, caches)
            }
            FallbackProviderKind::KuGou => {
                fetch_kugou_with_retry(&lookup, raw_query, order_idx, settings, caches)
            }
            FallbackProviderKind::NetEase => {
                fetch_netease_with_retry(&lookup, raw_query, order_idx, settings, caches)
            }
        };

        if caches.is_rate_limited_429(ep) {
            any_rate_limited = true;
        }

        if let Some(c) = cand {
            if c.has_synced() && c.matches_playing_length {
                have_length_matched_synced = true;
            }
            let step_prog = ((candidates.len() as f32 + 1.0) / 2.0).clamp(0.35, 0.85);
            let _ = event_tx.send(FallbackEvent::Status {
                query: raw_query.clone(),
                status: LookupStatus::Downloading {
                    progress: step_prog,
                },
            });
            candidates.push(c);
        }
    }

    let Some(chosen) = select_best_candidate(candidates) else {
        return (None, any_rate_limited);
    };
    if chosen.lines.is_empty() {
        return (None, any_rate_limited);
    }

    // Emit final downloading progress steps so the user sees the download progress bar fill
    for prog in [0.65f32, 0.90, 1.0] {
        let _ = event_tx.send(FallbackEvent::Status {
            query: raw_query.clone(),
            status: LookupStatus::Downloading { progress: prog },
        });
        thread::sleep(Duration::from_millis(28));
    }

    caches.insert_cached_lyrics(cache_key, chosen.clone());
    (Some(chosen), false)
}

/// Select the winning `ParsedLrc` from the collected provider candidates:
/// 1. Synced lyrics timed for a recording whose length matches the playing track (in provider priority order).
/// 2. When none match the playing length, compare synced lyrics from every source line by line;
///    the version whose line timings the other sources confirm wins (ties go to the earlier source).
/// 3. Plain text is used only when nothing is synced.
pub fn select_best_candidate(candidates: Vec<ProviderCandidate>) -> Option<ParsedLrc> {
    // 1. Prefer synced lyrics timed for a recording whose length matches the playing track
    for cand in &candidates {
        if cand.matches_playing_length {
            if let Some(ref synced) = cand.synced_lrc {
                let mut out = synced.clone();
                out.source_label = Some(format!("Lyrics from {}", cand.provider.display_name()));
                out.match_score = cand.score;
                return Some(out);
            }
        }
    }

    // 2. Collect all sources with valid synced lyrics and run line-by-line timing consensus
    let synced_sources: Vec<(usize, FallbackProviderKind, &ParsedLrc, f64)> = candidates
        .iter()
        .filter_map(|c| {
            c.synced_lrc
                .as_ref()
                .map(|lrc| (c.provider_order, c.provider, lrc, c.score))
        })
        .collect();

    if !synced_sources.is_empty() {
        let stripped: Vec<(usize, FallbackProviderKind, &ParsedLrc)> = synced_sources
            .iter()
            .map(|&(ord, prov, lrc, _)| (ord, prov, lrc))
            .collect();
        let best_idx = select_best_synced_by_consensus(&stripped);
        let (_, provider, winning_lrc, score) = synced_sources[best_idx];
        let mut out = winning_lrc.clone();
        out.source_label = Some(format!("Lyrics from {}", provider.display_name()));
        out.match_score = score;
        return Some(out);
    }

    // 3. Plain text is used only when nothing is synced
    for cand in candidates {
        if let Some(mut plain) = cand.plain_lrc {
            plain.source_label = Some(format!("Lyrics from {}", cand.provider.display_name()));
            plain.match_score = cand.score;
            return Some(plain);
        }
    }

    None
}

/// Compare synced lyrics across sources line by line and return the index into `sources`
/// whose line timings are confirmed by the most other sources. Ties go to the earlier source.
pub fn select_best_synced_by_consensus(
    sources: &[(usize, FallbackProviderKind, &ParsedLrc)],
) -> usize {
    if sources.len() <= 1 {
        return 0;
    }

    let mut best_idx = 0usize;
    let mut best_confirmed_lines = 0usize;

    for (i, &(_, _, cand_i)) in sources.iter().enumerate() {
        let mut confirmed_lines = 0usize;

        for (line_idx, line_i) in cand_i.lines.iter().enumerate() {
            let norm_i = normalize(&line_i.text);
            if norm_i.is_empty() {
                continue;
            }
            let ts_i = line_i.timestamp.as_secs_f64();

            // Check if any other source confirms this line's timing
            let confirmed_by_other = sources.iter().enumerate().any(|(j, &(_, _, cand_j))| {
                if i == j {
                    return false;
                }
                cand_j.lines.iter().enumerate().any(|(other_idx, line_j)| {
                    let ts_j = line_j.timestamp.as_secs_f64();
                    if (ts_i - ts_j).abs() > LINE_CONSENSUS_TOLERANCE_SECS {
                        return false;
                    }
                    let norm_j = normalize(&line_j.text);
                    if !norm_j.is_empty() && string_similarity(&norm_i, &norm_j) >= 0.72 {
                        return true;
                    }
                    // Also allow positional confirmation when line counts match closely
                    cand_i.lines.len().abs_diff(cand_j.lines.len()) <= 2
                        && line_idx.abs_diff(other_idx) <= 1
                })
            });

            if confirmed_by_other {
                confirmed_lines += 1;
            }
        }

        // Strictly greater (`>`) ensures ties go to the earlier source in the list
        if confirmed_lines > best_confirmed_lines {
            best_confirmed_lines = confirmed_lines;
            best_idx = i;
        }
    }

    best_idx
}

// -----------------------------------------------------------------------------
// Recording Identification via Deezer & MusicBrainz
// -----------------------------------------------------------------------------

fn prepare_lookup_metadata(
    raw_query: &TrackQuery,
    track_length: Option<Duration>,
    settings: &Settings,
    caches: &mut ProviderCaches,
) -> LookupMetadata {
    let cleaned = clean_video_metadata(raw_query);
    let mut effective_query = TrackQuery {
        title: cleaned.title.clone(),
        artists: if cleaned.artist.is_empty() {
            Vec::new()
        } else {
            vec![cleaned.artist.clone()]
        },
        album: cleaned.album.clone(),
        url: raw_query.url.clone(),
    };
    let mut target_duration = track_length;

    if settings.enable_metadata_identify {
        if let Some((id_title, id_artist, id_album, id_dur)) =
            identify_recording_deezer_or_musicbrainz(&cleaned, track_length, settings, caches)
        {
            effective_query.title = id_title;
            if !id_artist.is_empty() {
                effective_query.artists = vec![id_artist];
            }
            if id_album.is_some() {
                effective_query.album = id_album;
            }
            if target_duration.is_none() && id_dur.is_some() {
                target_duration = id_dur;
            }
        }
    }

    LookupMetadata {
        cleaned,
        effective_query,
        target_duration,
    }
}

fn identify_recording_deezer_or_musicbrainz(
    cleaned: &CleanedTrackMetadata,
    track_length: Option<Duration>,
    settings: &Settings,
    caches: &mut ProviderCaches,
) -> Option<(String, String, Option<String>, Option<Duration>)> {
    let agent = http_agent();
    let probe_query = TrackQuery {
        title: cleaned.core_title.clone(),
        artists: if cleaned.core_artist.is_empty() {
            Vec::new()
        } else {
            vec![cleaned.core_artist.clone()]
        },
        album: cleaned.album.clone(),
        url: None,
    };

    // 1. Try Deezer Search API first
    if !caches.is_on_cooldown(ServiceEndpoint::Deezer) {
        let q_str = if !cleaned.core_artist.is_empty() {
            format!(
                "artist:\"{}\" track:\"{}\"",
                cleaned.core_artist, cleaned.core_title
            )
        } else {
            cleaned.core_title.clone()
        };
        let url = format!(
            "https://api.deezer.com/search?q={}&limit=5",
            urlencoding::encode(&q_str)
        );
        if let Some(json) =
            endpoint_http_get_json(&agent, ServiceEndpoint::Deezer, &url, &[], caches)
        {
            if let Some(items) = json.get("data").and_then(|d| d.as_array()) {
                let mut best: Option<(f64, String, String, Option<String>, Option<Duration>)> =
                    None;
                for item in items {
                    let Some(c_title) = item.get("title").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let c_artist = item
                        .get("artist")
                        .and_then(|a| a.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let c_album = item
                        .get("album")
                        .and_then(|a| a.get("title"))
                        .and_then(|v| v.as_str());
                    let c_dur = item.get("duration").and_then(|v| v.as_f64());

                    if !is_within_duration_bounds(track_length, c_dur) {
                        continue;
                    }
                    let score = score_remote_metadata(
                        &probe_query,
                        c_title,
                        c_artist,
                        c_album,
                        track_length,
                        c_dur,
                    );
                    if score >= settings.fallback_match_threshold.max(0.75) {
                        if best.as_ref().map_or(true, |(b_s, ..)| score > *b_s) {
                            best = Some((
                                score,
                                c_title.to_string(),
                                c_artist.to_string(),
                                c_album.map(String::from),
                                c_dur.map(Duration::from_secs_f64),
                            ));
                        }
                    }
                }
                if let Some((_, t, a, al, d)) = best {
                    return Some((t, a, al, d));
                }
            }
        }
    }

    // 2. Try MusicBrainz Recording Search API
    if !caches.is_on_cooldown(ServiceEndpoint::MusicBrainz) {
        let mb_query = if !cleaned.core_artist.is_empty() {
            format!(
                "recording:\"{}\" AND artist:\"{}\"",
                cleaned.core_title, cleaned.core_artist
            )
        } else {
            format!("recording:\"{}\"", cleaned.core_title)
        };
        let url = format!(
            "https://musicbrainz.org/ws/2/recording?query={}&limit=5&fmt=json",
            urlencoding::encode(&mb_query)
        );
        if let Some(json) = endpoint_http_get_json(
            &agent,
            ServiceEndpoint::MusicBrainz,
            &url,
            &[("Accept", "application/json")],
            caches,
        ) {
            if let Some(recordings) = json.get("recordings").and_then(|r| r.as_array()) {
                let mut best: Option<(f64, String, String, Option<String>, Option<Duration>)> =
                    None;
                for rec in recordings {
                    let Some(c_title) = rec.get("title").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let c_artist = rec
                        .get("artist-credit")
                        .and_then(|ac| ac.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|first| {
                            first
                                .get("name")
                                .and_then(|v| v.as_str())
                                .or_else(|| {
                                    first
                                        .get("artist")
                                        .and_then(|a| a.get("name"))
                                        .and_then(|v| v.as_str())
                                })
                        })
                        .unwrap_or("");
                    let c_album = rec
                        .get("releases")
                        .and_then(|r| r.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|rel| rel.get("title"))
                        .and_then(|v| v.as_str());
                    let c_dur = rec
                        .get("length")
                        .and_then(|v| v.as_f64())
                        .map(|ms| ms / 1000.0);

                    if !is_within_duration_bounds(track_length, c_dur) {
                        continue;
                    }
                    let score = score_remote_metadata(
                        &probe_query,
                        c_title,
                        c_artist,
                        c_album,
                        track_length,
                        c_dur,
                    );
                    if score >= settings.fallback_match_threshold.max(0.75) {
                        if best.as_ref().map_or(true, |(b_s, ..)| score > *b_s) {
                            best = Some((
                                score,
                                c_title.to_string(),
                                c_artist.to_string(),
                                c_album.map(String::from),
                                c_dur.map(Duration::from_secs_f64),
                            ));
                        }
                    }
                }
                if let Some((_, t, a, al, d)) = best {
                    return Some((t, a, al, d));
                }
            }
        }
    }

    None
}

// -----------------------------------------------------------------------------
// Validation & Scoring Helpers
// -----------------------------------------------------------------------------

/// Strictly verify that the parsed LRC contains genuine synced timestamps.
pub fn is_valid_synced_lrc(lrc: &ParsedLrc) -> bool {
    if !lrc.is_synced || lrc.lines.len() < 2 {
        return false;
    }
    let non_empty_timed = lrc
        .lines
        .iter()
        .filter(|l| !l.text.trim().is_empty())
        .count();
    let has_positive_ts = lrc
        .lines
        .iter()
        .any(|l| l.timestamp > Duration::from_millis(100));
    non_empty_timed >= 2 && has_positive_ts
}

/// Score a remote track candidate against the `TrackQuery`.
/// Rejects unrelated/ambiguous recordings and recordings differing by > 30s from the playing track.
pub fn score_remote_metadata(
    query: &TrackQuery,
    cand_title: &str,
    cand_artist: &str,
    cand_album: Option<&str>,
    query_len: Option<Duration>,
    cand_len_secs: Option<f64>,
) -> f64 {
    if !is_within_duration_bounds(query_len, cand_len_secs) {
        return 0.0;
    }

    let q_clean = clean_video_metadata(query);
    let q_title = normalize(&q_clean.core_title);
    let c_title = normalize(cand_title);
    if q_title.is_empty() || c_title.is_empty() {
        return 0.0;
    }

    let title_sim = string_similarity(&q_title, &c_title)
        .max(string_similarity(&normalize(&query.title), &c_title));

    // Reject unrelated songs whose title doesn't even loosely match
    if title_sim < 0.58 {
        return 0.0;
    }

    let q_artist = normalize(&q_clean.core_artist);
    let c_artist = normalize(cand_artist);
    let artist_sim = if !q_artist.is_empty() && !c_artist.is_empty() {
        let sim = string_similarity(&q_artist, &c_artist)
            .max(string_similarity(&normalize(&q_clean.artist), &c_artist));
        if sim < 0.45 {
            // Unrelated artist
            return 0.0;
        }
        sim
    } else {
        0.75
    };

    let mut score = title_sim * 0.65 + artist_sim * 0.35;

    if let (Some(q_al), Some(c_al)) = (query.album.as_deref(), cand_album) {
        let q_al_n = normalize(q_al);
        let c_al_n = normalize(c_al);
        if !q_al_n.is_empty() && !c_al_n.is_empty() {
            let al_sim = string_similarity(&q_al_n, &c_al_n);
            if al_sim > 0.80 {
                score = (score + 0.04 * al_sim).min(1.0);
            }
        }
    }

    if let (Some(q_dur), Some(c_secs)) = (query_len, cand_len_secs) {
        let q_secs = q_dur.as_secs_f64();
        if q_secs > 5.0 && c_secs > 5.0 {
            let diff = (q_secs - c_secs).abs();
            if diff <= LENGTH_MATCH_TOLERANCE_SECS {
                score = (score + 0.05).min(1.0);
            } else if diff > 15.0 {
                score *= 0.90;
            }
        }
    }

    score.clamp(0.0, 1.0)
}

/// Evaluate censorship vs uncensored adjustment for candidate ranking.
pub fn uncensored_ranking_adjustment(
    cand_title: &str,
    cand_album: Option<&str>,
    lyrics_text: &str,
    explicit_flag: Option<bool>,
) -> f64 {
    let mut delta = 0.0;

    let title_lower = cand_title.to_ascii_lowercase();
    let album_lower = cand_album.unwrap_or("").to_ascii_lowercase();
    for clean_marker in ["clean", "radio edit", "censored", "amended"] {
        if title_lower.contains(clean_marker) || album_lower.contains(clean_marker) {
            delta -= 0.12;
        }
    }

    if let Some(is_explicit) = explicit_flag {
        if is_explicit {
            delta += 0.05;
        }
    }

    let censored_count = count_censored_patterns(lyrics_text);
    if censored_count > 0 {
        delta -= (censored_count as f64 * 0.04).min(0.25);
    }

    let uncensored_count = count_uncensored_explicit_words(lyrics_text);
    if uncensored_count > 0 && censored_count == 0 {
        delta += (uncensored_count as f64 * 0.02).min(0.08);
    }

    delta
}

fn count_censored_patterns(text: &str) -> usize {
    let mut count = 0usize;
    for token in text.split_whitespace() {
        let trimmed = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '*');
        if trimmed.len() >= 3 && trimmed.contains('*') {
            let stars = trimmed.chars().filter(|&c| c == '*').count();
            let alpha = trimmed.chars().filter(|c| c.is_alphabetic()).count();
            if (stars >= 1 && alpha >= 1) || stars >= 3 {
                count += 1;
            }
        }
    }
    count
}

fn count_uncensored_explicit_words(text: &str) -> usize {
    const EXPLICIT_WORDS: &[&str] = &[
        "fuck", "fucking", "fucked", "motherfucker", "shit", "bullshit", "bitch", "bitches",
        "nigga", "niggas", "asshole", "dick", "pussy", "damn", "goddamn",
    ];
    let mut count = 0usize;
    for word in text.split(|c: char| !c.is_alphabetic()) {
        if word.is_empty() {
            continue;
        }
        let lower = word.to_ascii_lowercase();
        if EXPLICIT_WORDS.contains(&lower.as_str()) {
            count += 1;
        }
    }
    count
}

fn string_similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    let jw = strsim::jaro_winkler(a, b);
    let sd = strsim::sorensen_dice(a, b);
    jw * 0.55 + sd * 0.45
}

fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(4))
        .timeout_read(Duration::from_secs(6))
        .user_agent("TermLRC/0.1.0 (https://github.com/MaveDX/TermLRC)")
        .build()
}

// -----------------------------------------------------------------------------
// 1. lrcmux Provider (https://lrcmux.dev / https://api.lrcmux.dev)
// -----------------------------------------------------------------------------

fn fetch_lrcmux(
    lookup: &LookupMetadata,
    provider_order: usize,
    settings: &Settings,
    caches: &mut ProviderCaches,
) -> Option<ProviderCandidate> {
    let query = &lookup.effective_query;
    let title = query.title.trim();
    if title.is_empty() {
        return None;
    }
    let artist = query.primary_artist().unwrap_or("");
    let album = query.album.as_deref().unwrap_or("");
    let dur_secs = lookup.target_duration.map(|d| d.as_secs());

    let base_url = if !settings.lrcmux_api_url.trim().is_empty() {
        settings.lrcmux_api_url.trim().trim_end_matches('/').to_string()
    } else {
        String::from("https://api.lrcmux.dev")
    };

    let agent = http_agent();
    let endpoint = ServiceEndpoint::Provider(FallbackProviderKind::LrcMux);

    // Try lrcmux KPOE/LyricsPlus compatibility route and standard `/api/get` / `/v1/lyrics` routes
    let mut urls = Vec::with_capacity(2);
    let mut kpoe_url = format!(
        "{}/compat/kpoe/v2/lyrics/get?title={}&artist={}",
        base_url,
        urlencoding::encode(title),
        urlencoding::encode(artist)
    );
    if !album.is_empty() {
        kpoe_url.push_str(&format!("&album={}", urlencoding::encode(album)));
    }
    if let Some(d) = dur_secs {
        kpoe_url.push_str(&format!("&duration={}", d));
    }
    urls.push(kpoe_url);

    let mut std_url = format!(
        "{}/api/get?track_name={}&artist_name={}",
        base_url,
        urlencoding::encode(title),
        urlencoding::encode(artist)
    );
    if !album.is_empty() {
        std_url.push_str(&format!("&album_name={}", urlencoding::encode(album)));
    }
    if let Some(d) = dur_secs {
        std_url.push_str(&format!("&duration={}", d));
    }
    urls.push(std_url);

    for url in urls {
        if caches.is_on_cooldown(endpoint) {
            break;
        }
        let Some(json) = endpoint_http_get_json(&agent, endpoint, &url, &[], caches) else {
            continue;
        };
        if let Some(cand) = parse_lrcmux_or_kpoe_response(
            &json,
            FallbackProviderKind::LrcMux,
            provider_order,
            lookup,
            settings,
        ) {
            return Some(cand);
        }
    }

    None
}

// -----------------------------------------------------------------------------
// 2. LyricsPlus Provider (https://github.com/ibratabian17/lyricsplus)
// -----------------------------------------------------------------------------

fn fetch_lyricsplus(
    lookup: &LookupMetadata,
    provider_order: usize,
    settings: &Settings,
    caches: &mut ProviderCaches,
) -> Option<ProviderCandidate> {
    let query = &lookup.effective_query;
    let title = query.title.trim();
    if title.is_empty() {
        return None;
    }
    let artist = query.primary_artist().unwrap_or("");
    let album = query.album.as_deref().unwrap_or("");
    let dur_secs = lookup.target_duration.map(|d| d.as_secs());

    let bases: Vec<String> = if !settings.lyricsplus_api_url.trim().is_empty() {
        vec![settings
            .lyricsplus_api_url
            .trim()
            .trim_end_matches('/')
            .to_string()]
    } else {
        vec![
            String::from("https://lyricsplus.prjktla.workers.dev"),
            String::from("https://lyricsplus.binimum.org"),
        ]
    };

    let agent = http_agent();
    let endpoint = ServiceEndpoint::Provider(FallbackProviderKind::LyricsPlus);

    for base in bases {
        if caches.is_on_cooldown(endpoint) {
            break;
        }
        let mut url = format!(
            "{}/v2/lyrics/get?title={}&artist={}",
            base,
            urlencoding::encode(title),
            urlencoding::encode(artist)
        );
        if !album.is_empty() {
            url.push_str(&format!("&album={}", urlencoding::encode(album)));
        }
        if let Some(d) = dur_secs {
            url.push_str(&format!("&duration={}", d));
        }

        let Some(json) = endpoint_http_get_json(&agent, endpoint, &url, &[], caches) else {
            continue;
        };
        if let Some(cand) = parse_lrcmux_or_kpoe_response(
            &json,
            FallbackProviderKind::LyricsPlus,
            provider_order,
            lookup,
            settings,
        ) {
            return Some(cand);
        }
    }

    None
}

/// Parse either a KPOE/LyricsPlus JSON payload (`lyrics: [{ time, duration, text, syllabus }]` or
/// `lyrics: { lines: [...] }`) or an lrcmux/LRCLIB-style JSON payload (`syncedLyrics`, `plainLyrics`).
pub fn parse_lrcmux_or_kpoe_response(
    json: &Value,
    provider: FallbackProviderKind,
    provider_order: usize,
    lookup: &LookupMetadata,
    settings: &Settings,
) -> Option<ProviderCandidate> {
    if json.get("error").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }

    let explicit_dur = json
        .get("duration")
        .and_then(|v| v.as_f64())
        .or_else(|| json.pointer("/metadata/duration").and_then(|v| v.as_f64()));

    if !is_within_duration_bounds(lookup.target_duration, explicit_dur) {
        return None;
    }

    // Check metadata match if candidate reports track/artist names
    let cand_title = json
        .get("trackName")
        .or_else(|| json.get("title"))
        .or_else(|| json.pointer("/metadata/title"))
        .and_then(|v| v.as_str());
    let cand_artist = json
        .get("artistName")
        .or_else(|| json.get("artist"))
        .or_else(|| json.pointer("/metadata/artist"))
        .and_then(|v| v.as_str());

    let mut base_score = 0.85;
    if let (Some(ct), Some(ca)) = (cand_title, cand_artist) {
        base_score = score_remote_metadata(
            &lookup.effective_query,
            ct,
            ca,
            None,
            lookup.target_duration,
            explicit_dur,
        );
        if base_score < settings.fallback_match_threshold {
            return None;
        }
    }

    // 1. Check for LRC string in `syncedLyrics` or `lrc`
    if let Some(synced_str) = json
        .get("syncedLyrics")
        .or_else(|| json.get("lrc"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
    {
        let parsed = ParsedLrc::parse(synced_str);
        if is_valid_synced_lrc(&parsed) {
            let eff_dur = estimate_lrc_duration_secs(&parsed, explicit_dur);
            if !is_within_duration_bounds(lookup.target_duration, eff_dur) {
                return None;
            }
            let matches_len = is_length_matched(lookup.target_duration, eff_dur);
            return Some(ProviderCandidate {
                provider,
                provider_order,
                synced_lrc: Some(parsed),
                plain_lrc: None,
                duration_secs: eff_dur,
                matches_playing_length: matches_len,
                score: base_score,
            });
        }
    }

    // 2. Check for KPOE / LyricsPlus array or `lyrics.lines` array
    if let Some(parsed) = parse_kpoe_lyrics_json(json) {
        if is_valid_synced_lrc(&parsed) {
            let eff_dur = estimate_lrc_duration_secs(&parsed, explicit_dur);
            if !is_within_duration_bounds(lookup.target_duration, eff_dur) {
                return None;
            }
            let matches_len = is_length_matched(lookup.target_duration, eff_dur);
            return Some(ProviderCandidate {
                provider,
                provider_order,
                synced_lrc: Some(parsed),
                plain_lrc: None,
                duration_secs: eff_dur,
                matches_playing_length: matches_len,
                score: base_score,
            });
        } else if !parsed.lines.is_empty() {
            return Some(ProviderCandidate {
                provider,
                provider_order,
                synced_lrc: None,
                plain_lrc: Some(parsed),
                duration_secs: explicit_dur,
                matches_playing_length: false,
                score: base_score,
            });
        }
    }

    // 3. Check for `plainLyrics` fallback string
    if let Some(plain_str) = json
        .get("plainLyrics")
        .or_else(|| json.get("lyrics").filter(|v| v.is_string()))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
    {
        let plain = ParsedLrc::from_plain_text(plain_str);
        if !plain.lines.is_empty() {
            return Some(ProviderCandidate {
                provider,
                provider_order,
                synced_lrc: None,
                plain_lrc: Some(plain),
                duration_secs: explicit_dur,
                matches_playing_length: false,
                score: base_score,
            });
        }
    }

    None
}

fn parse_kpoe_lyrics_json(json: &Value) -> Option<ParsedLrc> {
    let lines_arr = json
        .get("lyrics")
        .and_then(|l| {
            l.as_array()
                .or_else(|| l.get("lines").and_then(|lines| lines.as_array()))
        })
        .or_else(|| json.get("lines").and_then(|l| l.as_array()))?;

    if lines_arr.is_empty() {
        return None;
    }

    let is_unsynced_type = json
        .get("type")
        .and_then(|t| t.as_str())
        .map(|t| t.eq_ignore_ascii_case("text") || t.eq_ignore_ascii_case("plain"))
        .unwrap_or(false);

    let mut lines = Vec::with_capacity(lines_arr.len());
    for item in lines_arr {
        let start_ms = item
            .get("time")
            .or_else(|| item.get("startTime"))
            .or_else(|| item.get("startTimeMs"))
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_f64().map(|f| f.max(0.0) as u64))
                    .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
            })
            .unwrap_or(0);

        let mut segments = Vec::new();
        let word_list = item
            .get("syllabus")
            .or_else(|| item.get("words"))
            .and_then(|v| v.as_array());
        if let Some(words) = word_list {
            for w in words {
                let w_text = w.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if w_text.is_empty() {
                    continue;
                }
                let w_ms = w
                    .get("time")
                    .or_else(|| w.get("startTime"))
                    .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)))
                    .unwrap_or(start_ms);
                segments.push(TimedSegment {
                    timestamp: Duration::from_millis(w_ms),
                    text: w_text.to_string(),
                });
            }
        }

        let text = item
            .get("text")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| {
                segments
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
                    .trim()
                    .to_string()
            });

        lines.push(LyricLine {
            timestamp: Duration::from_millis(start_ms),
            text,
            segments,
        });
    }

    lines.sort_by_key(|l| l.timestamp);
    let has_real_timestamps =
        !is_unsynced_type && lines.iter().any(|l| l.timestamp > Duration::from_millis(100));

    Some(ParsedLrc {
        metadata: LrcMetadata::default(),
        lines,
        source_label: None,
        is_synced: has_real_timestamps,
        match_score: 1.0,
    })
}

// -----------------------------------------------------------------------------
// 3. LRCLIB Provider (with stripped title+artist retry)
// -----------------------------------------------------------------------------

fn fetch_lrclib_with_retry(
    lookup: &LookupMetadata,
    raw_query: &TrackQuery,
    provider_order: usize,
    settings: &Settings,
    caches: &mut ProviderCaches,
) -> Option<ProviderCandidate> {
    if let Some(cand) = fetch_lrclib_single(
        &lookup.effective_query,
        lookup.target_duration,
        provider_order,
        settings,
        caches,
        false,
    ) {
        return Some(cand);
    }

    // When LRCLIB finds nothing for the full metadata, search once more with just
    // the song title and its artist, leaving out the uploader's channel name
    // (for example `Crazy chomin` for a `Crazy (feat. Yina) by chomin` video from `DJMAX ARCHIVE`).
    if lookup.cleaned.has_distinct_stripped_query(raw_query) {
        let stripped_query = TrackQuery {
            title: lookup.cleaned.core_title.clone(),
            artists: if lookup.cleaned.core_artist.is_empty() {
                Vec::new()
            } else {
                vec![lookup.cleaned.core_artist.clone()]
            },
            album: None,
            url: None,
        };
        return fetch_lrclib_single(
            &stripped_query,
            lookup.target_duration,
            provider_order,
            settings,
            caches,
            true,
        );
    }

    None
}

fn fetch_lrclib_single(
    query: &TrackQuery,
    track_length: Option<Duration>,
    provider_order: usize,
    settings: &Settings,
    caches: &mut ProviderCaches,
    use_combined_q: bool,
) -> Option<ProviderCandidate> {
    if query.title.trim().is_empty() {
        return None;
    }

    let agent = http_agent();
    let endpoint = ServiceEndpoint::Provider(FallbackProviderKind::LrcLib);
    let artist = query.primary_artist().unwrap_or("");
    let url = if use_combined_q || artist.is_empty() {
        let combined = if artist.is_empty() {
            query.title.clone()
        } else {
            format!("{} {}", query.title, artist)
        };
        format!(
            "https://lrclib.net/api/search?q={}",
            urlencoding::encode(&combined)
        )
    } else {
        format!(
            "https://lrclib.net/api/search?track_name={}&artist_name={}",
            urlencoding::encode(&query.title),
            urlencoding::encode(artist)
        )
    };

    let json = endpoint_http_get_json(&agent, endpoint, &url, &[], caches)?;
    parse_lrclib_candidates(&json, query, track_length, provider_order, settings)
}

pub fn parse_lrclib_candidates(
    json: &Value,
    query: &TrackQuery,
    track_length: Option<Duration>,
    provider_order: usize,
    settings: &Settings,
) -> Option<ProviderCandidate> {
    let items = json.as_array()?;
    let mut best_synced: Option<(f64, ParsedLrc, Option<f64>, bool)> = None;
    let mut best_plain: Option<(f64, ParsedLrc, Option<f64>)> = None;

    for item in items {
        let cand_title = item
            .get("trackName")
            .or_else(|| item.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let cand_artist = item
            .get("artistName")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let cand_album = item.get("albumName").and_then(|v| v.as_str());
        let cand_dur = item.get("duration").and_then(|v| v.as_f64());

        if !is_within_duration_bounds(track_length, cand_dur) {
            continue;
        }

        let base_score = score_remote_metadata(
            query,
            cand_title,
            cand_artist,
            cand_album,
            track_length,
            cand_dur,
        );
        if base_score < settings.fallback_match_threshold {
            continue;
        }

        if let Some(synced_str) = item
            .get("syncedLyrics")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        {
            let mut parsed = ParsedLrc::parse(synced_str);
            if is_valid_synced_lrc(&parsed) {
                if parsed.metadata.title.is_none() && !cand_title.is_empty() {
                    parsed.metadata.title = Some(cand_title.to_string());
                }
                if parsed.metadata.artist.is_none() && !cand_artist.is_empty() {
                    parsed.metadata.artist = Some(cand_artist.to_string());
                }
                let rank_score = if settings.prefer_uncensored {
                    base_score
                        + uncensored_ranking_adjustment(cand_title, cand_album, synced_str, None)
                } else {
                    base_score
                };
                let eff_dur = estimate_lrc_duration_secs(&parsed, cand_dur);
                let len_matched = is_length_matched(track_length, eff_dur);
                let is_better = match &best_synced {
                    None => true,
                    Some((prev_s, _, _, prev_len_m)) => {
                        (len_matched && !*prev_len_m)
                            || (len_matched == *prev_len_m && rank_score > *prev_s)
                    }
                };
                if is_better {
                    best_synced = Some((rank_score, parsed, eff_dur, len_matched));
                }
                continue;
            }
        }

        if let Some(plain_str) = item
            .get("plainLyrics")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        {
            let plain = ParsedLrc::from_plain_text(plain_str);
            if !plain.lines.is_empty() {
                if best_plain.as_ref().map_or(true, |(prev_s, ..)| base_score > *prev_s) {
                    best_plain = Some((base_score, plain, cand_dur));
                }
            }
        }
    }

    if let Some((score, lrc, dur, len_matched)) = best_synced {
        return Some(ProviderCandidate {
            provider: FallbackProviderKind::LrcLib,
            provider_order,
            synced_lrc: Some(lrc),
            plain_lrc: best_plain.map(|(_, p, _)| p),
            duration_secs: dur,
            matches_playing_length: len_matched,
            score,
        });
    }

    if let Some((score, plain, dur)) = best_plain {
        return Some(ProviderCandidate {
            provider: FallbackProviderKind::LrcLib,
            provider_order,
            synced_lrc: None,
            plain_lrc: Some(plain),
            duration_secs: dur,
            matches_playing_length: false,
            score,
        });
    }

    None
}

// -----------------------------------------------------------------------------
// 4. KuGou Provider (with stripped title+artist retry)
// -----------------------------------------------------------------------------

fn fetch_kugou_with_retry(
    lookup: &LookupMetadata,
    raw_query: &TrackQuery,
    provider_order: usize,
    settings: &Settings,
    caches: &mut ProviderCaches,
) -> Option<ProviderCandidate> {
    let full_keyword = if let Some(ar) = lookup.effective_query.primary_artist() {
        format!("{} - {}", ar, lookup.effective_query.title)
    } else {
        lookup.effective_query.title.clone()
    };

    if let Some(cand) = fetch_kugou_keyword(
        &full_keyword,
        &lookup.effective_query,
        lookup.target_duration,
        provider_order,
        settings,
        caches,
    ) {
        return Some(cand);
    }

    if lookup.cleaned.has_distinct_stripped_query(raw_query) {
        let stripped_kw = lookup.cleaned.stripped_title_artist_query();
        let stripped_q = TrackQuery {
            title: lookup.cleaned.core_title.clone(),
            artists: vec![lookup.cleaned.core_artist.clone()],
            album: None,
            url: None,
        };
        return fetch_kugou_keyword(
            &stripped_kw,
            &stripped_q,
            lookup.target_duration,
            provider_order,
            settings,
            caches,
        );
    }

    None
}

fn fetch_kugou_keyword(
    keyword: &str,
    query: &TrackQuery,
    track_length: Option<Duration>,
    provider_order: usize,
    settings: &Settings,
    caches: &mut ProviderCaches,
) -> Option<ProviderCandidate> {
    let agent = http_agent();
    let endpoint = ServiceEndpoint::Provider(FallbackProviderKind::KuGou);
    let dur_ms = track_length.map(|d| d.as_millis() as u64).unwrap_or(0);
    let search_url = if dur_ms > 0 {
        format!(
            "http://krcs.kugou.com/search?ver=1&man=yes&client=pc&keyword={}&duration={}",
            urlencoding::encode(keyword),
            dur_ms
        )
    } else {
        format!(
            "http://krcs.kugou.com/search?ver=1&man=yes&client=pc&keyword={}",
            urlencoding::encode(keyword)
        )
    };

    let search_json = endpoint_http_get_json(&agent, endpoint, &search_url, &[], caches)?;
    let candidates = search_json.get("candidates")?.as_array()?;

    let mut ranked: Vec<(f64, String, String, Option<f64>)> = Vec::new();
    for item in candidates {
        let Some(id) = item.get("id").and_then(|v| {
            v.as_str()
                .map(String::from)
                .or_else(|| v.as_u64().map(|n| n.to_string()))
        }) else {
            continue;
        };
        let Some(accesskey) = item.get("accesskey").and_then(|v| v.as_str()) else {
            continue;
        };
        let c_title = item
            .get("song")
            .or_else(|| item.get("soundname"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let c_artist = item.get("singer").and_then(|v| v.as_str()).unwrap_or("");
        let c_dur_secs = item
            .get("duration")
            .and_then(|v| v.as_f64())
            .map(|ms| if ms > 1000.0 { ms / 1000.0 } else { ms });

        if !is_within_duration_bounds(track_length, c_dur_secs) {
            continue;
        }
        let score = score_remote_metadata(query, c_title, c_artist, None, track_length, c_dur_secs);
        if score >= settings.fallback_match_threshold {
            ranked.push((score, id, accesskey.to_string(), c_dur_secs));
        }
    }

    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    for (score, id, accesskey, c_dur_secs) in ranked.into_iter().take(MAX_CANDIDATE_LYRIC_FETCHES) {
        if caches.is_on_cooldown(endpoint) {
            break;
        }
        let dl_url = format!(
            "http://lyrics.kugou.com/download?ver=1&client=pc&id={}&accesskey={}&fmt=lrc&charset=utf8",
            urlencoding::encode(&id),
            urlencoding::encode(&accesskey)
        );
        let Some(dl_json) = endpoint_http_get_json(&agent, endpoint, &dl_url, &[], caches) else {
            continue;
        };
        let Some(b64_content) = dl_json.get("content").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(decoded_bytes) = decode_base64(b64_content) else {
            continue;
        };
        let lrc_text = String::from_utf8_lossy(&decoded_bytes);
        let parsed = ParsedLrc::parse(&lrc_text);
        if is_valid_synced_lrc(&parsed) {
            let eff_dur = estimate_lrc_duration_secs(&parsed, c_dur_secs);
            let len_matched = is_length_matched(track_length, eff_dur);
            return Some(ProviderCandidate {
                provider: FallbackProviderKind::KuGou,
                provider_order,
                synced_lrc: Some(parsed),
                plain_lrc: None,
                duration_secs: eff_dur,
                matches_playing_length: len_matched,
                score,
            });
        }
    }

    None
}

// -----------------------------------------------------------------------------
// 5. NetEase Cloud Music Provider (with stripped title+artist retry)
// -----------------------------------------------------------------------------

fn fetch_netease_with_retry(
    lookup: &LookupMetadata,
    raw_query: &TrackQuery,
    provider_order: usize,
    settings: &Settings,
    caches: &mut ProviderCaches,
) -> Option<ProviderCandidate> {
    let full_kw = if let Some(ar) = lookup.effective_query.primary_artist() {
        format!("{} {}", lookup.effective_query.title, ar)
    } else {
        lookup.effective_query.title.clone()
    };

    if let Some(cand) = fetch_netease_keyword(
        &full_kw,
        &lookup.effective_query,
        lookup.target_duration,
        provider_order,
        settings,
        caches,
    ) {
        return Some(cand);
    }

    if lookup.cleaned.has_distinct_stripped_query(raw_query) {
        let stripped_kw = lookup.cleaned.stripped_title_artist_query();
        let stripped_q = TrackQuery {
            title: lookup.cleaned.core_title.clone(),
            artists: vec![lookup.cleaned.core_artist.clone()],
            album: None,
            url: None,
        };
        return fetch_netease_keyword(
            &stripped_kw,
            &stripped_q,
            lookup.target_duration,
            provider_order,
            settings,
            caches,
        );
    }

    None
}

fn fetch_netease_keyword(
    keyword: &str,
    query: &TrackQuery,
    track_length: Option<Duration>,
    provider_order: usize,
    settings: &Settings,
    caches: &mut ProviderCaches,
) -> Option<ProviderCandidate> {
    let agent = http_agent();
    let endpoint = ServiceEndpoint::Provider(FallbackProviderKind::NetEase);
    let search_url = format!(
        "https://music.163.com/api/search/get?s={}&type=1&offset=0&limit=6",
        urlencoding::encode(keyword)
    );
    let headers = [("Referer", "https://music.163.com")];
    let search_json = endpoint_http_get_json(&agent, endpoint, &search_url, &headers, caches)?;
    let songs = search_json.pointer("/result/songs")?.as_array()?;

    let mut ranked: Vec<(f64, u64, Option<f64>)> = Vec::new();
    for song in songs {
        let Some(song_id) = song.get("id").and_then(|v| v.as_u64()) else {
            continue;
        };
        let c_title = song.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let c_artist = song
            .get("artists")
            .and_then(|a| a.as_array())
            .and_then(|arr| arr.first())
            .and_then(|first| first.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let c_album = song
            .get("album")
            .and_then(|al| al.get("name"))
            .and_then(|v| v.as_str());
        let c_dur_secs = song
            .get("duration")
            .and_then(|v| v.as_f64())
            .map(|ms| ms / 1000.0);

        if !is_within_duration_bounds(track_length, c_dur_secs) {
            continue;
        }
        let score =
            score_remote_metadata(query, c_title, c_artist, c_album, track_length, c_dur_secs);
        if score >= settings.fallback_match_threshold {
            ranked.push((score, song_id, c_dur_secs));
        }
    }

    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    for (score, song_id, c_dur_secs) in ranked.into_iter().take(MAX_CANDIDATE_LYRIC_FETCHES) {
        if caches.is_on_cooldown(endpoint) {
            break;
        }
        let lrc_url = format!(
            "https://music.163.com/api/song/lyric?os=pc&id={}&lv=-1&tv=-1",
            song_id
        );
        let Some(lrc_json) = endpoint_http_get_json(&agent, endpoint, &lrc_url, &headers, caches)
        else {
            continue;
        };
        let Some(lrc_str) = lrc_json
            .pointer("/lrc/lyric")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        else {
            continue;
        };
        let parsed = ParsedLrc::parse(lrc_str);
        if is_valid_synced_lrc(&parsed) {
            let eff_dur = estimate_lrc_duration_secs(&parsed, c_dur_secs);
            let len_matched = is_length_matched(track_length, eff_dur);
            return Some(ProviderCandidate {
                provider: FallbackProviderKind::NetEase,
                provider_order,
                synced_lrc: Some(parsed),
                plain_lrc: None,
                duration_secs: eff_dur,
                matches_playing_length: len_matched,
                score,
            });
        } else {
            let plain = ParsedLrc::from_plain_text(lrc_str);
            if !plain.lines.is_empty() {
                return Some(ProviderCandidate {
                    provider: FallbackProviderKind::NetEase,
                    provider_order,
                    synced_lrc: None,
                    plain_lrc: Some(plain),
                    duration_secs: c_dur_secs,
                    matches_playing_length: false,
                    score,
                });
            }
        }
    }

    None
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u8;

    for b in input.bytes() {
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\r' | b'\n' | b' ' | b'\t' => continue,
            _ => return None,
        };
        buf = (buf << 6) | (val as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_reject_over_30s_duration_diff_and_prioritise_uncensored() {
        let mut settings = Settings::default();
        settings.fallback_match_threshold = 0.72;
        settings.prefer_uncensored = true;

        let query = TrackQuery {
            title: "200".to_string(),
            artists: vec!["Future".to_string()],
            album: None,
            url: None,
        };

        // 1. Candidate > 30s longer than playing track (150s vs 200s = 50s diff) must be rejected
        let payload_too_long = json!([
            {
                "trackName": "200",
                "artistName": "Future",
                "duration": 200.0,
                "syncedLyrics": "[00:05.00]Line 1\n[00:10.00]Line 2\n"
            }
        ]);
        assert!(
            parse_lrclib_candidates(
                &payload_too_long,
                &query,
                Some(Duration::from_secs(150)),
                0,
                &settings
            )
            .is_none(),
            "Recording > 30s different from playing track must be rejected"
        );

        // 2. Recording within 30s (165s vs 150s = 15s diff) is still matched, and uncensored wins
        let both_payload = json!([
            {
                "trackName": "200 (Clean Version)",
                "artistName": "Future",
                "duration": 150.0,
                "syncedLyrics": "[00:05.00]200 I floor it (f**k them old n***as)\n[00:09.00]Better keep up\n"
            },
            {
                "trackName": "200",
                "artistName": "Future",
                "duration": 150.0,
                "syncedLyrics": "[00:05.00]200 I floor it (fuck them old niggas)\n[00:09.00]Better keep up\n"
            }
        ]);
        let matched = parse_lrclib_candidates(
            &both_payload,
            &query,
            Some(Duration::from_secs(150)),
            0,
            &settings,
        )
        .expect("Should match synced uncensored lyrics");
        let synced = matched.synced_lrc.expect("Should have synced LRC");
        assert!(synced.lines[0].text.contains("fuck them old niggas"));
    }

    #[test]
    fn test_line_by_line_consensus_and_tie_breaking() {
        // Source 0 (lrcmux) has corrupted timestamps ([00:45.00] instead of [00:10.00])
        // Source 1 (LyricsPlus) and Source 2 (LRCLIB) confirm each other's timestamps ([00:10.00], [00:15.00])
        let lrc_corrupted = ParsedLrc::parse("[00:45.00]First line\n[00:55.00]Second line\n");
        let lrc_good_a = ParsedLrc::parse("[00:10.00]First line\n[00:15.00]Second line\n");
        let lrc_good_b = ParsedLrc::parse("[00:10.20]First line\n[00:15.10]Second line\n");

        let sources = vec![
            (0, FallbackProviderKind::LrcMux, &lrc_corrupted),
            (1, FallbackProviderKind::LyricsPlus, &lrc_good_a),
            (2, FallbackProviderKind::LrcLib, &lrc_good_b),
        ];

        // Source 1 and Source 2 both have 2 confirmed lines; tie goes to the earlier source (index 1, LyricsPlus)!
        let winner = select_best_synced_by_consensus(&sources);
        assert_eq!(winner, 1);
    }

    #[test]
    fn test_kpoe_and_exponential_backoff() {
        let kpoe_json = json!({
            "type": "Word",
            "lyrics": [
                {
                    "time": 5000,
                    "duration": 3000,
                    "text": "Hello world",
                    "syllabus": [
                        { "time": 5000, "duration": 1200, "text": "Hello " },
                        { "time": 6200, "duration": 1800, "text": "world" }
                    ]
                },
                {
                    "time": 9000,
                    "duration": 3000,
                    "text": "Second line"
                }
            ]
        });

        let parsed = parse_kpoe_lyrics_json(&kpoe_json).expect("Should parse KPOE JSON");
        assert!(is_valid_synced_lrc(&parsed));
        assert_eq!(parsed.lines.len(), 2);
        assert_eq!(parsed.lines[0].segments.len(), 2);

        // Verify exponential backoff: 30s -> 60s -> 120s -> 240s -> 480s -> capped at 600s (10m)
        assert_eq!(retry_backoff_duration(0), Duration::from_secs(30));
        assert_eq!(retry_backoff_duration(1), Duration::from_secs(60));
        assert_eq!(retry_backoff_duration(2), Duration::from_secs(120));
        assert_eq!(retry_backoff_duration(3), Duration::from_secs(240));
        assert_eq!(retry_backoff_duration(4), Duration::from_secs(480));
        assert_eq!(retry_backoff_duration(5), Duration::from_secs(600));
        assert_eq!(retry_backoff_duration(8), Duration::from_secs(600));
    }
}
