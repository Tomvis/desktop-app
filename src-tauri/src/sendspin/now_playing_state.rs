//! Transport-agnostic accumulator that folds Sendspin protocol messages into a
//! [`NowPlaying`] snapshot.
//!
//! Two protocol facts drive the design:
//! - `server/state` carries the **full** metadata object whenever it includes
//!   one (a progress tick re-sends title/artist too); an absent field means
//!   no value and a `null` object clears all metadata. We REPLACE, so a field
//!   the next track lacks (e.g. artwork) does not linger.
//! - `group/update.playback_state` is the **only** authoritative play/stop
//!   signal. `stream/end` arrives late mid-transition and must not touch
//!   now-playing state.

use crate::now_playing::NowPlaying;
use sendspin::protocol::messages::{GroupUpdate, MetadataState, PlaybackState};

/// Server progress fields are milliseconds; `NowPlaying` is seconds.
const MILLIS_PER_SEC: f64 = 1000.0;

/// Folds protocol messages into a coherent now-playing view.
///
/// `is_playing` is driven exclusively by `group/update`; metadata fields are
/// merged from `server/state` deltas.
pub struct NowPlayingState {
    player_id: String,
    player_name: String,
    is_playing: bool,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    image_url: Option<String>,
    duration: Option<f64>,
    elapsed: Option<f64>,
}

impl NowPlayingState {
    pub fn new(player_id: String, player_name: String) -> Self {
        Self {
            player_id,
            player_name,
            is_playing: false,
            title: None,
            artist: None,
            album: None,
            image_url: None,
            duration: None,
            elapsed: None,
        }
    }

    /// Apply a `group/update`; its `playback_state` is authoritative for
    /// play/stop.
    pub fn apply_group_update(&mut self, gu: &GroupUpdate) {
        self.is_playing = matches!(gu.playback_state, PlaybackState::Playing);
    }

    /// Apply a `server/state` metadata object: `Some` is the complete new
    /// metadata, `None` (a `null` object) clears it.
    pub fn apply_metadata(&mut self, md: Option<&MetadataState>) {
        let Some(md) = md else {
            self.title = None;
            self.artist = None;
            self.album = None;
            self.image_url = None;
            self.elapsed = None;
            self.duration = None;
            return;
        };
        self.title.clone_from(&md.title);
        self.artist.clone_from(&md.artist);
        self.album.clone_from(&md.album);
        self.image_url.clone_from(&md.artwork_url);
        if let Some(p) = &md.progress {
            // Don't crash on negative values
            self.elapsed = Some(p.track_progress.max(0) as f64 / MILLIS_PER_SEC);
            // 0 = live/unknown stream (no finite length). Represent as absent
            // rather than a bogus zero-length track so the UI can show
            // elapsed-only instead of a 0:00/0:00 progress bar.
            self.duration =
                (p.track_duration > 0).then(|| p.track_duration as f64 / MILLIS_PER_SEC);
        } else {
            self.elapsed = None;
            self.duration = None;
        }
    }

    /// Render the current accumulated state as a [`NowPlaying`] for the UI/tray.
    pub fn snapshot(&self) -> NowPlaying {
        NowPlaying {
            is_playing: self.is_playing,
            track: self.title.clone(),
            artist: self.artist.clone(),
            album: self.album.clone(),
            image_url: self.image_url.clone(),
            player_name: Some(self.player_name.clone()),
            player_id: Some(self.player_id.clone()),
            duration: self.duration,
            elapsed: self.elapsed,
            can_play: !self.is_playing,
            can_pause: self.is_playing,
            can_next: true,
            can_previous: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const PLAYER_ID: &str = "player-1";
    const PLAYER_NAME: &str = "Living Room";
    const TITLE: &str = "100 Days, 100 Nights";
    const ARTIST: &str = "Sharon Jones & The Dap-Kings";

    fn state() -> NowPlayingState {
        NowPlayingState::new(PLAYER_ID.to_string(), PLAYER_NAME.to_string())
    }

    fn metadata_from_json(value: serde_json::Value) -> MetadataState {
        serde_json::from_value(value).expect("test metadata JSON should deserialize")
    }

    /// Full metadata object for a track at the given position.
    fn track_at(title: &str, artist: &str, progress_ms: i64, duration_ms: i64) -> MetadataState {
        metadata_from_json(serde_json::json!({
            "timestamp": 0,
            "title": title,
            "artist": artist,
            "progress": {
                "track_progress": progress_ms,
                "track_duration": duration_ms,
                "playback_speed": 1000,
            },
        }))
    }

    /// Metadata object with only `timestamp` and `progress` set.
    fn progress_delta(progress_ms: i64, duration_ms: i64) -> MetadataState {
        metadata_from_json(serde_json::json!({
            "timestamp": 0,
            "progress": {
                "track_progress": progress_ms,
                "track_duration": duration_ms,
                "playback_speed": 1000,
            },
        }))
    }

    fn track_delta(title: &str, artist: &str) -> MetadataState {
        metadata_from_json(serde_json::json!({
            "timestamp": 0,
            "title": title,
            "artist": artist,
        }))
    }

    fn group_update(ps: PlaybackState) -> GroupUpdate {
        GroupUpdate {
            playback_state: ps,
            group_id: "group-1".to_string(),
            group_name: PLAYER_NAME.to_string(),
        }
    }

    #[test]
    fn progress_tick_carries_full_track_and_updates_position() {
        let mut s = state();
        s.apply_metadata(Some(&track_delta(TITLE, ARTIST)));

        let progress_ms = 30_000;
        let duration_ms = 210_000;
        s.apply_metadata(Some(&track_at(TITLE, ARTIST, progress_ms, duration_ms)));

        let snap = s.snapshot();
        assert_eq!(snap.track.as_deref(), Some(TITLE));
        assert_eq!(snap.artist.as_deref(), Some(ARTIST));
        assert_eq!(snap.elapsed, Some(progress_ms as f64 / MILLIS_PER_SEC));
        assert_eq!(snap.duration, Some(duration_ms as f64 / MILLIS_PER_SEC));
    }

    #[test]
    fn live_stream_zero_duration_is_absent() {
        let mut s = state();
        let progress_ms = 45_000;
        s.apply_metadata(Some(&progress_delta(progress_ms, 0)));

        let snap = s.snapshot();
        assert_eq!(snap.elapsed, Some(progress_ms as f64 / MILLIS_PER_SEC));
        assert_eq!(
            snap.duration, None,
            "0 duration is live/unknown, not a zero-length track"
        );
    }

    #[test]
    fn negative_progress_clamps_to_track_start() {
        let mut s = state();
        s.apply_metadata(Some(&progress_delta(-250, 210_000)));

        let snap = s.snapshot();
        assert_eq!(snap.elapsed, Some(0.0));
        assert_eq!(snap.duration, Some(210.0));
    }

    #[test]
    fn group_update_drives_is_playing() {
        let mut s = state();

        s.apply_group_update(&group_update(PlaybackState::Playing));
        let playing = s.snapshot();
        assert!(playing.is_playing);
        assert!(playing.can_pause);
        assert!(!playing.can_play);

        s.apply_group_update(&group_update(PlaybackState::Stopped));
        let stopped = s.snapshot();
        assert!(!stopped.is_playing);
        assert!(!stopped.can_pause);
        assert!(stopped.can_play);
    }

    #[test]
    fn stopped_keeps_last_metadata() {
        let mut s = state();
        s.apply_metadata(Some(&track_delta(TITLE, ARTIST)));
        s.apply_group_update(&group_update(PlaybackState::Playing));
        s.apply_group_update(&group_update(PlaybackState::Stopped));

        let snap = s.snapshot();
        assert!(!snap.is_playing);
        assert_eq!(snap.track.as_deref(), Some(TITLE), "track survives stop");
        assert_eq!(snap.artist.as_deref(), Some(ARTIST));
    }

    #[test]
    fn metadata_delta_while_stopped_does_not_resume() {
        let mut s = state();
        s.apply_group_update(&group_update(PlaybackState::Stopped));
        s.apply_metadata(Some(&track_delta(TITLE, ARTIST)));

        let snap = s.snapshot();
        assert!(!snap.is_playing, "metadata must not flip play state");
        assert_eq!(snap.track.as_deref(), Some(TITLE));
    }

    #[test]
    fn full_object_drops_fields_the_new_track_lacks() {
        let mut s = state();
        let mut with_art = track_delta(TITLE, ARTIST);
        with_art.artwork_url = Some("http://example/art.jpg".to_string());
        with_art.album = Some("Album".to_string());
        s.apply_metadata(Some(&with_art));
        s.apply_metadata(Some(&track_delta("Next", ARTIST)));

        let snap = s.snapshot();
        assert_eq!(snap.track.as_deref(), Some("Next"));
        assert_eq!(snap.image_url, None);
        assert_eq!(snap.album, None);
    }

    #[test]
    fn null_metadata_clears_everything() {
        let mut s = state();
        s.apply_metadata(Some(&track_at(TITLE, ARTIST, 1_000, 2_000)));
        s.apply_metadata(None);

        let snap = s.snapshot();
        assert_eq!(snap.track, None);
        assert_eq!(snap.artist, None);
        assert_eq!(snap.elapsed, None);
        assert_eq!(snap.duration, None);
    }

    #[test]
    fn snapshot_carries_player_identity() {
        let snap = state().snapshot();
        assert_eq!(snap.player_id.as_deref(), Some(PLAYER_ID));
        assert_eq!(snap.player_name.as_deref(), Some(PLAYER_NAME));
        assert!(snap.can_next);
        assert!(snap.can_previous);
    }
}
