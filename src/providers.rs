use crate::config::{FallbackProviderKind, Settings};
use crate::lrc::{LyricLine, LrcMetadata, ParsedLrc, TimedSegment};
use crate::matcher::{normalize, TrackQuery};
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

const MAX_LYRIC_CACHE_ENTRIES: usize = 16;
const MAX_CANDIDATE_LYRIC_FETCHES: usize = 2;

#[derive(Debug, Clone, Default)]
struct RateLimitState {
    last_request: Option<Instant>,
    cooldown_until: Option<Instant>,
}

#[derive(Default)]
struct ProviderCaches {
    mxm_token: Option<String>,
    spotify_token: Option<(String, String, Instant)>, // (sp_dc_source, access_token, valid_until)
    rate_limits: HashMap<FallbackProviderKind, RateLimitState>,
    lyric_cache_order: VecDeque<String>,
    lyric_cache: HashMap<String, ParsedLrc>,
}

impl ProviderCaches {
    fn min_interval(provider: FallbackProviderKind) -> Duration {
        match provider {
            FallbackProviderKind::LrcLib => Duration::from_millis(350),
            FallbackProviderKind::Musixmatch => Duration::from_millis(650),
            FallbackProviderKind::Spotify => Duration::from_millis(450),
            FallbackProviderKind::AppleMusic => Duration::from_millis(450),
        }
    }

    fn is_on_cooldown(&self, provider: FallbackProviderKind) -> bool {
        self.rate_limits
            .get(&provider)
            .and_then(|s| s.cooldown_until)
            .map(|until| Instant::now() < until)
            .unwrap_or(false)
    }

    fn set_cooldown(&mut self, provider: FallbackProviderKind, duration: Duration) {
        let state = self.rate_limits.entry(provider).or_default();
        state.cooldown_until = Some(Instant::now() + duration);
    }

    fn invalidate_provider_token(&mut self, provider: FallbackProviderKind) {
        match provider {
            FallbackProviderKind::Musixmatch => self.mxm_token = None,
            FallbackProviderKind::Spotify => self.spotify_token = None,
            _ => {}
        }
    }

    fn wait_turn(&mut self, provider: FallbackProviderKind) {
        let min_gap = Self::min_interval(provider);
        let state = self.rate_limits.entry(provider).or_default();
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
    }
}

fn build_query_cache_key(query: &TrackQuery, settings: &Settings) -> String {
    format!(
        "{}|{}|{}|{}",
        normalize(&query.joined_artists()),
        normalize(&query.title),
        query.spotify_track_id().unwrap_or_default(),
        settings.prefer_uncensored
    )
}

fn provider_http_get_string(
    agent: &ureq::Agent,
    provider: FallbackProviderKind,
    url: &str,
    headers: &[(&str, &str)],
    caches: &mut ProviderCaches,
) -> Option<String> {
    if caches.is_on_cooldown(provider) {
        return None;
    }
    caches.wait_turn(provider);

    let mut req = agent.get(url);
    for &(k, v) in headers {
        req = req.set(k, v);
    }

    match req.call() {
        Ok(resp) => resp.into_string().ok(),
        Err(ureq::Error::Status(code, resp)) => {
            if code == 429 || code == 503 {
                let default_wait = if code == 429 { 60 } else { 20 };
                let retry_secs = resp
                    .header("Retry-After")
                    .and_then(|h| h.trim().parse::<u64>().ok())
                    .unwrap_or(default_wait)
                    .clamp(5, 600);
                caches.set_cooldown(provider, Duration::from_secs(retry_secs));
            } else if code == 401 || code == 403 {
                caches.invalidate_provider_token(provider);
                caches.set_cooldown(provider, Duration::from_secs(15));
            }
            None
        }
        Err(_) => None,
    }
}

fn provider_http_get_json(
    agent: &ureq::Agent,
    provider: FallbackProviderKind,
    url: &str,
    headers: &[(&str, &str)],
    caches: &mut ProviderCaches,
) -> Option<Value> {
    let body = provider_http_get_string(agent, provider, url, headers, caches)?;
    serde_json::from_str(&body).ok()
}

pub struct FallbackRequest {
    pub query: TrackQuery,
    pub track_length: Option<Duration>,
    pub settings: Settings,
}

pub struct FallbackResponse {
    pub query: TrackQuery,
    pub lrc: Option<ParsedLrc>,
}

/// Asynchronous background worker that queries remote fallback providers in priority order
/// without blocking the TUI rendering loop.
pub struct FallbackWorker {
    tx: Sender<FallbackRequest>,
    rx: Receiver<FallbackResponse>,
}

impl FallbackWorker {
    pub fn new() -> Self {
        let (req_tx, req_rx) = mpsc::channel::<FallbackRequest>();
        let (res_tx, res_rx) = mpsc::channel::<FallbackResponse>();

        thread::spawn(move || {
            let mut caches = ProviderCaches::default();
            while let Ok(mut req) = req_rx.recv() {
                // Drain any queued requests so we only fetch for the latest active song
                while let Ok(newer) = req_rx.try_recv() {
                    req = newer;
                }

                let lrc = fetch_from_fallback_chain(
                    &req.query,
                    req.track_length,
                    &req.settings,
                    &mut caches,
                );
                let _ = res_tx.send(FallbackResponse {
                    query: req.query,
                    lrc,
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

    pub fn poll_response(&self) -> Option<FallbackResponse> {
        let mut latest = None;
        while let Ok(res) = self.rx.try_recv() {
            latest = Some(res);
        }
        latest
    }
}

/// Try each enabled provider in the exact priority order specified in `settings.fallback_priority`.
fn fetch_from_fallback_chain(
    query: &TrackQuery,
    track_length: Option<Duration>,
    settings: &Settings,
    caches: &mut ProviderCaches,
) -> Option<ParsedLrc> {
    if query.title.trim().is_empty() && query.spotify_track_id().is_none() {
        return None;
    }

    let cache_key = build_query_cache_key(query, settings);
    if let Some(cached) = caches.get_cached_lyrics(&cache_key) {
        return Some(cached);
    }

    for provider in settings.enabled_fallback_providers() {
        if caches.is_on_cooldown(provider) {
            continue;
        }

        let result = match provider {
            FallbackProviderKind::LrcLib => fetch_lrclib(query, track_length, settings, caches),
            FallbackProviderKind::Musixmatch => {
                fetch_musixmatch(query, track_length, settings, caches)
            }
            FallbackProviderKind::Spotify => {
                fetch_spotify(query, track_length, settings, caches)
            }
            FallbackProviderKind::AppleMusic => {
                fetch_apple_music(query, track_length, settings, caches)
            }
        };

        if let Some(mut parsed) = result {
            if is_valid_synced_lrc(&parsed) {
                parsed.source_label = Some(format!("Lyrics from {}", provider.display_name()));
                caches.insert_cached_lyrics(cache_key, parsed.clone());
                return Some(parsed);
            }
        }
    }

    None
}

/// Strictly verify that the parsed LRC contains genuine synced timestamps.
pub fn is_valid_synced_lrc(lrc: &ParsedLrc) -> bool {
    if lrc.lines.len() < 2 {
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

/// Score a remote track candidate against the MPRIS `TrackQuery`.
/// Returns a base confidence score in `[0.0, 1.0]`.
pub fn score_remote_metadata(
    query: &TrackQuery,
    cand_title: &str,
    cand_artist: &str,
    cand_album: Option<&str>,
    query_len: Option<Duration>,
    cand_len_secs: Option<f64>,
) -> f64 {
    let q_title = normalize(&query.title);
    let c_title = normalize(cand_title);
    if q_title.is_empty() || c_title.is_empty() {
        return 0.0;
    }

    let title_sim = string_similarity(&q_title, &c_title);

    let q_artist = query.primary_artist().map(normalize).unwrap_or_default();
    let c_artist = normalize(cand_artist);
    let artist_sim = if !q_artist.is_empty() && !c_artist.is_empty() {
        string_similarity(&q_artist, &c_artist)
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
            if diff <= 2.5 {
                score = (score + 0.04).min(1.0);
            } else if diff > 15.0 {
                score *= 0.85;
            }
        }
    }

    score.clamp(0.0, 1.0)
}

/// Evaluate censorship vs uncensored adjustment for candidate ranking.
/// Positive adjustment favors uncensored lyrics; negative penalizes censored (`f**k`, `***`, `clean version`).
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
            // Matches patterns like "f**k", "s**t", "n***a", "b***h", or "****"
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
        .user_agent("TermLRC/0.1.0 (https://github.com/termlrc)")
        .build()
}

// -----------------------------------------------------------------------------
// 1. LRCLIB Provider
// -----------------------------------------------------------------------------

fn fetch_lrclib(
    query: &TrackQuery,
    track_length: Option<Duration>,
    settings: &Settings,
    caches: &mut ProviderCaches,
) -> Option<ParsedLrc> {
    if query.title.trim().is_empty() {
        return None;
    }

    let agent = http_agent();
    let artist = query.primary_artist().unwrap_or("");
    let url = if !artist.is_empty() {
        format!(
            "https://lrclib.net/api/search?track_name={}&artist_name={}",
            urlencoding::encode(&query.title),
            urlencoding::encode(artist)
        )
    } else {
        format!(
            "https://lrclib.net/api/search?q={}",
            urlencoding::encode(&query.title)
        )
    };

    let resp = provider_http_get_json(&agent, FallbackProviderKind::LrcLib, &url, &[], caches)?;
    let items = resp.as_array()?;

    let mut best_rank = f64::MIN;
    let mut best_lrc: Option<ParsedLrc> = None;

    for item in items {
        if item.get("instrumental").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        let Some(synced_str) = item
            .get("syncedLyrics")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        else {
            continue;
        };

        let parsed = ParsedLrc::parse(synced_str);
        if !is_valid_synced_lrc(&parsed) {
            continue;
        }

        let cand_title = item.get("trackName").and_then(|v| v.as_str()).unwrap_or("");
        let cand_artist = item
            .get("artistName")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let cand_album = item.get("albumName").and_then(|v| v.as_str());
        let cand_dur = item.get("duration").and_then(|v| v.as_f64());

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

        let rank = if settings.prefer_uncensored {
            base_score + uncensored_ranking_adjustment(cand_title, cand_album, synced_str, None)
        } else {
            base_score
        };

        if rank > best_rank {
            best_rank = rank;
            best_lrc = Some(parsed);
        }
    }

    best_lrc
}

// -----------------------------------------------------------------------------
// 2. Musixmatch Provider
// -----------------------------------------------------------------------------

fn check_musixmatch_json_rate_limit(json: &Value, caches: &mut ProviderCaches) -> bool {
    let status = json
        .pointer("/message/header/status_code")
        .and_then(|v| v.as_i64())
        .unwrap_or(200);
    if status == 401 || status == 402 || status == 429 {
        // Musixmatch returns 401/402 inside JSON when a usertoken is expired or rate-limited
        caches.invalidate_provider_token(FallbackProviderKind::Musixmatch);
        caches.set_cooldown(FallbackProviderKind::Musixmatch, Duration::from_secs(45));
        return true;
    }
    false
}

fn fetch_musixmatch(
    query: &TrackQuery,
    track_length: Option<Duration>,
    settings: &Settings,
    caches: &mut ProviderCaches,
) -> Option<ParsedLrc> {
    if query.title.trim().is_empty() {
        return None;
    }

    let agent = http_agent();
    let token = if !settings.musixmatch_token.trim().is_empty() {
        settings.musixmatch_token.trim().to_string()
    } else if let Some(ref t) = caches.mxm_token {
        t.clone()
    } else {
        let fetched = obtain_musixmatch_token(&agent, caches)?;
        caches.mxm_token = Some(fetched.clone());
        fetched
    };

    let artist = query.primary_artist().unwrap_or("");
    let search_url = format!(
        "https://apic-desktop.musixmatch.com/ws/1.1/track.search?app_id=web-desktop-app-v1.0&format=json&q_track={}&q_artist={}&f_has_subtitle=1&s_track_rating=desc&page_size=5&usertoken={}",
        urlencoding::encode(&query.title),
        urlencoding::encode(artist),
        urlencoding::encode(&token)
    );

    let search_json = provider_http_get_json(
        &agent,
        FallbackProviderKind::Musixmatch,
        &search_url,
        &[("Cookie", "x-mxm-token-guid=termlrc")],
        caches,
    )?;

    if check_musixmatch_json_rate_limit(&search_json, caches) {
        return None;
    }

    let track_list = search_json
        .pointer("/message/body/track_list")
        .and_then(|v| v.as_array())?;

    struct MxmCand {
        track_id: i64,
        title: String,
        album: Option<String>,
        is_explicit: Option<bool>,
        base_score: f64,
        pre_rank: f64,
    }

    let mut candidates: Vec<MxmCand> = Vec::new();
    for entry in track_list {
        let Some(track) = entry.get("track") else {
            continue;
        };
        if track.get("has_subtitles").and_then(|v| v.as_i64()) != Some(1) {
            continue;
        }
        let Some(track_id) = track.get("track_id").and_then(|v| v.as_i64()) else {
            continue;
        };
        let cand_title = track
            .get("track_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let cand_artist = track
            .get("artist_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let cand_album = track.get("album_name").and_then(|v| v.as_str());
        let cand_len = track.get("track_length").and_then(|v| v.as_f64());
        let is_explicit = track
            .get("explicit")
            .and_then(|v| v.as_i64())
            .map(|v| v == 1);

        let base_score = score_remote_metadata(
            query,
            cand_title,
            cand_artist,
            cand_album,
            track_length,
            cand_len,
        );
        if base_score < settings.fallback_match_threshold {
            continue;
        }

        let pre_rank = if settings.prefer_uncensored {
            base_score + uncensored_ranking_adjustment(cand_title, cand_album, "", is_explicit)
        } else {
            base_score
        };

        candidates.push(MxmCand {
            track_id,
            title: cand_title.to_string(),
            album: cand_album.map(|s| s.to_string()),
            is_explicit,
            base_score,
            pre_rank,
        });
    }

    candidates.sort_by(|a, b| {
        b.pre_rank
            .partial_cmp(&a.pre_rank)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut best_rank = f64::MIN;
    let mut best_lrc: Option<ParsedLrc> = None;

    for cand in candidates.into_iter().take(MAX_CANDIDATE_LYRIC_FETCHES) {
        let sub_url = format!(
            "https://apic-desktop.musixmatch.com/ws/1.1/track.subtitle.get?app_id=web-desktop-app-v1.0&format=json&track_id={}&subtitle_format=lrc&usertoken={}",
            cand.track_id,
            urlencoding::encode(&token)
        );
        let Some(sub_json) = provider_http_get_json(
            &agent,
            FallbackProviderKind::Musixmatch,
            &sub_url,
            &[("Cookie", "x-mxm-token-guid=termlrc")],
            caches,
        ) else {
            if caches.is_on_cooldown(FallbackProviderKind::Musixmatch) {
                break;
            }
            continue;
        };

        if check_musixmatch_json_rate_limit(&sub_json, caches) {
            break;
        }

        let Some(lrc_body) = sub_json
            .pointer("/message/body/subtitle/subtitle_body")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        else {
            continue;
        };

        let parsed = ParsedLrc::parse(lrc_body);
        if !is_valid_synced_lrc(&parsed) {
            continue;
        }

        let is_censored = count_censored_patterns(lrc_body) > 0;
        let rank = if settings.prefer_uncensored {
            cand.base_score
                + uncensored_ranking_adjustment(
                    &cand.title,
                    cand.album.as_deref(),
                    lrc_body,
                    cand.is_explicit,
                )
        } else {
            cand.base_score
        };

        if rank > best_rank {
            best_rank = rank;
            best_lrc = Some(parsed);
        }

        // Early exit if the top candidate is synced and uncensored
        if !settings.prefer_uncensored || !is_censored {
            break;
        }
    }

    best_lrc
}

fn obtain_musixmatch_token(agent: &ureq::Agent, caches: &mut ProviderCaches) -> Option<String> {
    let url = "https://apic-desktop.musixmatch.com/ws/1.1/token.get?app_id=web-desktop-app-v1.0&format=json";
    let json = provider_http_get_json(
        agent,
        FallbackProviderKind::Musixmatch,
        url,
        &[("Cookie", "x-mxm-token-guid=termlrc")],
        caches,
    )?;
    if check_musixmatch_json_rate_limit(&json, caches) {
        return None;
    }
    json.pointer("/message/body/user_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != "UpgradeOnlyUpgradeOnlyUpgradeOnlyUpgradeOnly")
        .map(|s| s.to_string())
}

// -----------------------------------------------------------------------------
// 3. Spotify Synced Color-Lyrics Provider
// -----------------------------------------------------------------------------

fn fetch_spotify(
    query: &TrackQuery,
    track_length: Option<Duration>,
    settings: &Settings,
    caches: &mut ProviderCaches,
) -> Option<ParsedLrc> {
    let agent = http_agent();
    let direct_track_id = query.spotify_track_id();

    // Option A: Custom spotify-lyrics-api endpoint URL if configured
    let api_base = settings.spotify_api_url.trim().trim_end_matches('/');
    if !api_base.is_empty() {
        // 1. If MPRIS provided the exact Spotify Track ID (via xesam:url or mpris:trackid),
        //    query ?trackid=<id> directly (standard akashrchandran/spotify-lyrics-api format).
        if let Some(ref tid) = direct_track_id {
            let tid_url = format!("{}?trackid={}", api_base, urlencoding::encode(tid));
            if let Some(parsed) = fetch_spotify_proxy_endpoint(&agent, &tid_url, caches) {
                return Some(parsed);
            }
        }

        // 2. Also try ?title=<title>&artist=<artist> for search-enabled proxy endpoints.
        if !query.title.trim().is_empty() && !caches.is_on_cooldown(FallbackProviderKind::Spotify) {
            let artist = query.primary_artist().unwrap_or("");
            let search_url = format!(
                "{}?title={}&artist={}",
                api_base,
                urlencoding::encode(&query.title),
                urlencoding::encode(artist)
            );
            if let Some(parsed) = fetch_spotify_proxy_endpoint(&agent, &search_url, caches) {
                return Some(parsed);
            }
        }
    }

    // Option B: Direct Spotify token / sp_dc -> color-lyrics/v2 (with token caching & direct MPRIS track ID lookup)
    let access_token = obtain_spotify_access_token(&agent, settings, caches)?;

    // Fast path: if MPRIS gave us the exact Spotify Track ID, fetch color-lyrics directly
    // without needing a search round-trip.
    if let Some(ref tid) = direct_track_id {
        if let Some(parsed) = fetch_spotify_color_lyrics_by_id(&agent, &access_token, tid, caches) {
            return Some(parsed);
        }
    }

    if query.title.trim().is_empty() || caches.is_on_cooldown(FallbackProviderKind::Spotify) {
        return None;
    }

    let artist = query.primary_artist().unwrap_or("");
    let search_q = if artist.is_empty() {
        query.title.clone()
    } else {
        format!("track:{} artist:{}", query.title, artist)
    };
    let search_url = format!(
        "https://api.spotify.com/v1/search?type=track&limit=5&q={}",
        urlencoding::encode(&search_q)
    );
    let auth_header = format!("Bearer {}", access_token);
    let search_json = provider_http_get_json(
        &agent,
        FallbackProviderKind::Spotify,
        &search_url,
        &[("Authorization", &auth_header)],
        caches,
    )?;

    let items = search_json
        .pointer("/tracks/items")
        .and_then(|v| v.as_array())?;

    struct SpCand {
        track_id: String,
        title: String,
        album: Option<String>,
        is_explicit: Option<bool>,
        base_score: f64,
        pre_rank: f64,
    }

    let mut candidates: Vec<SpCand> = Vec::new();
    for item in items {
        let Some(track_id) = item.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let cand_title = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let cand_artist = item
            .pointer("/artists/0/name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let cand_album = item.pointer("/album/name").and_then(|v| v.as_str());
        let cand_len_secs = item
            .get("duration_ms")
            .and_then(|v| v.as_f64())
            .map(|ms| ms / 1000.0);
        let is_explicit = item.get("explicit").and_then(|v| v.as_bool());

        let base_score = score_remote_metadata(
            query,
            cand_title,
            cand_artist,
            cand_album,
            track_length,
            cand_len_secs,
        );
        if base_score < settings.fallback_match_threshold {
            continue;
        }

        let pre_rank = if settings.prefer_uncensored {
            base_score + uncensored_ranking_adjustment(cand_title, cand_album, "", is_explicit)
        } else {
            base_score
        };

        candidates.push(SpCand {
            track_id: track_id.to_string(),
            title: cand_title.to_string(),
            album: cand_album.map(|s| s.to_string()),
            is_explicit,
            base_score,
            pre_rank,
        });
    }

    candidates.sort_by(|a, b| {
        b.pre_rank
            .partial_cmp(&a.pre_rank)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut best_rank = f64::MIN;
    let mut best_lrc: Option<ParsedLrc> = None;

    for cand in candidates.into_iter().take(MAX_CANDIDATE_LYRIC_FETCHES) {
        if caches.is_on_cooldown(FallbackProviderKind::Spotify) {
            break;
        }

        let parsed_opt = if !api_base.is_empty() {
            let tid_url = format!("{}?trackid={}", api_base, urlencoding::encode(&cand.track_id));
            fetch_spotify_proxy_endpoint(&agent, &tid_url, caches).or_else(|| {
                fetch_spotify_color_lyrics_by_id(&agent, &access_token, &cand.track_id, caches)
            })
        } else {
            fetch_spotify_color_lyrics_by_id(&agent, &access_token, &cand.track_id, caches)
        };

        let Some(parsed) = parsed_opt else {
            continue;
        };

        let joined_text: String = parsed
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let is_censored = count_censored_patterns(&joined_text) > 0;
        let rank = if settings.prefer_uncensored {
            cand.base_score
                + uncensored_ranking_adjustment(
                    &cand.title,
                    cand.album.as_deref(),
                    &joined_text,
                    cand.is_explicit,
                )
        } else {
            cand.base_score
        };

        if rank > best_rank {
            best_rank = rank;
            best_lrc = Some(parsed);
        }

        if !settings.prefer_uncensored || !is_censored {
            break;
        }
    }

    best_lrc
}

fn fetch_spotify_proxy_endpoint(
    agent: &ureq::Agent,
    url: &str,
    caches: &mut ProviderCaches,
) -> Option<ParsedLrc> {
    let body = provider_http_get_string(agent, FallbackProviderKind::Spotify, url, &[], caches)?;
    if let Ok(json) = serde_json::from_str::<Value>(&body) {
        if json.get("error").and_then(|v| v.as_bool()) == Some(true) {
            return None;
        }
        if let Some(parsed) = parse_spotify_lyrics_json(&json) {
            if is_valid_synced_lrc(&parsed) {
                return Some(parsed);
            }
        }
    }
    let parsed_lrc = ParsedLrc::parse(&body);
    if is_valid_synced_lrc(&parsed_lrc) {
        return Some(parsed_lrc);
    }
    None
}

fn fetch_spotify_color_lyrics_by_id(
    agent: &ureq::Agent,
    access_token: &str,
    track_id: &str,
    caches: &mut ProviderCaches,
) -> Option<ParsedLrc> {
    let lyrics_url = format!(
        "https://spclient.wg.spotify.com/color-lyrics/v2/track/{}?format=json&vocalRemoval=false",
        track_id
    );
    let auth_header = format!("Bearer {}", access_token);
    let lyr_json = provider_http_get_json(
        agent,
        FallbackProviderKind::Spotify,
        &lyrics_url,
        &[
            ("Authorization", &auth_header),
            ("App-Platform", "WebPlayer"),
        ],
        caches,
    )?;
    let parsed = parse_spotify_lyrics_json(&lyr_json)?;
    if is_valid_synced_lrc(&parsed) {
        Some(parsed)
    } else {
        None
    }
}

fn obtain_spotify_access_token(
    agent: &ureq::Agent,
    settings: &Settings,
    caches: &mut ProviderCaches,
) -> Option<String> {
    let raw = settings.spotify_sp_dc.trim();
    if raw.is_empty() {
        return None;
    }

    // Allow passing a direct Bearer token if prefixed with "Bearer "
    if let Some(bearer) = raw.strip_prefix("Bearer ").map(str::trim) {
        if !bearer.is_empty() {
            return Some(bearer.to_string());
        }
    }

    let sp_dc = raw.strip_prefix("sp_dc=").unwrap_or(raw).trim();
    let now = Instant::now();
    if let Some((ref cached_source, ref token, valid_until)) = caches.spotify_token {
        if cached_source == sp_dc && now < valid_until {
            return Some(token.clone());
        }
    }

    let cookie_header = format!("sp_dc={}", sp_dc);
    let token_json = provider_http_get_json(
        agent,
        FallbackProviderKind::Spotify,
        "https://open.spotify.com/get_access_token?reason=transport&productType=web_player",
        &[("Cookie", &cookie_header)],
        caches,
    )?;
    let access_token = token_json.get("accessToken")?.as_str()?.to_string();
    // Cache token for 15 minutes
    caches.spotify_token = Some((
        sp_dc.to_string(),
        access_token.clone(),
        Instant::now() + Duration::from_secs(900),
    ));
    Some(access_token)
}

pub fn parse_spotify_lyrics_json(json: &Value) -> Option<ParsedLrc> {
    let container = json.get("lyrics").unwrap_or(json);
    let sync_type = container
        .get("syncType")
        .or_else(|| json.get("syncType"))
        .and_then(|v| v.as_str())
        .unwrap_or("LINE_SYNCED");

    // Strictly reject unsynced Spotify lyrics
    if sync_type.eq_ignore_ascii_case("UNSYNCED") {
        return None;
    }

    let lines_arr = container
        .get("lines")
        .or_else(|| json.get("lines"))
        .and_then(|v| v.as_array())?;

    let mut lines = Vec::with_capacity(lines_arr.len());
    for item in lines_arr {
        let start_ms: u64 = item
            .get("startTimeMs")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })?;
        let words = item
            .get("words")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        lines.push(LyricLine {
            timestamp: Duration::from_millis(start_ms),
            text: words,
            segments: Vec::new(),
        });
    }

    lines.sort_by_key(|l| l.timestamp);
    Some(ParsedLrc {
        metadata: LrcMetadata::default(),
        lines,
        source_label: None,
    })
}

// -----------------------------------------------------------------------------
// 4. Apple Music (Syllable / TTML / LRC) Provider
// -----------------------------------------------------------------------------

fn fetch_apple_music(
    query: &TrackQuery,
    track_length: Option<Duration>,
    settings: &Settings,
    caches: &mut ProviderCaches,
) -> Option<ParsedLrc> {
    if query.title.trim().is_empty() {
        return None;
    }

    let agent = http_agent();

    // Option A: Custom Apple Music LRC/TTML proxy endpoint if configured
    if !settings.apple_music_api_url.trim().is_empty() {
        let base = settings.apple_music_api_url.trim().trim_end_matches('/');
        let artist = query.primary_artist().unwrap_or("");
        let url = format!(
            "{}?title={}&artist={}",
            base,
            urlencoding::encode(&query.title),
            urlencoding::encode(artist)
        );
        if let Some(body) =
            provider_http_get_string(&agent, FallbackProviderKind::AppleMusic, &url, &[], caches)
        {
            if let Some(parsed) = parse_apple_music_payload(&body) {
                if is_valid_synced_lrc(&parsed) {
                    return Some(parsed);
                }
            }
        }
    }

    // Option B: Direct Apple Music AMP API with Bearer + Media-User-Token
    let bearer = settings.apple_music_bearer_token.trim();
    let media_token = settings.apple_music_media_user_token.trim();
    if bearer.is_empty()
        || media_token.is_empty()
        || caches.is_on_cooldown(FallbackProviderKind::AppleMusic)
    {
        return None;
    }

    let artist = query.primary_artist().unwrap_or("");
    let term = if artist.is_empty() {
        query.title.clone()
    } else {
        format!("{} {}", artist, query.title)
    };
    let search_url = format!(
        "https://amp-api.music.apple.com/v1/catalog/us/search?types=songs&limit=5&term={}",
        urlencoding::encode(&term)
    );

    let auth_header = format!("Bearer {}", bearer);
    let am_headers = [
        ("Authorization", auth_header.as_str()),
        ("Media-User-Token", media_token),
        ("Origin", "https://music.apple.com"),
    ];

    let search_json = provider_http_get_json(
        &agent,
        FallbackProviderKind::AppleMusic,
        &search_url,
        &am_headers,
        caches,
    )?;

    let songs = search_json
        .pointer("/results/songs/data")
        .and_then(|v| v.as_array())?;

    struct AmCand {
        song_id: String,
        title: String,
        album: Option<String>,
        is_explicit: Option<bool>,
        base_score: f64,
        pre_rank: f64,
    }

    let mut candidates: Vec<AmCand> = Vec::new();
    for song in songs {
        let Some(song_id) = song.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(attrs) = song.get("attributes") else {
            continue;
        };
        let cand_title = attrs.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let cand_artist = attrs
            .get("artistName")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let cand_album = attrs.get("albumName").and_then(|v| v.as_str());
        let cand_len_secs = attrs
            .get("durationInMillis")
            .and_then(|v| v.as_f64())
            .map(|ms| ms / 1000.0);
        let is_explicit = attrs
            .get("contentRating")
            .and_then(|v| v.as_str())
            .map(|r| r.eq_ignore_ascii_case("explicit"));

        let base_score = score_remote_metadata(
            query,
            cand_title,
            cand_artist,
            cand_album,
            track_length,
            cand_len_secs,
        );
        if base_score < settings.fallback_match_threshold {
            continue;
        }

        let pre_rank = if settings.prefer_uncensored {
            base_score + uncensored_ranking_adjustment(cand_title, cand_album, "", is_explicit)
        } else {
            base_score
        };

        candidates.push(AmCand {
            song_id: song_id.to_string(),
            title: cand_title.to_string(),
            album: cand_album.map(|s| s.to_string()),
            is_explicit,
            base_score,
            pre_rank,
        });
    }

    candidates.sort_by(|a, b| {
        b.pre_rank
            .partial_cmp(&a.pre_rank)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut best_rank = f64::MIN;
    let mut best_lrc: Option<ParsedLrc> = None;

    for cand in candidates.into_iter().take(MAX_CANDIDATE_LYRIC_FETCHES) {
        if caches.is_on_cooldown(FallbackProviderKind::AppleMusic) {
            break;
        }

        let lyrics_url = format!(
            "https://amp-api.music.apple.com/v1/catalog/us/songs/{}/syllable-lyrics",
            cand.song_id
        );
        let Some(body) = provider_http_get_string(
            &agent,
            FallbackProviderKind::AppleMusic,
            &lyrics_url,
            &am_headers,
            caches,
        ) else {
            continue;
        };
        let Some(parsed) = parse_apple_music_payload(&body) else {
            continue;
        };
        if !is_valid_synced_lrc(&parsed) {
            continue;
        }

        let joined_text: String = parsed
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let is_censored = count_censored_patterns(&joined_text) > 0;
        let rank = if settings.prefer_uncensored {
            cand.base_score
                + uncensored_ranking_adjustment(
                    &cand.title,
                    cand.album.as_deref(),
                    &joined_text,
                    cand.is_explicit,
                )
        } else {
            cand.base_score
        };

        if rank > best_rank {
            best_rank = rank;
            best_lrc = Some(parsed);
        }

        if !settings.prefer_uncensored || !is_censored {
            break;
        }
    }

    best_lrc
}

pub fn parse_apple_music_payload(body: &str) -> Option<ParsedLrc> {
    if let Ok(json) = serde_json::from_str::<Value>(body) {
        if let Some(ttml) = json
            .pointer("/data/0/attributes/ttml")
            .or_else(|| json.get("ttml"))
            .and_then(|v| v.as_str())
        {
            return parse_ttml_synced_lyrics(ttml);
        }
        if let Some(lrc_str) = json.get("lrc").and_then(|v| v.as_str()) {
            let parsed = ParsedLrc::parse(lrc_str);
            if is_valid_synced_lrc(&parsed) {
                return Some(parsed);
            }
        }
    }

    if body.contains("<tt") || body.contains("<p ") {
        return parse_ttml_synced_lyrics(body);
    }

    let parsed = ParsedLrc::parse(body);
    if is_valid_synced_lrc(&parsed) {
        Some(parsed)
    } else {
        None
    }
}

/// Parse Apple Music synced TTML (`<p begin="00:12.34" ...>`) into `ParsedLrc`.
pub fn parse_ttml_synced_lyrics(ttml: &str) -> Option<ParsedLrc> {
    let mut lines = Vec::new();
    let mut rest = ttml;

    while let Some(p_start) = rest.find("<p ") {
        let after_p = &rest[p_start + 3..];
        let Some(p_tag_end) = after_p.find('>') else {
            break;
        };
        let attrs = &after_p[..p_tag_end];
        let body_start = &after_p[p_tag_end + 1..];
        let Some(p_close) = body_start.find("</p>") else {
            break;
        };
        let p_inner = &body_start[..p_close];
        rest = &body_start[p_close + 4..];

        let Some(begin_str) = extract_xml_attr(attrs, "begin") else {
            continue;
        };
        let Some(timestamp) = parse_ttml_timestamp(begin_str) else {
            continue;
        };

        let (text, segments) = extract_ttml_text_and_segments(p_inner);
        if !text.is_empty() {
            lines.push(LyricLine {
                timestamp,
                text,
                segments,
            });
        }
    }

    lines.sort_by_key(|l| l.timestamp);
    let parsed = ParsedLrc {
        metadata: LrcMetadata::default(),
        lines,
        source_label: None,
    };
    if is_valid_synced_lrc(&parsed) {
        Some(parsed)
    } else {
        None
    }
}

fn extract_ttml_text_and_segments(p_inner: &str) -> (String, Vec<TimedSegment>) {
    let mut full_text = String::new();
    let mut segments = Vec::new();
    let mut in_tag = false;
    let mut tag_buf = String::new();
    let mut current_span_ts: Option<Duration> = None;
    let mut current_span_text = String::new();

    for ch in p_inner.chars() {
        if ch == '<' {
            if let Some(ts) = current_span_ts.take() {
                if !current_span_text.is_empty() {
                    segments.push(TimedSegment {
                        timestamp: ts,
                        text: std::mem::take(&mut current_span_text),
                    });
                }
            }
            in_tag = true;
            tag_buf.clear();
        } else if ch == '>' {
            in_tag = false;
            if tag_buf.starts_with("span ") {
                if let Some(b) = extract_xml_attr(&tag_buf, "begin") {
                    current_span_ts = parse_ttml_timestamp(b);
                }
            }
        } else if in_tag {
            tag_buf.push(ch);
        } else {
            full_text.push(ch);
            if current_span_ts.is_some() {
                current_span_text.push(ch);
            }
        }
    }

    if let Some(ts) = current_span_ts.take() {
        if !current_span_text.is_empty() {
            segments.push(TimedSegment {
                timestamp: ts,
                text: current_span_text,
            });
        }
    }

    let decoded = full_text
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&amp;", "&");
    (decoded.trim().to_string(), segments)
}

fn extract_xml_attr<'a>(attrs: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("{}=\"", key);
    let start = attrs.find(&pattern)? + pattern.len();
    let rest = &attrs[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn parse_ttml_timestamp(s: &str) -> Option<Duration> {
    let s = s.trim().trim_end_matches('s');
    let parts: Vec<&str> = s.split(':').collect();
    let secs_f64: f64 = match parts.len() {
        1 => parts[0].parse().ok()?,
        2 => {
            let mins: f64 = parts[0].parse().ok()?;
            let secs: f64 = parts[1].parse().ok()?;
            mins * 60.0 + secs
        }
        3 => {
            let hrs: f64 = parts[0].parse().ok()?;
            let mins: f64 = parts[1].parse().ok()?;
            let secs: f64 = parts[2].parse().ok()?;
            hrs * 3600.0 + mins * 60.0 + secs
        }
        _ => return None,
    };
    if secs_f64 >= 0.0 {
        Some(Duration::from_secs_f64(secs_f64))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reject_unsynced_and_prioritise_uncensored() {
        let unsynced = ParsedLrc::parse("Just plain lyrics without timestamps\nSecond line");
        assert!(!is_valid_synced_lrc(&unsynced));

        let synced = ParsedLrc::parse("[00:05.00]First line\n[00:10.00]Second line");
        assert!(is_valid_synced_lrc(&synced));

        let censored_adj = uncensored_ranking_adjustment(
            "Song Name (Clean Version)",
            None,
            "[00:05.00]Y'all get the s**t, f**k them",
            Some(false),
        );
        let uncensored_adj = uncensored_ranking_adjustment(
            "Song Name",
            None,
            "[00:05.00]Y'all get the shit, fuck them",
            Some(true),
        );
        assert!(
            uncensored_adj > censored_adj,
            "Uncensored lyrics must rank higher than censored lyrics"
        );
        assert!(censored_adj < 0.0);
        assert!(uncensored_adj > 0.0);
    }

    #[test]
    fn test_spotify_and_apple_ttml_synced_parsers() {
        let unsynced_spotify = serde_json::json!({
            "lyrics": {
                "syncType": "UNSYNCED",
                "lines": [
                    { "startTimeMs": "0", "words": "Hello" },
                    { "startTimeMs": "0", "words": "World" }
                ]
            }
        });
        assert!(parse_spotify_lyrics_json(&unsynced_spotify).is_none());

        let synced_spotify = serde_json::json!({
            "lyrics": {
                "syncType": "LINE_SYNCED",
                "lines": [
                    { "startTimeMs": "1200", "words": "First synced line" },
                    { "startTimeMs": "4500", "words": "Second synced line" }
                ]
            }
        });
        let sp_parsed = parse_spotify_lyrics_json(&synced_spotify).expect("Should parse synced Spotify JSON");
        assert!(is_valid_synced_lrc(&sp_parsed));
        assert_eq!(sp_parsed.lines.len(), 2);

        let sample_ttml = r#"
            <tt><body><div>
                <p begin="00:02.50" end="00:05.00"><span begin="00:02.50">Hello </span><span begin="00:03.20">world</span></p>
                <p begin="00:06.00" end="00:09.00">Second verse</p>
            </div></body></tt>
        "#;
        let am_parsed = parse_ttml_synced_lyrics(sample_ttml).expect("Should parse synced Apple Music TTML");
        assert_eq!(am_parsed.lines.len(), 2);
        assert_eq!(am_parsed.lines[0].text, "Hello world");
        assert_eq!(am_parsed.lines[0].segments.len(), 2);
    }

    #[test]
    fn test_provider_rate_limits_and_session_cache() {
        let mut caches = ProviderCaches::default();
        assert!(!caches.is_on_cooldown(FallbackProviderKind::Musixmatch));

        caches.mxm_token = Some("valid_token".to_string());
        let mxm_rate_limited = serde_json::json!({
            "message": {
                "header": {
                    "status_code": 401,
                    "hint": "captcha"
                }
            }
        });
        assert!(check_musixmatch_json_rate_limit(&mxm_rate_limited, &mut caches));
        assert!(caches.mxm_token.is_none());
        assert!(caches.is_on_cooldown(FallbackProviderKind::Musixmatch));

        let synced = ParsedLrc::parse("[00:05.00]Line A\n[00:10.00]Line B");
        caches.insert_cached_lyrics("artist|song||true".to_string(), synced);
        assert!(caches.get_cached_lyrics("artist|song||true").is_some());
    }
}

