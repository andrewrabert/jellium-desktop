use jfn_playback::{ItemKind, MediaMetadata, PlaybackPhase};

const TYPE_LISTENING: u8 = 2;
const TYPE_WATCHING: u8 = 3;

const TEXT_MAX_CHARS: usize = 128;
const TEXT_MIN_CHARS: usize = 2;

pub const ASSET_LOGO: &str = "logo";
pub const ASSET_PAUSE: &str = "pause";

#[derive(Clone, Copy, Debug)]
pub struct ProjectInput<'a> {
    pub phase: PlaybackPhase,
    pub seeking: bool,
    pub buffering: bool,
    pub rate: f64,
    pub position_us: i64,
    pub duration_us: i64,
    pub meta: &'a MediaMetadata,
    pub timeline_armed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timeline {
    pub position_us: i64,
    pub duration_us: i64,
    pub rate: f64,
}

const MAX_BUTTONS: usize = 2;
const MAX_ID_CHARS: usize = 32;

#[derive(Clone, Debug, PartialEq)]
pub struct Button {
    pub label: &'static str,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Activity {
    pub activity_type: u8,
    pub name: String,
    pub details: String,
    pub state: String,
    pub large_image: String,
    pub large_text: String,
    pub small_image: &'static str,
    pub small_text: String,
    pub timeline: Option<Timeline>,
    pub buttons: Vec<Button>,
}

#[must_use]
pub fn project(input: &ProjectInput<'_>) -> Option<Activity> {
    if input.phase == PlaybackPhase::Stopped {
        return None;
    }
    let meta = input.meta;
    if meta.title.trim().is_empty() {
        return None;
    }

    let (details, state, activity_type) = match meta.kind {
        ItemKind::Episode => {
            let (details, state) = episode_lines(meta);
            (details, state, TYPE_WATCHING)
        }
        ItemKind::Music => (meta.title.clone(), meta.artist.clone(), TYPE_LISTENING),
        ItemKind::Movie => {
            let year = if meta.year > 0 {
                meta.year.to_string()
            } else {
                String::new()
            };
            (meta.title.clone(), year, TYPE_WATCHING)
        }
        ItemKind::MusicVideo | ItemKind::Video | ItemKind::Unknown => {
            (meta.title.clone(), meta.artist.clone(), TYPE_WATCHING)
        }
    };

    let rolling = input.phase == PlaybackPhase::Playing
        && !input.seeking
        && !input.buffering
        && input.rate > 0.0
        && input.timeline_armed;
    let timeline = rolling.then(|| Timeline {
        position_us: input.position_us.max(0),
        duration_us: input.duration_us.max(0),
        rate: input.rate,
    });

    let (small_image, small_text) = if input.phase == PlaybackPhase::Paused {
        let timecode = timecode_text(input.position_us, input.duration_us);
        (ASSET_PAUSE, clamp_text(&format!("Paused · {timecode}")))
    } else {
        ("", String::new())
    };

    let details = clamp_text(&details);
    Some(Activity {
        activity_type,
        name: details.clone(),
        details,
        state: clamp_text(&state),
        large_image: if meta.art_url.is_empty() {
            ASSET_LOGO.to_string()
        } else {
            meta.art_url.clone()
        },
        large_text: clamp_text(first_non_empty(&[&meta.artist, &meta.title])),
        small_image,
        small_text,
        timeline,
        buttons: links(meta),
    })
}

fn links(meta: &MediaMetadata) -> Vec<Button> {
    let mut out = Vec::new();
    if is_sane_id(&meta.imdb_id) {
        out.push(Button {
            label: "IMDb",
            url: format!("https://www.imdb.com/title/{}/", meta.imdb_id),
        });
    }
    if is_sane_id(&meta.anilist_id) {
        out.push(Button {
            label: "AniList",
            url: format!("https://anilist.co/anime/{}", meta.anilist_id),
        });
    }
    if is_sane_id(&meta.tmdb_id) {
        let path = if meta.kind == ItemKind::Movie {
            "movie"
        } else {
            "tv"
        };
        out.push(Button {
            label: "TMDb",
            url: format!("https://www.themoviedb.org/{path}/{}", meta.tmdb_id),
        });
    }
    out.truncate(MAX_BUTTONS);
    out
}

fn is_sane_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_CHARS
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn episode_lines(meta: &MediaMetadata) -> (String, String) {
    let numbering = season_episode(meta.season_number, meta.track_number);
    let series = meta.artist.trim();
    if series.is_empty() {
        return (meta.title.clone(), numbering);
    }
    let state = if numbering.is_empty() {
        meta.title.clone()
    } else {
        format!("{numbering} · {}", meta.title)
    };
    (series.to_string(), state)
}

fn season_episode(season: i32, episode: i32) -> String {
    match (season > 0, episode > 0) {
        (true, true) => format!("S{season}E{episode}"),
        (true, false) => format!("S{season}"),
        (false, true) => format!("E{episode}"),
        (false, false) => String::new(),
    }
}

fn first_non_empty<'a>(candidates: &[&'a str]) -> &'a str {
    for c in candidates {
        if !c.trim().is_empty() {
            return c;
        }
    }
    ""
}

fn timecode_text(position_us: i64, duration_us: i64) -> String {
    let pos = format_hms(position_us);
    if duration_us > 0 {
        format!("{pos} / {}", format_hms(duration_us))
    } else {
        pos
    }
}

fn format_hms(us: i64) -> String {
    let total = us.max(0) / 1_000_000;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn clamp_text(s: &str) -> String {
    let t = s.trim();
    if t.chars().count() < TEXT_MIN_CHARS {
        return String::new();
    }
    t.chars().take(TEXT_MAX_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jfn_playback::MediaType;

    #[track_caller]
    fn projected(i: &ProjectInput<'_>) -> Activity {
        match project(i) {
            Some(a) => a,
            None => unreachable!("expected an activity"),
        }
    }

    fn episode() -> MediaMetadata {
        MediaMetadata {
            id: "abc".into(),
            title: "The Land Where Souls Rest".into(),
            artist: "Frieren".into(),
            album: "Season 1".into(),
            track_number: 12,
            season_number: 1,
            year: 2023,
            duration_us: 1_400_000_000,
            art_url: String::new(),
            art_data_uri: String::new(),
            media_type: MediaType::Video,
            kind: ItemKind::Episode,
            imdb_id: String::new(),
            tmdb_id: String::new(),
            anilist_id: String::new(),
        }
    }

    fn input(meta: &MediaMetadata, phase: PlaybackPhase) -> ProjectInput<'_> {
        ProjectInput {
            phase,
            seeking: false,
            buffering: false,
            rate: 1.0,
            position_us: 60_000_000,
            duration_us: meta.duration_us,
            meta,
            timeline_armed: true,
        }
    }

    #[test]
    fn stopped_clears_presence() {
        let m = episode();
        assert!(project(&input(&m, PlaybackPhase::Stopped)).is_none());
    }

    #[test]
    fn untitled_clears_presence() {
        let mut m = episode();
        m.title = String::new();
        assert!(project(&input(&m, PlaybackPhase::Playing)).is_none());
    }

    #[test]
    fn episode_shows_series_then_numbering() {
        let m = episode();
        let a = projected(&input(&m, PlaybackPhase::Playing));
        assert_eq!(a.details, "Frieren");
        assert_eq!(a.state, "S1E12 · The Land Where Souls Rest");
        assert_eq!(a.activity_type, TYPE_WATCHING);
    }

    #[test]
    fn episode_without_series_leads_with_title() {
        let mut m = episode();
        m.artist = String::new();
        let a = projected(&input(&m, PlaybackPhase::Playing));
        assert_eq!(a.details, "The Land Where Souls Rest");
        assert_eq!(a.state, "S1E12");
    }

    #[test]
    fn movie_shows_title_and_year() {
        let mut m = episode();
        m.kind = ItemKind::Movie;
        m.title = "Dune".into();
        m.year = 2021;
        let a = projected(&input(&m, PlaybackPhase::Playing));
        assert_eq!(a.details, "Dune");
        assert_eq!(a.state, "2021");
    }

    #[test]
    fn music_uses_listening_type() {
        let mut m = episode();
        m.kind = ItemKind::Music;
        let a = projected(&input(&m, PlaybackPhase::Playing));
        assert_eq!(a.activity_type, TYPE_LISTENING);
    }

    #[test]
    fn playing_arms_the_bar() {
        let m = episode();
        let a = projected(&input(&m, PlaybackPhase::Playing));
        assert_eq!(
            a.timeline,
            Some(Timeline {
                position_us: 60_000_000,
                duration_us: 1_400_000_000,
                rate: 1.0,
            })
        );
    }

    #[test]
    fn paused_drops_the_bar_but_keeps_the_card() {
        let m = episode();
        let a = projected(&input(&m, PlaybackPhase::Paused));
        assert!(a.timeline.is_none());
        assert_eq!(a.small_image, ASSET_PAUSE);
        assert!(a.small_text.starts_with("Paused · "));
    }

    #[test]
    fn playing_shows_no_badge_at_all() {
        let m = episode();
        let a = projected(&input(&m, PlaybackPhase::Playing));
        assert_eq!(a.small_image, "");
        assert_eq!(a.small_text, "");
    }

    #[test]
    fn only_paused_gets_a_badge() {
        let m = episode();
        for phase in [PlaybackPhase::Playing, PlaybackPhase::Starting] {
            assert_eq!(projected(&input(&m, phase)).small_image, "", "{phase:?}");
        }
        assert_eq!(
            projected(&input(&m, PlaybackPhase::Paused)).small_image,
            ASSET_PAUSE
        );
    }

    #[test]
    fn starting_drops_the_bar() {
        let m = episode();
        let a = projected(&input(&m, PlaybackPhase::Starting));
        assert!(a.timeline.is_none());
    }

    #[test]
    fn seeking_drops_the_bar() {
        let m = episode();
        let mut i = input(&m, PlaybackPhase::Playing);
        i.seeking = true;
        assert!(projected(&i).timeline.is_none());
    }

    #[test]
    fn buffering_drops_the_bar() {
        let m = episode();
        let mut i = input(&m, PlaybackPhase::Playing);
        i.buffering = true;
        assert!(projected(&i).timeline.is_none());
    }

    #[test]
    fn zero_rate_drops_the_bar() {
        let m = episode();
        let mut i = input(&m, PlaybackPhase::Playing);
        i.rate = 0.0;
        assert!(projected(&i).timeline.is_none());
    }

    #[test]
    fn unarmed_timeline_drops_the_bar() {
        let m = episode();
        let mut i = input(&m, PlaybackPhase::Playing);
        i.timeline_armed = false;
        assert!(projected(&i).timeline.is_none());
    }

    #[test]
    fn multibyte_titles_truncate_on_char_boundaries() {
        let mut m = episode();
        m.artist = "葬送のフリーレン".repeat(40);
        let a = projected(&input(&m, PlaybackPhase::Playing));
        assert_eq!(a.details.chars().count(), TEXT_MAX_CHARS);
    }

    #[test]
    fn single_char_text_is_dropped() {
        let mut m = episode();
        m.kind = ItemKind::Movie;
        m.title = "Z".into();
        m.year = 0;
        let a = projected(&input(&m, PlaybackPhase::Playing));
        assert_eq!(a.details, "");
        assert_eq!(a.state, "");
    }

    #[test]
    fn art_url_overrides_the_static_asset() {
        let mut m = episode();
        m.art_url = "https://jf.example/Items/1/Images/Primary".into();
        let a = projected(&input(&m, PlaybackPhase::Playing));
        assert_eq!(a.large_image, m.art_url);
    }

    #[test]
    fn missing_art_falls_back_to_the_logo() {
        let m = episode();
        let a = projected(&input(&m, PlaybackPhase::Playing));
        assert_eq!(a.large_image, ASSET_LOGO);
    }

    #[test]
    fn imdb_and_anilist_win_over_tmdb_when_all_present() {
        let mut m = episode();
        m.imdb_id = "tt15469038".into();
        m.anilist_id = "154587".into();
        m.tmdb_id = "209867".into();
        let a = projected(&input(&m, PlaybackPhase::Playing));
        assert_eq!(a.buttons.len(), MAX_BUTTONS);
        assert_eq!(a.buttons[0].label, "IMDb");
        assert_eq!(a.buttons[0].url, "https://www.imdb.com/title/tt15469038/");
        assert_eq!(a.buttons[1].label, "AniList");
        assert_eq!(a.buttons[1].url, "https://anilist.co/anime/154587");
    }

    #[test]
    fn tmdb_path_follows_the_item_kind() {
        let mut m = episode();
        m.tmdb_id = "693134".into();
        let ep = projected(&input(&m, PlaybackPhase::Playing));
        assert_eq!(ep.buttons[0].url, "https://www.themoviedb.org/tv/693134");

        m.kind = ItemKind::Movie;
        let film = projected(&input(&m, PlaybackPhase::Playing));
        assert_eq!(
            film.buttons[0].url,
            "https://www.themoviedb.org/movie/693134"
        );
    }

    #[test]
    fn no_provider_ids_means_no_buttons() {
        let m = episode();
        assert!(
            projected(&input(&m, PlaybackPhase::Playing))
                .buttons
                .is_empty()
        );
    }

    #[test]
    fn hostile_provider_ids_are_rejected() {
        for bad in [
            "//evil.example",
            "tt1/../../x",
            "tt1?q=a",
            "https://jellyfin.internal/x",
            "tt1 tt2",
            "",
        ] {
            let mut m = episode();
            m.imdb_id = bad.into();
            assert!(
                projected(&input(&m, PlaybackPhase::Playing))
                    .buttons
                    .is_empty(),
                "should have rejected {bad:?}"
            );
        }
    }

    #[test]
    fn buttons_never_reference_the_jellyfin_server() {
        let mut m = episode();
        m.art_url = "https://jellyfin.example.internal/Items/1/Images/Primary".into();
        m.imdb_id = "tt15469038".into();
        m.tmdb_id = "209867".into();
        let a = projected(&input(&m, PlaybackPhase::Playing));
        assert!(!a.buttons.is_empty());
        for b in &a.buttons {
            assert!(
                b.url.starts_with("https://www.imdb.com/")
                    || b.url.starts_with("https://anilist.co/")
                    || b.url.starts_with("https://www.themoviedb.org/"),
                "unexpected host in {}",
                b.url
            );
        }
    }

    #[test]
    fn timecode_uses_hours_only_when_needed() {
        assert_eq!(format_hms(0), "0:00");
        assert_eq!(format_hms(62_000_000), "1:02");
        assert_eq!(format_hms(3_723_000_000), "1:02:03");
    }

    #[test]
    fn live_streams_show_elapsed_only() {
        let mut m = episode();
        m.duration_us = 0;
        let mut i = input(&m, PlaybackPhase::Paused);
        i.duration_us = 0;
        let a = projected(&i);
        assert!(!a.small_text.contains('/'));
    }
}
