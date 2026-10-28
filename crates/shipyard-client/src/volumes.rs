//! The Volumes section's data (step 14.6b): each volume's size and top
//! level (`/shipyard/volumes`, its version checked), with the place its data
//! lives (Docker's `Mountpoint`); and a volume's deletion.
//!
//! The API shares one walk of the volumes for 5 seconds; Docker's list is
//! read fresh beside it and says which volumes there are, so one deleted
//! is gone at once, and one made since is there, its size not yet walked
//! (one deleted and made again under its name too: told by its time).

use std::collections::HashMap;

use crate::Result;
use crate::api::{ACTION_WAIT, Client, name_ref, within};

/// Every volume Docker's list has, each with its contents (one the shared
/// walk hasn't reached: none, not `complete`) and its `mountpoint`.
///
/// # Errors
///
/// The API's; [`crate::ClientError::Newer`] for a newer answer.
pub async fn volumes(client: &Client) -> Result<serde_json::Value> {
    let mut contents = client.volumes().await?;
    // Docker's list as JSON: `bollard`'s `created_at` is a `String` or a
    // time by its features, which another crate of the workspace can turn on.
    let listed = client.json("/volumes", "listing volumes").await?;
    let listed: Vec<Listed> = listed["Volumes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|volume| {
            Some(Listed {
                name: volume["Name"].as_str()?.to_owned(),
                created: volume["CreatedAt"].as_str().unwrap_or_default().to_owned(),
                mountpoint: volume["Mountpoint"].as_str().unwrap_or_default().to_owned(),
            })
        })
        .collect();
    joined(&mut contents, &listed);
    Ok(contents)
}

/// A volume as Docker's list says it.
struct Listed {
    name: String,
    created: String,
    mountpoint: String,
}

/// `contents`' volumes made `listed`'s, in its order: each with its place,
/// one the walk hasn't reached (or reached as another volume of its name,
/// made at another time) with no contents, one gone since left out.
fn joined(contents: &mut serde_json::Value, listed: &[Listed]) {
    let mut walked: HashMap<String, serde_json::Value> = contents["volumes"]
        .as_array_mut()
        .map(std::mem::take)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|volume| Some((volume["name"].as_str()?.to_owned(), volume)))
        .collect();
    let volumes = listed
        .iter()
        .map(|listed| {
            let walked = walked
                .remove(&listed.name)
                .filter(|volume| volume["created"] == listed.created.as_str());
            let mut volume = walked.unwrap_or_else(|| {
                serde_json::json!({
                    "name": listed.name, "created": listed.created, "size": 0,
                    "complete": false, "files": [], "more": 0, "listed_whole": false,
                })
            });
            volume["mountpoint"] = listed.mountpoint.clone().into();
            volume
        })
        .collect();
    contents["volumes"] = serde_json::Value::Array(volumes);
}

/// Deletes volume `name` and its data, as `shipyard volume rm`, never
/// forced: a container that names it (running or not) is the API's
/// refusal.
///
/// # Errors
///
/// [`crate::ClientError::BadReference`] for a name of another shape; the
/// API's.
pub async fn remove(client: &Client, name: &str) -> Result<()> {
    let name = name_ref(name)?;
    let options = bollard::query_parameters::RemoveVolumeOptions { force: false };
    let docker = client.docker().clone().with_timeout(ACTION_WAIT);
    within(
        ACTION_WAIT,
        "deleting a volume",
        docker.remove_volume(name, Some(options)),
    )
    .await??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fresh list says which volumes there are: one deleted since the
    /// shared walk is gone, one made since is there unwalked; each has its
    /// place.
    #[test]
    fn the_list_says_which_volumes_there_are() {
        let made = "2026-10-28T00:00:00Z";
        let mut contents = serde_json::json!({"version": 1, "volumes": [
            {"name": "data", "created": made, "size": 1, "complete": true},
            {"name": "deleted", "created": made, "size": 2},
            {"name": "again", "created": "2026-10-27T00:00:00Z", "size": 3, "complete": true}
        ]});
        let listed = |name: &str| Listed {
            name: name.to_owned(),
            created: made.to_owned(),
            mountpoint: format!("/v/{name}/_data"),
        };
        // `again` deleted and made anew under its name since the walk.
        joined(
            &mut contents,
            &[listed("new"), listed("data"), listed("again")],
        );
        let volumes = contents["volumes"].as_array().unwrap();
        assert_eq!(volumes.len(), 3);
        assert_eq!(
            (
                volumes[2]["size"].as_u64(),
                volumes[2]["complete"].as_bool()
            ),
            (Some(0), Some(false))
        );
        assert_eq!(volumes[0]["name"], "new");
        assert_eq!(volumes[0]["complete"], false);
        assert_eq!(volumes[0]["size"], 0);
        assert_eq!(volumes[0]["mountpoint"], "/v/new/_data");
        assert_eq!(volumes[1]["size"], 1);
        assert_eq!(volumes[1]["mountpoint"], "/v/data/_data");
    }
}
