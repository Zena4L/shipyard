//! `images`' and `history`'s rows: an image, and each of its layers, as
//! Docker's `--format` sees them.

use std::time::SystemTime;

use serde::Serialize;
use shipyard_engine::docker::ImageSummary;

use crate::format::template::Row;

use super::units::human_size;
use super::{go_time, short, unix, unix_time};

/// An image under one of its names, Docker's `ImageContext`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct ImageRow {
    #[serde(rename = "ID")]
    id: String,
    repository: String,
    tag: String,
    digest: String,
    created_since: String,
    created_at: String,
    size: String,
    shared_size: String,
    unique_size: String,
    containers: String,
}

impl ImageRow {
    /// `image`'s rows, one per name (as Docker's: an image tagged twice is
    /// two rows; one known by its digest only, `<none>` for its tag), at
    /// `now`, its ID cut short unless `full`.
    pub(crate) fn rows(image: &ImageSummary, now: SystemTime, full: bool) -> Vec<Self> {
        Self::rows_of(
            &ImageParts {
                id: &image.id,
                repo_tags: &image.repo_tags,
                repo_digests: &image.repo_digests,
                created: image.created,
                size: image.size,
                shared_size: image.shared_size,
                containers: image.containers,
            },
            now,
            full,
        )
    }

    /// [`ImageRow::rows`] of an image's parts.
    fn rows_of(image: &ImageParts<'_>, now: SystemTime, full: bool) -> Vec<Self> {
        let digest_of = |repository: &str| {
            image
                .repo_digests
                .iter()
                .find_map(|named| {
                    let (name, digest) = named.split_once('@')?;
                    (name == repository).then(|| digest.to_owned())
                })
                .unwrap_or_else(|| "<none>".to_owned())
        };
        let mut names: Vec<(String, String)> = image
            .repo_tags
            .iter()
            .map(|tagged| {
                let (repository, tag) = tagged
                    .rsplit_once(':')
                    .filter(|(_, tag)| !tag.contains('/'))
                    .unwrap_or((tagged.as_str(), "<none>"));
                (repository.to_owned(), tag.to_owned())
            })
            .collect();
        if names.is_empty() {
            names = image
                .repo_digests
                .iter()
                .filter_map(|named| named.split_once('@'))
                .map(|(repository, _)| (repository.to_owned(), "<none>".to_owned()))
                .collect();
        }
        if names.is_empty() {
            names.push(("<none>".to_owned(), "<none>".to_owned()));
        }
        let id = if full {
            image.id.to_owned()
        } else {
            short(image.id.trim_start_matches("sha256:"), false)
        };
        let created_since = if image.created > 0 {
            format!(
                "{} ago",
                shipyard_engine::docker::ago(unix_time(image.created), now)
            )
        } else {
            "N/A".to_owned()
        };
        let not_known =
            |count: i64, shown: String| if count < 0 { "N/A".to_owned() } else { shown };
        names
            .into_iter()
            .map(|(repository, tag)| Self {
                id: id.clone(),
                digest: digest_of(&repository),
                repository,
                tag,
                created_since: created_since.clone(),
                created_at: go_time(unix(image.created), 0),
                size: human_size(image.size, 3),
                shared_size: not_known(
                    image.shared_size,
                    human_size(image.shared_size.unsigned_abs(), 3),
                ),
                unique_size: not_known(
                    image.shared_size,
                    human_size(
                        image.size.saturating_sub(image.shared_size.unsigned_abs()),
                        3,
                    ),
                ),
                containers: not_known(image.containers, image.containers.to_string()),
            })
            .collect()
    }
}

impl Row for ImageRow {
    fn id(&self) -> &str {
        &self.id
    }

    fn headers() -> &'static [(&'static str, &'static str)] {
        &[
            ("ID", "IMAGE ID"),
            ("Repository", "REPOSITORY"),
            ("Tag", "TAG"),
            ("Digest", "DIGEST"),
            ("CreatedSince", "CREATED"),
            ("CreatedAt", "CREATED AT"),
            ("Size", "SIZE"),
            ("SharedSize", "SHARED SIZE"),
            ("UniqueSize", "UNIQUE SIZE"),
            ("Containers", "CONTAINERS"),
        ]
    }
}

/// What an image's rows are made of ([`ImageSummary`]'s).
struct ImageParts<'a> {
    id: &'a str,
    repo_tags: &'a [String],
    repo_digests: &'a [String],
    created: i64,
    size: u64,
    shared_size: i64,
    containers: i64,
}

/// `images`' default table (Docker's classic one).
pub(crate) const IMAGES_TABLE: &str = "table {{.Repository}}\t{{.Tag}}\t{{.ID}}\t\
     {{if .CreatedSince}}{{.CreatedSince}}{{else}}N/A{{end}}\t{{.Size}}";

/// `images --digests`' default table (Docker's).
pub(crate) const IMAGES_DIGESTS_TABLE: &str = "table {{.Repository}}\t{{.Tag}}\t{{.Digest}}\t\
     {{.ID}}\t{{if .CreatedSince}}{{.CreatedSince}}{{else}}N/A{{end}}\t{{.Size}}";

/// A step of an image's history, Docker's `historyContext` (step 12.2a).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct HistoryRow {
    #[serde(rename = "ID")]
    id: String,
    created_since: String,
    created_at: String,
    created_by: String,
    size: String,
    comment: String,
}

impl HistoryRow {
    /// `item` as Docker's `history` shows it at `now`: its ID cut to 12
    /// and its instruction to 45 characters unless `full`; times and sizes
    /// for a person (`3 weeks ago`, `7.8MB`) if `human`, else RFC 3339 and
    /// bytes.
    pub(crate) fn of(
        item: &shipyard_engine::docker::ImageHistoryItem,
        now: SystemTime,
        full: bool,
        human: bool,
    ) -> Self {
        let created_at = if item.created > 0 {
            rfc3339(item.created)
        } else {
            String::new()
        };
        let created_since = if !human {
            created_at.clone()
        } else if item.created > 0 {
            format!(
                "{} ago",
                shipyard_engine::docker::ago(unix_time(item.created), now)
            )
        } else {
            String::new()
        };
        let id = if full || item.id == "<missing>" {
            item.id.clone()
        } else {
            item.id
                .strip_prefix("sha256:")
                .unwrap_or(&item.id)
                .chars()
                .take(12)
                .collect()
        };
        let created_by = item.created_by.replace('\t', " ");
        let created_by = if full || created_by.chars().count() <= 45 {
            created_by
        } else {
            let kept: String = created_by.chars().take(44).collect();
            format!("{kept}…")
        };
        Self {
            id,
            created_since,
            created_at,
            created_by,
            size: if human {
                human_size(item.size, 3)
            } else {
                item.size.to_string()
            },
            comment: item.comment.clone(),
        }
    }
}

/// Unix `seconds` in RFC 3339 (UTC), as Go's `time.RFC3339`.
fn rfc3339(seconds: i64) -> String {
    // Shipyard's own form has nine digits of fraction: none, here.
    shipyard_runtime::logs::time::Timestamp::from_unix(seconds, 0)
        .map(|time| time.to_string().replace(".000000000Z", "Z"))
        .unwrap_or_default()
}

impl Row for HistoryRow {
    fn id(&self) -> &str {
        &self.id
    }

    fn headers() -> &'static [(&'static str, &'static str)] {
        &[
            ("ID", "IMAGE"),
            ("CreatedSince", "CREATED"),
            ("CreatedAt", "CREATED AT"),
            ("CreatedBy", "CREATED BY"),
            ("Size", "SIZE"),
            ("Comment", "COMMENT"),
        ]
    }
}

/// `history`'s default table (Docker's).
pub(crate) const HISTORY_TABLE: &str =
    "table {{.ID}}\t{{.CreatedSince}}\t{{.CreatedBy}}\t{{.Size}}\t{{.Comment}}";

#[cfg(test)]
mod tests;
