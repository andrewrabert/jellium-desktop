//! Jellyfin DeviceProfile JSON builder.
//!
//! Jellyfin source pinned to commit 2c62d40 (matches the
//! `third_party/jellyfin` submodule). Profile-vs-stream matching is
//! plain case-insensitive equality against ffprobe-derived names, so
//! any rename below has to mirror what the server stores on
//! `MediaSource.Container` / `MediaStream.Codec` at probe time.
//!
//! Match logic:
//!   <https://github.com/jellyfin/jellyfin/blob/2c62d40f0d13926874eef9118a95be0dcee4e659/MediaBrowser.Model/Extensions/ContainerHelper.cs#L82-L107>
//! Subtitle match in StreamBuilder:
//!   <https://github.com/jellyfin/jellyfin/blob/2c62d40f0d13926874eef9118a95be0dcee4e659/MediaBrowser.Model/Dlna/StreamBuilder.cs#L1476>
//! Container normalization at probe time (NormalizeFormat):
//!   <https://github.com/jellyfin/jellyfin/blob/2c62d40f0d13926874eef9118a95be0dcee4e659/MediaBrowser.MediaEncoding/Probing/ProbeResultNormalizer.cs#L270-L315>
//! Subtitle normalization at probe time (NormalizeSubtitleCodec):
//!   <https://github.com/jellyfin/jellyfin/blob/2c62d40f0d13926874eef9118a95be0dcee4e659/MediaBrowser.MediaEncoding/Probing/ProbeResultNormalizer.cs#L632-L652>

use serde::Serialize;
use serde_json::Value;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum MediaKind {
    Video,
    Audio,
    Subtitle,
}

#[derive(Clone, Debug)]
pub struct Codec {
    pub name: String,
    pub kind: MediaKind,
}

const SUBTITLE_RENAMES: &[(&str, &str)] = &[
    ("subrip", "srt"),
    ("ass", "ssa"),
    ("hdmv_pgs_subtitle", "PGSSUB"),
    ("dvd_subtitle", "DVDSUB"),
    ("dvb_subtitle", "DVBSUB"),
    ("dvb_teletext", "DVBTXT"),
];

const CONTAINER_RENAMES: &[(&str, &str)] =
    &[("matroska", "mkv"), ("mpegts", "ts"), ("mpegvideo", "mpeg")];

const MAX_STATIC_BITRATE: i64 = 1_000_000_000;
const MUSIC_STREAMING_TRANSCODING_BITRATE: i64 = 1_280_000;
const TIMELINE_OFFSET_SECONDS: i64 = 5;

const TRANSCODE_CONTAINER: &str = "ts";
const TRANSCODE_CONTAINER_MP4: &str = "mp4";
const TRANSCODE_PROTOCOL: &str = "hls";
const TRANSCODE_MAX_AUDIO_CHANNELS: &str = "6";

// Codec sets come from Jellyfin's StreamBuilder._supportedHls* lists (alac
// dropped to stay under the server's 40-char AudioCodec query-param
// validator, ^[a-zA-Z0-9\-\._,|]{0,40}$):
// https://github.com/jellyfin/jellyfin/blob/2c62d40f0d13926874eef9118a95be0dcee4e659/MediaBrowser.Model/Dlna/StreamBuilder.cs#L31-L33
//
// ORDER IS CRITICAL for video: the server picks the output codec straight from
// this list (StreamBuilder keeps profile order; StreamingHelpers takes the
// first entry) and does NO encoder-capability validation — it will happily
// emit `-codec:v av1_nvenc` on a GPU with no AV1 encoder and hard-fail (ffmpeg
// exit 218). So the list must be ordered by descending real-world encode
// compatibility: h264 (every server can hardware-encode it) first, then hevc
// (all recent NVENC/QSV/VAAPI/AMF). av1/vp9 are software-encode-only on the
// vast majority of servers (~0.1x realtime), so they trail as last-resort
// fallbacks only — never reached in practice, since any client that can decode
// av1/vp9 also decodes hevc, which precedes them.
const TRANSCODE_VIDEO_CODEC: &[&str] = &["h264", "hevc", "av1", "vp9"];
const TRANSCODE_AUDIO_CODEC_MP4: &[&str] =
    &["opus", "aac", "eac3", "ac3", "flac", "mp3", "dts", "truehd"];
const TRANSCODE_AUDIO_CODEC_TS: &[&str] = &["aac", "eac3", "ac3", "mp3"];

/// Profile lists this client never populates; serializes as `[]`.
type EmptyList = [(); 0];

const EMPTY_LIST: EmptyList = [];

/// Jellyfin's `DlnaProfileType`.
#[derive(Copy, Clone, Serialize)]
enum ProfileType {
    Video,
    Audio,
    Photo,
}

#[derive(Copy, Clone, Serialize)]
enum SubtitleDeliveryMethod {
    Embed,
    External,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct DirectPlayProfile<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    container: Option<&'a str>,
    #[serde(rename = "Type")]
    profile_type: ProfileType,
    #[serde(skip_serializing_if = "Option::is_none")]
    video_codec: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    audio_codec: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct TranscodingProfile<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    container: Option<&'a str>,
    #[serde(rename = "Type")]
    profile_type: ProfileType,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    audio_codec: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    video_codec: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_audio_channels: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct SubtitleProfile<'a> {
    format: &'a str,
    method: SubtitleDeliveryMethod,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct DeviceProfile<'a> {
    name: &'a str,
    max_static_bitrate: i64,
    music_streaming_transcoding_bitrate: i64,
    timeline_offset_seconds: i64,
    direct_play_profiles: Vec<DirectPlayProfile<'a>>,
    transcoding_profiles: Vec<TranscodingProfile<'a>>,
    subtitle_profiles: Vec<SubtitleProfile<'a>>,
    response_profiles: EmptyList,
    container_profiles: EmptyList,
    codec_profiles: EmptyList,
}

// Expand each input through `renames` and return the deduped union of raw +
// renamed names. Inputs may be comma-joined ffmpeg aliases (e.g.
// "matroska,webm"); each piece is split before lookup.
fn expand_with_renames(inputs: &[String], renames: &[(&'static str, &'static str)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        if s.is_empty() {
            return;
        }
        if !out.iter().any(|e| e == s) {
            out.push(s.to_string());
        }
    };
    for input in inputs {
        for piece in input.split(',') {
            push(piece);
            if let Some((_, renamed)) = renames.iter().find(|(from, _)| *from == piece) {
                push(renamed);
            }
        }
    }
    out
}

// Intersect `preferred` with `available`, keeping `preferred`'s order — the
// server takes the first entry as its output codec — joined as CSV.
fn preferred_csv(preferred: &[&str], available: &[String]) -> String {
    preferred
        .iter()
        .filter(|s| available.iter().any(|a| a == *s))
        .copied()
        .collect::<Vec<&str>>()
        .join(",")
}

/// Build the DeviceProfile JSON.
pub fn build_device_profile(
    decoders: &[Codec],
    demuxers: &[String],
    device_name: &str,
    _app_version: &str,
    force_transcode: bool,
) -> String {
    let mut video_codecs: Vec<String> = Vec::new();
    let mut audio_codecs: Vec<String> = Vec::new();
    let mut subtitle_codecs: Vec<String> = Vec::new();
    for c in decoders {
        match c.kind {
            MediaKind::Video => video_codecs.push(c.name.clone()),
            MediaKind::Audio => audio_codecs.push(c.name.clone()),
            MediaKind::Subtitle => subtitle_codecs.push(c.name.clone()),
        }
    }

    let video_csv = if force_transcode {
        String::new()
    } else {
        video_codecs.join(",")
    };
    let audio_csv = if force_transcode {
        String::new()
    } else {
        audio_codecs.join(",")
    };

    let containers = expand_with_renames(demuxers, CONTAINER_RENAMES);
    let subtitle_names = expand_with_renames(&subtitle_codecs, SUBTITLE_RENAMES);

    // TranscodingProfiles tell the server which formats it may transcode TO,
    // not what we can decode. Stick to the curated preference list intersected
    // with mpv's decoder support — adding the rest would (a) invite the server
    // to transcode to formats we don't want as targets (truehd, dts, vp9...)
    // and (b) push the AudioCodec CSV past the server's 40-char query-param
    // validator (^[a-zA-Z0-9\-\._,|]{0,40}$).
    let transcode_video_csv = preferred_csv(TRANSCODE_VIDEO_CODEC, &video_codecs);
    let transcode_audio_csv_mp4 = preferred_csv(TRANSCODE_AUDIO_CODEC_MP4, &audio_codecs);
    let transcode_audio_csv_ts = preferred_csv(TRANSCODE_AUDIO_CODEC_TS, &audio_codecs);

    // DirectPlayProfiles. ContainerHelper.ContainsContainer splits both the
    // profile's Container and the file's MediaSource.Container on comma and
    // does case-insensitive equality, so one entry with every container
    // comma-joined matches identically to N entries with one container each —
    // without repeating the codec CSV per container.
    let container_csv = containers.join(",");
    let mut direct_play: Vec<DirectPlayProfile> = Vec::new();
    if !video_csv.is_empty() || force_transcode {
        direct_play.push(DirectPlayProfile {
            container: Some(&container_csv),
            profile_type: ProfileType::Video,
            video_codec: Some(&video_csv),
            audio_codec: Some(&audio_csv),
        });
    }
    if !audio_csv.is_empty() || force_transcode {
        direct_play.push(DirectPlayProfile {
            container: Some(&container_csv),
            profile_type: ProfileType::Audio,
            video_codec: None,
            audio_codec: Some(&audio_csv),
        });
    }
    direct_play.push(DirectPlayProfile {
        container: None,
        profile_type: ProfileType::Photo,
        video_codec: None,
        audio_codec: None,
    });

    // mpv handles both Embed and External natively, so no need to distinguish.
    let mut sub_profiles: Vec<SubtitleProfile> = Vec::new();
    for fmt in &subtitle_names {
        sub_profiles.push(SubtitleProfile {
            format: fmt,
            method: SubtitleDeliveryMethod::Embed,
        });
        sub_profiles.push(SubtitleProfile {
            format: fmt,
            method: SubtitleDeliveryMethod::External,
        });
    }

    // TranscodingProfiles: describes what server should produce when a
    // transcode is unavoidable. Order of VideoCodec/AudioCodec is the
    // server's preference order.
    let mut transcoding: Vec<TranscodingProfile> = vec![TranscodingProfile {
        container: None,
        profile_type: ProfileType::Audio,
        protocol: None,
        audio_codec: None,
        video_codec: None,
        max_audio_channels: None,
    }];
    if !force_transcode {
        transcoding.push(TranscodingProfile {
            container: Some(TRANSCODE_CONTAINER_MP4),
            profile_type: ProfileType::Video,
            protocol: Some(TRANSCODE_PROTOCOL),
            audio_codec: Some(&transcode_audio_csv_mp4),
            video_codec: Some(&transcode_video_csv),
            max_audio_channels: Some(TRANSCODE_MAX_AUDIO_CHANNELS),
        });
    }
    transcoding.push(TranscodingProfile {
        container: Some(TRANSCODE_CONTAINER),
        profile_type: ProfileType::Video,
        protocol: Some(TRANSCODE_PROTOCOL),
        audio_codec: Some(&transcode_audio_csv_ts),
        video_codec: Some(&transcode_video_csv),
        max_audio_channels: Some(TRANSCODE_MAX_AUDIO_CHANNELS),
    });
    transcoding.push(TranscodingProfile {
        container: Some("jpeg"),
        profile_type: ProfileType::Photo,
        protocol: None,
        audio_codec: None,
        video_codec: None,
        max_audio_channels: None,
    });

    let profile = DeviceProfile {
        name: device_name,
        max_static_bitrate: MAX_STATIC_BITRATE,
        music_streaming_transcoding_bitrate: MUSIC_STREAMING_TRANSCODING_BITRATE,
        timeline_offset_seconds: TIMELINE_OFFSET_SECONDS,
        direct_play_profiles: direct_play,
        transcoding_profiles: transcoding,
        subtitle_profiles: sub_profiles,
        response_profiles: EMPTY_LIST,
        container_profiles: EMPTY_LIST,
        codec_profiles: EMPTY_LIST,
    };
    serde_json::to_string(&profile).unwrap_or_default()
}

// ---- URL helpers ----

const SCHEME_SEPARATOR: &str = "://";
const DEFAULT_SCHEME: &str = "http://";

/// Scheme prefixes normalized to lowercase; every other prefix passes through.
const LOWERCASE_SCHEMES: [&str; 2] = ["http:", "https:"];

const WEB_PATH_SEGMENT: &str = "/web";

/// Trim surrounding whitespace, lowercase `Http:`/`Https:` scheme prefixes,
/// and prepend `http://` when no scheme is present.
///
/// Non-ASCII input never panics: the prefix comparison is `get`-guarded.
pub fn normalize_input(user_input: &str) -> String {
    let trimmed = user_input.trim();
    let cased = LOWERCASE_SCHEMES
        .iter()
        .find(|scheme| {
            trimmed
                .get(..scheme.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme))
        })
        .map_or_else(
            || trimmed.to_string(),
            |scheme| format!("{scheme}{}", &trimmed[scheme.len()..]),
        );
    if cased.contains(SCHEME_SEPARATOR) {
        cased
    } else {
        format!("{DEFAULT_SCHEME}{cased}")
    }
}

/// Reduce a URL to its server base:
///   - if the URL contains `/web` (case-insensitive) in its path, truncate
///     at the last occurrence;
///   - otherwise return the origin (everything up to the first `/` after
///     `://`, or the whole string if there's no path).
///
/// The result is a prefix slice of `url`: no percent-encoding, no punycode,
/// no default-port stripping, no case folding of host or path.
pub fn extract_base_url(url: &str) -> &str {
    let lower = url.to_ascii_lowercase();
    if let Some(pos) = lower.rfind(WEB_PATH_SEGMENT) {
        return &url[..pos];
    }
    let host_start = url
        .find(SCHEME_SEPARATOR)
        .map_or(0, |i| i + SCHEME_SEPARATOR.len());
    let end = url[host_start..]
        .find('/')
        .map_or(url.len(), |rel| host_start + rel);
    &url[..end]
}

/// Whether `url` is a page of the server at `base`.
///
/// `base` is canonical — the probe resolved it, including the path a
/// subpath-hosted server sits under — so it is compared verbatim and never
/// reduced again. The match ends on a path boundary, so
/// `https://host/jellyfin2/web` is not a page of `https://host/jellyfin`, and a
/// document with no base at all — `about:blank`, an empty URL, a
/// `chrome-error:` page — is a page of nothing.
pub fn is_page_of(base: &str, url: &str) -> bool {
    if base.is_empty() {
        return false;
    }
    let Some(rest) = url.strip_prefix(base) else {
        return false;
    };
    rest.is_empty() || rest.starts_with(['/', '?', '#'])
}

/// Validate that a Jellyfin `/System/Info/Public` response body is a JSON
/// object with a non-empty string `Id` field.
pub fn is_valid_public_info(body: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let Some(o) = v.as_object() else { return false };
    o.get("Id")
        .and_then(Value::as_str)
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

// ---- tests ----

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn parse(s: &str) -> Result<Value, Box<dyn std::error::Error>> {
        Ok(serde_json::from_str(s)?)
    }

    fn codec(name: &str, kind: MediaKind) -> Codec {
        Codec {
            name: name.to_string(),
            kind,
        }
    }

    #[test]
    fn empty_capabilities_emits_photo_only_direct_play() -> TestResult {
        let s = build_device_profile(&[], &[], "dev", "1.0", false);
        let v = parse(&s)?;
        let dp = v["DirectPlayProfiles"].as_array().ok_or("expected array")?;
        assert_eq!(dp.len(), 1);
        assert_eq!(dp[0]["Type"], "Photo");
        Ok(())
    }

    #[test]
    fn force_transcode_empties_codec_csvs_and_drops_fmp4() -> TestResult {
        let decoders = vec![
            codec("h264", MediaKind::Video),
            codec("aac", MediaKind::Audio),
        ];
        let s = build_device_profile(&decoders, &["matroska".into()], "dev", "1.0", true);
        let v = parse(&s)?;
        let dp = v["DirectPlayProfiles"].as_array().ok_or("expected array")?;
        let video = dp
            .iter()
            .find(|e| e["Type"] == "Video")
            .ok_or("no Video entry")?;
        assert_eq!(video["VideoCodec"], "");
        assert_eq!(video["AudioCodec"], "");
        let audio = dp
            .iter()
            .find(|e| e["Type"] == "Audio")
            .ok_or("no Audio entry")?;
        assert_eq!(audio["AudioCodec"], "");

        let tp = v["TranscodingProfiles"]
            .as_array()
            .ok_or("expected array")?;
        // Audio + Video (ts) + Photo. No fmp4 entry under force_transcode.
        assert!(!tp.iter().any(|e| e["Container"] == "mp4"));
        Ok(())
    }

    #[test]
    fn container_rename_expands_and_dedupes() -> TestResult {
        // Container CSV is only emitted when there are video/audio decoders.
        let decoders = vec![codec("h264", MediaKind::Video)];
        let s = build_device_profile(&decoders, &["matroska,webm".into()], "dev", "1.0", false);
        let v = parse(&s)?;
        let dp = v["DirectPlayProfiles"].as_array().ok_or("expected array")?;
        let video = dp
            .iter()
            .find(|e| e["Type"] == "Video")
            .ok_or("no Video entry")?;
        let container = video["Container"].as_str().ok_or("expected string")?;
        let parts: Vec<&str> = container.split(',').collect();
        assert!(parts.contains(&"matroska"));
        assert!(parts.contains(&"webm"));
        assert!(parts.contains(&"mkv"));
        // No duplicates.
        let mut sorted = parts.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), parts.len());
        Ok(())
    }

    #[test]
    fn subtitle_rename_emits_both_methods() -> TestResult {
        let decoders = vec![codec("subrip", MediaKind::Subtitle)];
        let s = build_device_profile(&decoders, &[], "dev", "1.0", false);
        let v = parse(&s)?;
        let sp = v["SubtitleProfiles"].as_array().ok_or("expected array")?;
        let formats: Vec<&str> = sp
            .iter()
            .map(|e| e["Format"].as_str().unwrap_or_default())
            .collect();
        assert!(formats.contains(&"subrip"));
        assert!(formats.contains(&"srt"));
        // Each format appears with both Embed and External.
        for fmt in ["subrip", "srt"] {
            let methods: Vec<&str> = sp
                .iter()
                .filter(|e| e["Format"] == fmt)
                .map(|e| e["Method"].as_str().unwrap_or_default())
                .collect();
            assert!(methods.contains(&"Embed"));
            assert!(methods.contains(&"External"));
        }
        Ok(())
    }

    #[test]
    fn transcode_video_prefers_h264_first_regardless_of_decoder_order() -> TestResult {
        // The server picks the first VideoCodec with no encoder validation, so
        // h264 (universally hardware-encodable) must lead even when the decoder
        // list enumerates other codecs first. av1/vp9 trail as last resort.
        let decoders = vec![
            codec("av1", MediaKind::Video),
            codec("vp9", MediaKind::Video),
            codec("hevc", MediaKind::Video),
            codec("h264", MediaKind::Video),
        ];
        let s = build_device_profile(&decoders, &["matroska".into()], "dev", "1.0", false);
        let v = parse(&s)?;
        let tp = v["TranscodingProfiles"]
            .as_array()
            .ok_or("expected array")?;
        for e in tp.iter().filter(|e| e["Type"] == "Video") {
            assert_eq!(e["VideoCodec"], "h264,hevc,av1,vp9");
        }
        Ok(())
    }

    #[test]
    fn transcode_audio_csv_uses_curated_order_not_decoder_order() -> TestResult {
        let decoders = vec![
            codec("h264", MediaKind::Video),
            codec("mp3", MediaKind::Audio),
            codec("aac", MediaKind::Audio),
            codec("opus", MediaKind::Audio),
        ];
        let s = build_device_profile(&decoders, &["matroska".into()], "dev", "1.0", false);
        let v = parse(&s)?;
        let tp = v["TranscodingProfiles"]
            .as_array()
            .ok_or("expected array")?;
        let fmp4 = tp
            .iter()
            .find(|e| e["Container"] == "mp4")
            .ok_or("no fmp4 entry")?;
        assert_eq!(fmp4["AudioCodec"], "opus,aac,mp3");
        Ok(())
    }

    #[test]
    fn normalize_trims_whitespace() {
        assert_eq!(
            normalize_input("  http://example.com  "),
            "http://example.com"
        );
        assert_eq!(normalize_input("\thttps://host\n"), "https://host");
    }

    #[test]
    fn normalize_lowercases_scheme() {
        assert_eq!(normalize_input("HTTP://example.com"), "http://example.com");
        assert_eq!(
            normalize_input("HTTPS://example.com"),
            "https://example.com"
        );
        assert_eq!(normalize_input("Http://example.com"), "http://example.com");
        assert_eq!(
            normalize_input("Https://example.com"),
            "https://example.com"
        );
    }

    #[test]
    fn normalize_prepends_http_when_no_scheme() {
        assert_eq!(normalize_input("example.com"), "http://example.com");
        assert_eq!(
            normalize_input("example.com:8096"),
            "http://example.com:8096"
        );
        assert_eq!(normalize_input("192.168.1.10"), "http://192.168.1.10");
    }

    #[test]
    fn normalize_trims_whitespace_before_prepending_scheme() {
        // Trim must happen first: otherwise a leading space would get trapped
        // between the prepended scheme and the host, producing "http:// host".
        assert_eq!(normalize_input(" example.com"), "http://example.com");
        assert_eq!(normalize_input("\texample.com\n"), "http://example.com");
        assert_eq!(normalize_input("   example.com  "), "http://example.com");
    }

    #[test]
    fn normalize_leaves_well_formed_input_unchanged() {
        assert_eq!(normalize_input("http://example.com"), "http://example.com");
        assert_eq!(
            normalize_input("https://example.com/jellyfin"),
            "https://example.com/jellyfin"
        );
    }

    #[test]
    fn normalize_passes_non_http_schemes_through() {
        // Only Http:/Https: prefixes are touched; anything else passes through.
        assert_eq!(normalize_input("FTP://example.com"), "FTP://example.com");
    }

    #[test]
    fn a_subpath_hosted_servers_pages_are_its_own() {
        let base = "https://host/jellyfin";
        assert!(is_page_of(base, base));
        assert!(is_page_of(base, "https://host/jellyfin/web/index.html"));
        assert!(is_page_of(base, "https://host/jellyfin/web/#!/home.html"));
        assert!(is_page_of(base, "https://host/jellyfin?foo=bar"));
    }

    #[test]
    fn a_root_hosted_servers_pages_are_its_own() {
        let base = "https://host";
        assert!(is_page_of(base, base));
        assert!(is_page_of(base, "https://host/web/index.html"));
        assert!(is_page_of(base, "https://host/"));
    }

    #[test]
    fn a_sibling_path_is_not_a_page_of_the_base() {
        let base = "https://host/jellyfin";
        assert!(!is_page_of(base, "https://host/jellyfin2/web/index.html"));
        assert!(!is_page_of(base, "https://other/jellyfin/web/index.html"));
        assert!(!is_page_of(base, "https://host/web/index.html"));
    }

    #[test]
    fn a_blank_or_error_document_is_a_page_of_nothing() {
        let base = "https://host/jellyfin";
        assert!(!is_page_of(base, "about:blank"));
        assert!(!is_page_of(base, ""));
        assert!(!is_page_of(base, "chrome-error://chromewebdata/"));
        assert!(!is_page_of("", "https://host/jellyfin/web/"));
    }

    #[test]
    fn extract_base_truncates_at_web() {
        assert_eq!(
            extract_base_url("https://host/web/index.html"),
            "https://host"
        );
        assert_eq!(extract_base_url("https://host/web"), "https://host");
    }

    #[test]
    fn extract_base_preserves_prefix_before_web() {
        assert_eq!(
            extract_base_url("https://host/jellyfin/web/index.html"),
            "https://host/jellyfin"
        );
        assert_eq!(
            extract_base_url("https://host:8096/jellyfin/web/"),
            "https://host:8096/jellyfin"
        );
    }

    #[test]
    fn extract_base_uses_last_web_when_multiple() {
        assert_eq!(
            extract_base_url("https://host/web/app/web/index.html"),
            "https://host/web/app"
        );
    }

    #[test]
    fn extract_base_case_insensitive_web() {
        assert_eq!(
            extract_base_url("https://host/WEB/index.html"),
            "https://host"
        );
        assert_eq!(
            extract_base_url("https://host/Web/index.html"),
            "https://host"
        );
        assert_eq!(
            extract_base_url("https://host/wEb/index.html"),
            "https://host"
        );
    }

    #[test]
    fn extract_base_returns_origin_when_no_web() {
        assert_eq!(extract_base_url("https://host/"), "https://host");
        assert_eq!(extract_base_url("https://host"), "https://host");
        assert_eq!(extract_base_url("https://host/foo"), "https://host");
        assert_eq!(extract_base_url("http://host:8096/foo"), "http://host:8096");
    }

    #[test]
    fn extract_base_handles_port_in_origin() {
        assert_eq!(
            extract_base_url("http://host:8096/web/index.html"),
            "http://host:8096"
        );
        assert_eq!(
            extract_base_url("http://localhost:8096/web/"),
            "http://localhost:8096"
        );
        assert_eq!(
            extract_base_url("http://192.168.1.100:8096/web/"),
            "http://192.168.1.100:8096"
        );
        assert_eq!(
            extract_base_url("http://[::1]:8096/web/"),
            "http://[::1]:8096"
        );
    }

    #[test]
    fn extract_base_strips_query_and_fragment_after_web() {
        assert_eq!(
            extract_base_url("https://host/web/?foo=bar"),
            "https://host"
        );
        assert_eq!(
            extract_base_url("https://host/web/#section"),
            "https://host"
        );
        assert_eq!(
            extract_base_url("https://host/jellyfin/web/?foo=bar#section"),
            "https://host/jellyfin"
        );
    }

    #[test]
    fn extract_base_treats_website_and_webdav_as_web_match() {
        // Matches Qt behavior: substring match on "/web" does not distinguish
        // these longer path segments. Locked in here so a future fix is deliberate.
        assert_eq!(extract_base_url("https://host/website/"), "https://host");
        assert_eq!(extract_base_url("https://host/webdav/"), "https://host");
    }

    #[test]
    fn extract_base_handles_degenerate_urls() {
        assert_eq!(extract_base_url("https://"), "https://");
        assert_eq!(extract_base_url("https:///web/"), "https://");
    }

    #[test]
    fn idn_hosts_survive_unchanged() {
        assert_eq!(
            normalize_input("http://example.みんな"),
            "http://example.みんな"
        );
        assert_eq!(normalize_input("example.みんな"), "http://example.みんな");
        assert_eq!(
            normalize_input("  HTTPS://example.みんな/web "),
            "https://example.みんな/web"
        );

        assert_eq!(
            extract_base_url("http://example.みんな/web/"),
            "http://example.みんな"
        );
        assert_eq!(
            extract_base_url("https://example.みんな/jellyfin/web"),
            "https://example.みんな/jellyfin"
        );
        assert_eq!(
            extract_base_url("http://example.みんな/"),
            "http://example.みんな"
        );
    }

    #[test]
    fn public_info_accepts_object_with_non_empty_id() {
        assert!(is_valid_public_info(br#"{"Id":"abc","ServerName":"x"}"#));
        assert!(is_valid_public_info(br#"{"ServerName":"x","Id":"zzz"}"#));
    }

    #[test]
    fn public_info_rejects_empty_or_missing_id() {
        assert!(!is_valid_public_info(br#"{"Id":""}"#));
        assert!(!is_valid_public_info(br#"{"ServerName":"x"}"#));
        assert!(!is_valid_public_info(br#"{}"#));
    }

    #[test]
    fn public_info_rejects_non_string_id() {
        assert!(!is_valid_public_info(br#"{"Id":null}"#));
        assert!(!is_valid_public_info(br#"{"Id":123}"#));
        assert!(!is_valid_public_info(br#"{"Id":true}"#));
    }

    #[test]
    fn public_info_rejects_non_object_json() {
        assert!(!is_valid_public_info(br#"["Id"]"#));
        assert!(!is_valid_public_info(br#""Id""#));
        assert!(!is_valid_public_info(b"null"));
    }

    #[test]
    fn public_info_rejects_invalid_json() {
        assert!(!is_valid_public_info(b""));
        assert!(!is_valid_public_info(b"not json"));
        assert!(!is_valid_public_info(br#"{"Id":"abc""#));
    }

    #[test]
    fn public_info_no_false_positive_on_substring() {
        // Regression: the old C++ code string-matched "Id" which matched any
        // body containing that substring. Real JSON parse must reject these.
        assert!(!is_valid_public_info(
            br#"<html>oops "Id" lives here</html>"#
        ));
    }

    const EMPTY_PROFILE_JSON: &str = r#"{"Name":"dev","MaxStaticBitrate":1000000000,"MusicStreamingTranscodingBitrate":1280000,"TimelineOffsetSeconds":5,"DirectPlayProfiles":[{"Type":"Photo"}],"TranscodingProfiles":[{"Type":"Audio"},{"Container":"mp4","Type":"Video","Protocol":"hls","AudioCodec":"","VideoCodec":"","MaxAudioChannels":"6"},{"Container":"ts","Type":"Video","Protocol":"hls","AudioCodec":"","VideoCodec":"","MaxAudioChannels":"6"},{"Container":"jpeg","Type":"Photo"}],"SubtitleProfiles":[],"ResponseProfiles":[],"ContainerProfiles":[],"CodecProfiles":[]}"#;

    const TYPICAL_PROFILE_JSON: &str = r#"{"Name":"dev","MaxStaticBitrate":1000000000,"MusicStreamingTranscodingBitrate":1280000,"TimelineOffsetSeconds":5,"DirectPlayProfiles":[{"Container":"matroska,mkv,webm,mpegts,ts","Type":"Video","VideoCodec":"h264,hevc","AudioCodec":"aac,opus"},{"Container":"matroska,mkv,webm,mpegts,ts","Type":"Audio","AudioCodec":"aac,opus"},{"Type":"Photo"}],"TranscodingProfiles":[{"Type":"Audio"},{"Container":"mp4","Type":"Video","Protocol":"hls","AudioCodec":"opus,aac","VideoCodec":"h264,hevc","MaxAudioChannels":"6"},{"Container":"ts","Type":"Video","Protocol":"hls","AudioCodec":"aac","VideoCodec":"h264,hevc","MaxAudioChannels":"6"},{"Container":"jpeg","Type":"Photo"}],"SubtitleProfiles":[{"Format":"subrip","Method":"Embed"},{"Format":"subrip","Method":"External"},{"Format":"srt","Method":"Embed"},{"Format":"srt","Method":"External"}],"ResponseProfiles":[],"ContainerProfiles":[],"CodecProfiles":[]}"#;

    const FORCED_TRANSCODE_PROFILE_JSON: &str = r#"{"Name":"dev","MaxStaticBitrate":1000000000,"MusicStreamingTranscodingBitrate":1280000,"TimelineOffsetSeconds":5,"DirectPlayProfiles":[{"Container":"matroska,mkv","Type":"Video","VideoCodec":"","AudioCodec":""},{"Container":"matroska,mkv","Type":"Audio","AudioCodec":""},{"Type":"Photo"}],"TranscodingProfiles":[{"Type":"Audio"},{"Container":"ts","Type":"Video","Protocol":"hls","AudioCodec":"aac","VideoCodec":"h264,hevc","MaxAudioChannels":"6"},{"Container":"jpeg","Type":"Photo"}],"SubtitleProfiles":[{"Format":"subrip","Method":"Embed"},{"Format":"subrip","Method":"External"},{"Format":"srt","Method":"Embed"},{"Format":"srt","Method":"External"}],"ResponseProfiles":[],"ContainerProfiles":[],"CodecProfiles":[]}"#;

    fn byte_exact_decoders() -> Vec<Codec> {
        vec![
            codec("h264", MediaKind::Video),
            codec("hevc", MediaKind::Video),
            codec("aac", MediaKind::Audio),
            codec("opus", MediaKind::Audio),
            codec("subrip", MediaKind::Subtitle),
        ]
    }

    #[test]
    fn empty_capabilities_profile_is_byte_exact() {
        assert_eq!(
            build_device_profile(&[], &[], "dev", "1.0", false),
            EMPTY_PROFILE_JSON
        );
    }

    #[test]
    fn typical_capabilities_profile_is_byte_exact() {
        let demuxers = vec!["matroska,webm".to_string(), "mpegts".to_string()];
        assert_eq!(
            build_device_profile(&byte_exact_decoders(), &demuxers, "dev", "1.0", false),
            TYPICAL_PROFILE_JSON
        );
    }

    #[test]
    fn forced_transcode_profile_is_byte_exact() {
        let demuxers = vec!["matroska".to_string()];
        assert_eq!(
            build_device_profile(&byte_exact_decoders(), &demuxers, "dev", "1.0", true),
            FORCED_TRANSCODE_PROFILE_JSON
        );
    }

    #[test]
    fn normalize_survives_multibyte_char_across_scheme_prefix() {
        assert_eq!(normalize_input("htt\u{2028}p"), "http://htt\u{2028}p");
        assert_eq!(normalize_input("みんな"), "http://みんな");
    }
}
