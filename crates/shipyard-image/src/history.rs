//! An image's build history (step 12.2a): its config's `history` array, as
//! `docker history` shows it. Read on its own, leniently, from the config's
//! stored bytes: an entry that isn't what the spec says (a `null`, a field
//! of the wrong type) is left out, never a failure, and nothing else of the
//! config is read here, so an image that runs today never stops parsing for
//! its history (the config's own parse, `ImageConfig`, doesn't read it).

use serde_json::Value;

/// One step of an image's build, as its config records it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryEntry {
    /// When the step ran (RFC 3339, as written), if it says.
    pub created: Option<String>,
    /// The instruction that made it (`/bin/sh -c #(nop) CMD ["sh"]`).
    pub created_by: String,
    /// A comment.
    pub comment: String,
    /// Its author.
    pub author: String,
    /// Whether it made no layer (a `CMD`, an `ENV`).
    pub empty_layer: bool,
}

/// The most entries read: an image has at most 128 layers (`MAX_LAYERS`),
/// and as many empty steps again is generous.
pub const MAX_HISTORY: usize = 1024;

/// The history of the config `bytes`, oldest first: empty if it has none,
/// or isn't JSON. At most [`MAX_HISTORY`] entries; a string field past
/// 64 KiB is cut there.
#[must_use]
pub fn history(bytes: &[u8]) -> Vec<HistoryEntry> {
    let Ok(Value::Object(config)) = serde_json::from_slice::<Value>(bytes) else {
        return Vec::new();
    };
    let Some(Value::Array(entries)) = config.get("history") else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(Value::as_object)
        .take(MAX_HISTORY)
        .map(|entry| {
            let text = |key: &str| {
                entry
                    .get(key)
                    .and_then(Value::as_str)
                    .map(|text| cut(text).to_owned())
            };
            HistoryEntry {
                created: text("created"),
                created_by: text("created_by").unwrap_or_default(),
                comment: text("comment").unwrap_or_default(),
                author: text("author").unwrap_or_default(),
                empty_layer: entry
                    .get("empty_layer")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }
        })
        .collect()
}

/// `text`, at most 64 KiB of it (on a character's boundary).
fn cut(text: &str) -> &str {
    const MOST: usize = 64 * 1024;
    if text.len() <= MOST {
        return text;
    }
    let mut end = MOST;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config's history, oldest first, each field as written; an empty
    /// layer said.
    #[test]
    fn a_history_is_read_as_written() {
        let config = br#"{"history": [
            {"created": "2026-01-01T00:00:00Z", "created_by": "/bin/sh -c #(nop) ADD file:x in /"},
            {"created_by": "/bin/sh -c #(nop) CMD [\"sh\"]", "empty_layer": true, "comment": "c", "author": "a"}
        ]}"#;
        let read = history(config);
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].created.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert!(!read[0].empty_layer);
        assert_eq!(read[1].created, None);
        assert!(read[1].empty_layer);
        assert_eq!(
            (read[1].comment.as_str(), read[1].author.as_str()),
            ("c", "a")
        );
    }

    /// What isn't the spec's is left out, never a failure (12.2a's plan
    /// review: a strict parse would have stopped images that run today):
    /// no history, `null`, an entry that isn't an object, a field of the
    /// wrong type, a config that isn't JSON.
    #[test]
    fn a_history_that_isnt_the_spec_s_is_read_as_far_as_it_goes() {
        assert!(history(b"{}").is_empty());
        assert!(history(br#"{"history": null}"#).is_empty());
        assert!(history(b"not json").is_empty());
        let odd = history(
            br#"{"history": [null, 3, {"created": 5, "empty_layer": "true", "created_by": "x"}]}"#,
        );
        assert_eq!(odd.len(), 1);
        assert_eq!(odd[0].created, None);
        assert!(!odd[0].empty_layer);
        assert_eq!(odd[0].created_by, "x");
    }

    /// Bounded: at most so many entries, each string at most 64 KiB.
    #[test]
    fn a_history_is_bounded() {
        // Three bytes a character: 64 KiB falls inside one, so the cut
        // steps back to its start.
        let long = "€".repeat(30 * 1024);
        let many: Vec<String> = (0..MAX_HISTORY + 5)
            .map(|_| format!(r#"{{"created_by": "{long}"}}"#))
            .collect();
        let config = format!(r#"{{"history": [{}]}}"#, many.join(","));
        let read = history(config.as_bytes());
        assert_eq!(read.len(), MAX_HISTORY);
        assert_eq!(read[0].created_by.len(), 64 * 1024 - 1);
        assert!(read[0].created_by.chars().all(|c| c == '€'));
    }
}
