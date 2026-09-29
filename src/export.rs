//! Exporting tracklists in formats that outlive Spotify.
//!
//! The point is portability: a playlist you can only read inside one service
//! is not really yours. All three formats are written from local data, so they
//! work offline and against snapshots of playlists that no longer exist.
//!
//! * **M3U8** — the de-facto playlist interchange format. Media players expect
//!   local paths here; streaming tracks have none, so each entry points at the
//!   track's web URL. Players that cannot resolve it still show the `#EXTINF`
//!   metadata, which is what makes the file useful for re-finding the music
//!   somewhere else.
//! * **CSV** — RFC 4180, for spreadsheets and for re-importing elsewhere.
//! * **JSON** — lossless, for scripting.

use crate::domain::Track;
use serde_json::json;

/// One row of an export. Built from a live playlist, a snapshot, or a run.
#[derive(Debug, Clone)]
pub struct ExportTrack {
    pub position: usize,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub duration_ms: u32,
    pub track_id: String,
    pub isrc: Option<String>,
    pub popularity: Option<u8>,
    /// Why the agent chose it, when this came from a generated run.
    pub reason: Option<String>,
    pub mood: Option<String>,
}

impl ExportTrack {
    pub fn from_track(position: usize, track: &Track) -> Self {
        Self {
            position,
            artist: track.artist_line(),
            title: track.name.clone(),
            album: track.album.clone(),
            duration_ms: track.duration_ms,
            track_id: track.id.clone(),
            isrc: track.isrc.clone(),
            popularity: Some(track.popularity),
            reason: None,
            mood: None,
        }
    }

    pub fn uri(&self) -> String {
        format!("spotify:track:{}", self.track_id)
    }

    pub fn url(&self) -> String {
        format!("https://open.spotify.com/track/{}", self.track_id)
    }

    fn duration_secs(&self) -> i64 {
        // M3U8 uses whole seconds and -1 for unknown.
        if self.duration_ms == 0 {
            -1
        } else {
            i64::from(self.duration_ms / 1000)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    M3u8,
    Csv,
    Json,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Self::M3u8 => "m3u8",
            Self::Csv => "csv",
            Self::Json => "json",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "m3u8" | "m3u" => Some(Self::M3u8),
            "csv" => Some(Self::Csv),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

pub fn render(format: Format, name: &str, tracks: &[ExportTrack]) -> String {
    match format {
        Format::M3u8 => m3u8(name, tracks),
        Format::Csv => csv(tracks),
        Format::Json => json_doc(name, tracks),
    }
}

/// Extended M3U. UTF-8 without a BOM, LF endings — the `.m3u8` contract.
fn m3u8(name: &str, tracks: &[ExportTrack]) -> String {
    let mut out = String::with_capacity(tracks.len() * 120);
    out.push_str("#EXTM3U\n");
    // Not every player reads #PLAYLIST, but the ones that do show the name.
    out.push_str(&format!("#PLAYLIST:{}\n", sanitize_line(name)));

    for track in tracks {
        out.push_str(&format!(
            "#EXTINF:{},{} - {}\n",
            track.duration_secs(),
            sanitize_line(&track.artist),
            sanitize_line(&track.title)
        ));
        if !track.album.is_empty() {
            out.push_str(&format!("#EXTALB:{}\n", sanitize_line(&track.album)));
        }
        if let Some(reason) = &track.reason {
            if !reason.trim().is_empty() {
                // A comment, so players ignore it but the rationale survives
                // in the exported file.
                out.push_str(&format!("# why: {}\n", sanitize_line(reason)));
            }
        }
        out.push_str(&track.url());
        out.push('\n');
    }
    out
}

/// Newlines in a title would terminate the directive early and corrupt the
/// file, so they are flattened.
fn sanitize_line(value: &str) -> String {
    value.replace(['\n', '\r'], " ").trim().to_string()
}

/// RFC 4180: comma-separated, CRLF line endings, `"` doubled inside quotes.
fn csv(tracks: &[ExportTrack]) -> String {
    const HEADERS: &[&str] = &[
        "position",
        "artist",
        "title",
        "album",
        "duration_ms",
        "duration",
        "spotify_id",
        "spotify_uri",
        "url",
        "isrc",
        "popularity",
        "mood",
        "reason",
    ];

    let mut out = String::with_capacity(tracks.len() * 160);
    out.push_str(&HEADERS.join(","));
    out.push_str("\r\n");

    for track in tracks {
        let seconds = track.duration_ms / 1000;
        let fields = [
            track.position.to_string(),
            track.artist.clone(),
            track.title.clone(),
            track.album.clone(),
            track.duration_ms.to_string(),
            format!("{}:{:02}", seconds / 60, seconds % 60),
            track.track_id.clone(),
            track.uri(),
            track.url(),
            track.isrc.clone().unwrap_or_default(),
            track.popularity.map(|p| p.to_string()).unwrap_or_default(),
            track.mood.clone().unwrap_or_default(),
            track.reason.clone().unwrap_or_default(),
        ];
        out.push_str(
            &fields
                .iter()
                .map(|f| csv_field(f))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push_str("\r\n");
    }
    out
}

/// Quote when the value contains a delimiter, a quote, or a line break.
fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn json_doc(name: &str, tracks: &[ExportTrack]) -> String {
    let value = json!({
        "playlist": name,
        "exported_at": chrono::Utc::now().to_rfc3339(),
        "generator": format!("spotify-agent {}", crate::VERSION),
        "count": tracks.len(),
        "tracks": tracks.iter().map(|t| json!({
            "position": t.position,
            "artist": t.artist,
            "title": t.title,
            "album": t.album,
            "duration_ms": t.duration_ms,
            "spotify_id": t.track_id,
            "spotify_uri": t.uri(),
            "url": t.url(),
            "isrc": t.isrc,
            "popularity": t.popularity,
            "mood": t.mood,
            "reason": t.reason,
        })).collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into())
}

/// Filesystem-safe file stem derived from a playlist name.
pub fn safe_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            // Reserved on Windows, awkward everywhere else.
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '-'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        "playlist".to_string()
    } else {
        trimmed.chars().take(80).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(position: usize, artist: &str, title: &str) -> ExportTrack {
        ExportTrack {
            position,
            artist: artist.into(),
            title: title.into(),
            album: "Album".into(),
            duration_ms: 215_000,
            track_id: format!("id{position}"),
            isrc: Some("GBAAA0000001".into()),
            popularity: Some(42),
            reason: Some("because it fits".into()),
            mood: Some("brooding".into()),
        }
    }

    #[test]
    fn m3u8_has_the_required_header_and_one_entry_per_track() {
        let out = m3u8("My List", &[track(1, "A", "B"), track(2, "C", "D")]);
        assert!(out.starts_with("#EXTM3U\n"));
        assert!(out.contains("#PLAYLIST:My List"));
        assert_eq!(out.matches("#EXTINF:").count(), 2);
        assert!(out.contains("#EXTINF:215,A - B"));
        assert!(out.contains("https://open.spotify.com/track/id1"));
    }

    #[test]
    fn m3u8_flattens_newlines_that_would_corrupt_a_directive() {
        let mut t = track(1, "A", "Title\nwith break");
        t.reason = Some("line\r\nbreak".into());
        let out = m3u8("name", &[t]);
        // Exactly one EXTINF line, and no stray bare line inside it.
        let extinf: Vec<&str> = out.lines().filter(|l| l.starts_with("#EXTINF")).collect();
        assert_eq!(extinf.len(), 1);
        assert!(extinf[0].contains("Title with break"));
    }

    #[test]
    fn unknown_duration_is_minus_one() {
        let mut t = track(1, "A", "B");
        t.duration_ms = 0;
        assert!(m3u8("n", &[t]).contains("#EXTINF:-1,"));
    }

    #[test]
    fn csv_quotes_commas_and_doubles_quotes() {
        let mut t = track(1, "Earth, Wind & Fire", "The \"Best\" Song");
        t.reason = None;
        let out = csv(&[t]);
        assert!(out.contains("\"Earth, Wind & Fire\""));
        assert!(out.contains("\"The \"\"Best\"\" Song\""));
        assert!(out.ends_with("\r\n"), "RFC 4180 uses CRLF");
    }

    #[test]
    fn csv_header_and_row_have_the_same_field_count() {
        let out = csv(&[track(1, "A", "B")]);
        let mut lines = out.lines();
        let headers = lines.next().expect("header").split(',').count();
        // The row has quoted fields, so count by parsing rather than splitting.
        let row = lines.next().expect("row");
        let mut fields = 1;
        let mut in_quotes = false;
        for ch in row.chars() {
            match ch {
                '"' => in_quotes = !in_quotes,
                ',' if !in_quotes => fields += 1,
                _ => {}
            }
        }
        assert_eq!(headers, fields);
    }

    #[test]
    fn json_round_trips() {
        let out = json_doc("List", &[track(1, "A", "B")]);
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(parsed["count"], 1);
        assert_eq!(parsed["tracks"][0]["artist"], "A");
        assert_eq!(
            parsed["tracks"][0]["url"],
            "https://open.spotify.com/track/id1"
        );
    }

    #[test]
    fn filenames_are_safe_on_windows_too() {
        assert_eq!(safe_filename("AI · focus"), "AI · focus");
        assert_eq!(safe_filename("a/b\\c:d*e?f"), "a-b-c-d-e-f");
        assert_eq!(safe_filename("   "), "playlist");
        assert_eq!(safe_filename("..."), "playlist");
        assert!(safe_filename(&"x".repeat(200)).chars().count() <= 80);
    }

    #[test]
    fn format_parsing_accepts_the_common_spellings() {
        assert_eq!(Format::parse("M3U"), Some(Format::M3u8));
        assert_eq!(Format::parse("csv"), Some(Format::Csv));
        assert_eq!(Format::parse("nope"), None);
    }
}
