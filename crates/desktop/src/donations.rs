//! The Support window: links and texts from the DeMoZ/donations repository (file `hubbridge.json`, schema 1), as in
//! the other DeMoZ tools. The links change there without a new release of the app.
//!
//! Order: raw.githubusercontent.com, the jsDelivr mirror, the last saved copy, the copy built into the app.

use std::path::PathBuf;

use serde_json::Value;

use crate::net;

pub const FILE: &str = "hubbridge.json";
const BUILT_IN: &str = include_str!("../assets/donations.json");

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Method {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Info {
    pub message: Option<String>,
    pub methods: Vec<Method>,
}

pub fn sources() -> [String; 2] {
    [
        format!("https://raw.githubusercontent.com/DeMoZ/donations/main/{FILE}"),
        format!("https://cdn.jsdelivr.net/gh/DeMoZ/donations@main/{FILE}"),
    ]
}

/// The text in the window's language: the exact tag, the base language ("pt-BR" → "pt"), then English.
/// A string instead of an object counts as the English text.
fn text(v: Option<&Value>, languages: &[&str]) -> Option<String> {
    let v = v?;
    if let Some(s) = v.as_str() {
        return (!s.is_empty()).then(|| s.to_string());
    }
    let obj = v.as_object()?;
    let by_tag: Vec<(String, &str)> = obj
        .iter()
        .filter_map(|(k, v)| {
            v.as_str()
                .filter(|s| !s.is_empty())
                .map(|s| (k.replace('_', "-").to_lowercase(), s))
        })
        .collect();
    let find = |tag: &str| by_tag.iter().find(|(k, _)| k == tag).map(|(_, s)| s.to_string());
    for lang in languages {
        let tag = lang.replace('_', "-").to_lowercase();
        if let Some(s) = find(&tag) {
            return Some(s);
        }
        if let Some(s) = tag.split('-').next().and_then(find) {
            return Some(s);
        }
    }
    find("en")
}

/// Lenient parsing: unknown fields are skipped, a method without a title or without an https:// link is dropped.
/// `None`: not JSON, or no usable method at all.
pub fn parse(data: &[u8], lang: &str) -> Option<Info> {
    let languages = [lang];
    let v: Value = serde_json::from_slice(data).ok()?;
    let methods: Vec<Method> = v
        .get("methods")?
        .as_array()?
        .iter()
        .filter_map(|m| {
            let url = m.get("url")?.as_str()?.trim().to_string();
            if !net::is_https(&url) {
                return None;
            }
            Some(Method {
                id: m.get("id").and_then(Value::as_str).unwrap_or(&url).to_string(),
                title: text(m.get("title"), &languages)?,
                description: text(m.get("description"), &languages),
                url,
            })
        })
        .collect();
    if methods.is_empty() {
        return None;
    }
    Some(Info {
        message: text(v.get("message"), &languages),
        methods,
    })
}

fn cache_file() -> PathBuf {
    net::data_dir().join("donations.json")
}

/// The file to show at once: the last downloaded copy, or the one built into the app.
/// Kept as a whole, not parsed: every opening of the window parses it in its language.
pub fn cached() -> Vec<u8> {
    std::fs::read(cache_file())
        .ok()
        .filter(|d| parse(d, "en").is_some())
        .unwrap_or_else(|| BUILT_IN.as_bytes().to_vec())
}

/// Parsed in the window's language; an unusable file is replaced by the built-in copy.
pub fn info(data: &[u8], lang: &str) -> Info {
    parse(data, lang).unwrap_or_else(|| parse(BUILT_IN.as_bytes(), lang).expect("built-in"))
}

/// Downloads a fresh file and saves it; `None`: no source answered with a usable file.
pub fn download() -> Option<Vec<u8>> {
    download_with(|url| net::get(url, 10))
}

fn download_with(get: impl Fn(&str) -> Result<Vec<u8>, String>) -> Option<Vec<u8>> {
    for url in sources() {
        let Ok(data) = get(&url) else { continue };
        if parse(&data, "en").is_none() {
            continue;
        }
        let file = cache_file();
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(file, &data);
        return Some(data);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The environment is shared by the whole test process.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn built_in_file_is_valid() {
        let v: Value = serde_json::from_str(BUILT_IN).unwrap();
        assert_eq!(v["schema"], 1);
        for lang in ["en", "ru"] {
            let info = parse(BUILT_IN.as_bytes(), lang).unwrap();
            assert!(info.message.unwrap().contains("HubBridge"), "{lang}");
            assert_eq!(
                info.methods.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
                ["tribute", "cloudtips"]
            );
            assert!(info.methods.iter().all(|m| m.url.starts_with("https://")));
            assert!(v["message"].get(lang).is_some(), "message: no {lang}");
            for m in v["methods"].as_array().unwrap() {
                assert!(
                    m["title"].get(lang).is_some() && m["description"].get(lang).is_some(),
                    "{}: no {lang}",
                    m["id"]
                );
            }
        }
        assert!(
            parse(BUILT_IN.as_bytes(), "ru").unwrap().methods[0]
                .title
                .contains("Карта")
        );
    }

    #[test]
    fn languages_and_unsafe_links() {
        let json = r#"{"schema":1,"message":"Hello","methods":[
            {"id":"a","title":{"en":"Card","ru":"Карта"},"url":"https://pay.example/a","extra":1},
            {"id":"b","title":{"en":"Bad"},"url":"http://pay.example/b"},
            {"id":"c","title":{"en":"Js"},"url":"javascript:alert(1)"},
            {"id":"d","title":{"pt-BR":"x","en":"Only en"},"description":{"en":"desc"},"url":"https://pay.example/d"},
            {"id":"e","url":"https://pay.example/e"}
        ]}"#;
        let info = parse(json.as_bytes(), "ru").unwrap();
        assert_eq!(info.message.as_deref(), Some("Hello"));
        assert_eq!(
            info.methods.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["a", "d"]
        );
        assert_eq!(info.methods[0].title, "Карта");
        assert_eq!(info.methods[1].title, "Only en");
        assert_eq!(info.methods[1].description.as_deref(), Some("desc"));
        assert!(parse(br#"{"methods":[{"title":"x","url":"http://a"}]}"#, "en").is_none());
        assert!(parse(b"not json", "en").is_none());
    }

    #[test]
    fn base_language() {
        let v: Value = serde_json::from_str(r#"{"en":"E","pt":"P"}"#).unwrap();
        assert_eq!(text(Some(&v), &["pt-BR"]).as_deref(), Some("P"));
        assert_eq!(text(Some(&v), &["de"]).as_deref(), Some("E"));
    }

    #[test]
    fn download_keeps_cache_on_bad_data() {
        let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("hubbridge_donations_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let before = std::env::var_os("HUBBRIDGE_HOME");
        // SAFETY: tests that change the environment hold `ENV`.
        unsafe { std::env::set_var("HUBBRIDGE_HOME", &dir) };

        assert!(parse(b"not json", "en").is_none());
        // Nothing saved yet, then garbage in the cache file: the built-in copy.
        assert_eq!(cached(), BUILT_IN.as_bytes());
        std::fs::write(cache_file(), b"not json").unwrap();
        assert_eq!(cache_file(), dir.join("donations.json"));
        assert_eq!(cached(), BUILT_IN.as_bytes());

        // A usable file is saved, from the mirror when the first source gives nothing usable.
        let good = br#"{"schema":1,"methods":[{"title":"x","url":"https://pay.example/x"}]}"#;
        let fresh = download_with(|url| {
            Ok(if url.contains("jsdelivr") {
                good.to_vec()
            } else {
                b"<html>".to_vec()
            })
        });
        assert_eq!(fresh.as_deref(), Some(&good[..]));
        assert_eq!(cached(), good);

        // Bad data and errors leave the saved copy as it was.
        assert_eq!(download_with(|_| Ok(b"not json".to_vec())), None);
        assert_eq!(download_with(|_| Ok(br#"{"methods":[]}"#.to_vec())), None);
        assert_eq!(download_with(|_| Err("offline".into())), None);
        assert_eq!(cached(), good);

        // SAFETY: as above.
        unsafe {
            match before {
                Some(v) => std::env::set_var("HUBBRIDGE_HOME", v),
                None => std::env::remove_var("HUBBRIDGE_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
