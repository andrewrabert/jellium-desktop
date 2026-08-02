use serde_json::{Map, Value, json};

use crate::projection::Activity;

fn put(o: &mut Map<String, Value>, key: &str, v: &str) {
    if !v.is_empty() {
        o.insert(key.to_owned(), Value::String(v.to_owned()));
    }
}

#[must_use]
pub fn to_json(activity: &Activity, now_ms: i64) -> Value {
    let mut o = Map::new();
    o.insert("type".to_owned(), json!(activity.activity_type));
    put(&mut o, "name", &activity.name);
    put(&mut o, "details", &activity.details);
    put(&mut o, "state", &activity.state);

    let primary_link = activity.buttons.first().map_or("", |b| b.url.as_str());
    put(&mut o, "details_url", primary_link);

    let mut assets = Map::new();
    put(&mut assets, "large_image", &activity.large_image);
    put(&mut assets, "large_text", &activity.large_text);
    put(&mut assets, "large_url", primary_link);
    put(&mut assets, "small_image", activity.small_image);
    put(&mut assets, "small_text", &activity.small_text);
    o.insert("assets".to_owned(), Value::Object(assets));

    if let Some(t) = activity.timeline {
        let rate = if t.rate > 0.0 { t.rate } else { 1.0 };
        let elapsed_ms = us_to_ms_scaled(t.position_us, rate);
        let start = now_ms - elapsed_ms;
        let mut ts = Map::new();
        ts.insert("start".to_owned(), json!(start));
        if t.duration_us > 0 {
            ts.insert(
                "end".to_owned(),
                json!(start + us_to_ms_scaled(t.duration_us, rate)),
            );
        }
        o.insert("timestamps".to_owned(), Value::Object(ts));
    }

    if !activity.buttons.is_empty() {
        let buttons: Vec<Value> = activity
            .buttons
            .iter()
            .map(|b| json!({ "label": b.label, "url": b.url }))
            .collect();
        o.insert("buttons".to_owned(), Value::Array(buttons));
    }

    Value::Object(o)
}

fn us_to_ms_scaled(us: i64, rate: f64) -> i64 {
    let ms = (us as f64) / 1000.0 / rate;
    if ms.is_finite() { ms as i64 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::{ASSET_LOGO, ASSET_PAUSE, Activity, Timeline};

    fn activity(timeline: Option<Timeline>) -> Activity {
        Activity {
            activity_type: 3,
            name: "Frieren".into(),
            details: "Frieren".into(),
            state: "S1E12".into(),
            large_image: ASSET_LOGO.into(),
            large_text: "Frieren".into(),
            small_image: ASSET_PAUSE,
            small_text: "1:00 / 23:20".into(),
            timeline,
            buttons: Vec::new(),
        }
    }

    #[test]
    fn the_activity_name_carries_the_title() {
        let v = to_json(&activity(None), 0);
        assert_eq!(v.get("name").and_then(Value::as_str), Some("Frieren"));
    }

    #[test]
    fn the_primary_link_makes_the_title_and_poster_clickable() {
        use crate::projection::Button;
        let mut a = activity(None);
        a.buttons = vec![Button {
            label: "IMDb",
            url: "https://www.imdb.com/title/tt15469038/".into(),
        }];
        let v = to_json(&a, 0);
        let url = Some("https://www.imdb.com/title/tt15469038/");
        assert_eq!(v.get("details_url").and_then(Value::as_str), url);
        assert_eq!(
            v.get("assets")
                .and_then(|a| a.get("large_url"))
                .and_then(Value::as_str),
            url
        );
    }

    #[test]
    fn without_a_link_no_url_fields_are_sent() {
        let v = to_json(&activity(None), 0);
        assert!(v.get("details_url").is_none());
        assert!(v.get("assets").and_then(|a| a.get("large_url")).is_none());
    }

    #[test]
    fn no_timeline_means_no_timestamps() {
        let v = to_json(&activity(None), 1_000_000);
        assert!(v.get("timestamps").is_none());
    }

    #[test]
    fn start_is_now_minus_elapsed() {
        let v = to_json(
            &activity(Some(Timeline {
                position_us: 60_000_000,
                duration_us: 1_400_000_000,
                rate: 1.0,
            })),
            1_000_000,
        );
        let ts = match v.get("timestamps") {
            Some(t) => t,
            None => unreachable!("timestamps"),
        };
        assert_eq!(ts.get("start").and_then(Value::as_i64), Some(940_000));
        assert_eq!(ts.get("end").and_then(Value::as_i64), Some(2_340_000));
    }

    #[test]
    fn double_speed_halves_the_remaining_wall_clock() {
        let v = to_json(
            &activity(Some(Timeline {
                position_us: 0,
                duration_us: 3_600_000_000,
                rate: 2.0,
            })),
            0,
        );
        let ts = match v.get("timestamps") {
            Some(t) => t,
            None => unreachable!("timestamps"),
        };
        assert_eq!(ts.get("end").and_then(Value::as_i64), Some(1_800_000));
    }

    #[test]
    fn zero_rate_cannot_produce_a_nan() {
        let v = to_json(
            &activity(Some(Timeline {
                position_us: 60_000_000,
                duration_us: 0,
                rate: 0.0,
            })),
            0,
        );
        let ts = match v.get("timestamps") {
            Some(t) => t,
            None => unreachable!("timestamps"),
        };
        assert_eq!(ts.get("start").and_then(Value::as_i64), Some(-60_000));
    }

    #[test]
    fn buttons_serialise_as_label_url_pairs() {
        use crate::projection::Button;
        let mut a = activity(None);
        a.buttons = vec![Button {
            label: "IMDb",
            url: "https://www.imdb.com/title/tt15469038/".into(),
        }];
        let v = to_json(&a, 0);
        let arr = match v.get("buttons").and_then(Value::as_array) {
            Some(b) => b,
            None => unreachable!("buttons"),
        };
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("label").and_then(Value::as_str), Some("IMDb"));
        assert_eq!(
            arr[0].get("url").and_then(Value::as_str),
            Some("https://www.imdb.com/title/tt15469038/")
        );
    }

    #[test]
    fn an_empty_badge_is_absent_from_the_assets() {
        let mut a = activity(None);
        a.small_image = "";
        a.small_text = String::new();
        let assets = match to_json(&a, 0).get("assets").cloned() {
            Some(v) => v,
            None => unreachable!("assets"),
        };
        assert!(assets.get("small_image").is_none());
        assert!(assets.get("small_text").is_none());
        assert!(assets.get("large_image").is_some());
    }

    #[test]
    fn absent_buttons_are_omitted_entirely() {
        assert!(to_json(&activity(None), 0).get("buttons").is_none());
    }

    #[test]
    fn empty_text_fields_are_omitted() {
        let mut a = activity(None);
        a.state = String::new();
        let v = to_json(&a, 0);
        assert!(v.get("state").is_none());
        assert!(v.get("details").is_some());
    }

    #[test]
    fn live_stream_has_a_start_but_no_end() {
        let v = to_json(
            &activity(Some(Timeline {
                position_us: 5_000_000,
                duration_us: 0,
                rate: 1.0,
            })),
            100_000,
        );
        let ts = match v.get("timestamps") {
            Some(t) => t,
            None => unreachable!("timestamps"),
        };
        assert_eq!(ts.get("start").and_then(Value::as_i64), Some(95_000));
        assert!(ts.get("end").is_none());
    }
}
