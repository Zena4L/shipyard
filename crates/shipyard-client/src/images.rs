//! The Images section's data (step 14.6a): each image with its layers,
//! bottom to top, each layer's digest (`RootFS.Layers`: the same digest in
//! two images is one layer, stored once), size and the step that made it,
//! paired from the image's history; and an image's removal.

use crate::Result;
use crate::api::{ACTION_WAIT, Client, bounded, image_ref, within};

/// The most layers of one image read (the store's own bound).
pub const MAX_LAYERS: usize = 256;

/// The most of a layer's command kept, characters.
pub const MAX_COMMAND: usize = 2048;

/// An image, as the section draws it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ImageLayers {
    /// Its ID (`sha256:…`).
    pub id: String,
    /// Its tags (none: untagged).
    pub tags: Vec<String>,
    /// When it was made, seconds since the epoch (0: unknown).
    pub created: i64,
    /// Its layers, bottom to top.
    pub layers: Vec<Layer>,
}

/// One layer of an image.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Layer {
    /// Its digest (`RootFS.Layers`'s): the same in every image that has it.
    pub diff: String,
    /// Its size, bytes (0: unknown).
    pub size: u64,
    /// The step that made it (empty: unknown), cut at [`MAX_COMMAND`].
    pub created_by: String,
}

/// A history step, oldest first: what made it, and its size (0: a step
/// that made no layer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The step's command.
    pub created_by: String,
    /// Its layer's size (0: no layer).
    pub size: u64,
}

/// `diffs` (bottom to top) with the steps that made them: the steps with a
/// size, in order, one a layer. Counts that disagree (an image whose
/// history doesn't say): the layers alone, sizes and commands unknown.
#[must_use]
pub fn paired(diffs: &[String], steps: &[Step]) -> Vec<Layer> {
    let made: Vec<&Step> = steps.iter().filter(|step| step.size > 0).collect();
    let cut = |text: &str| text.chars().take(MAX_COMMAND).collect::<String>();
    diffs
        .iter()
        .take(MAX_LAYERS)
        .enumerate()
        .map(|(i, diff)| {
            let step = (made.len() == diffs.len()).then(|| made[i]);
            Layer {
                diff: diff.clone(),
                size: step.map_or(0, |step| step.size),
                created_by: step.map_or_else(String::new, |step| cut(&step.created_by)),
            }
        })
        .collect()
}

/// Every image, with its layers: listed, then each one's inspect and
/// history read.
///
/// # Errors
///
/// The API's (an image gone between the list and its read is left out).
pub async fn images(client: &Client) -> Result<Vec<ImageLayers>> {
    let docker = client.docker();
    let listed = bounded(
        "listing images",
        docker.list_images(None::<bollard::query_parameters::ListImagesOptions>),
    )
    .await??;
    let mut images = Vec::with_capacity(listed.len());
    for summary in listed {
        let inspected =
            match bounded("inspecting an image", docker.inspect_image(&summary.id)).await? {
                Ok(inspected) => inspected,
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => continue,
                Err(error) => return Err(error.into()),
            };
        // Gone since the list (removed meanwhile): left out, as at its inspect.
        let history = match bounded("an image's history", docker.image_history(&summary.id)).await?
        {
            Ok(history) => history,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => continue,
            Err(error) => return Err(error.into()),
        };
        // The API says it newest first.
        let steps: Vec<Step> = history
            .iter()
            .rev()
            .map(|item| Step {
                created_by: item.created_by.clone(),
                size: u64::try_from(item.size).unwrap_or(0),
            })
            .collect();
        let diffs = inspected
            .root_fs
            .and_then(|root| root.layers)
            .unwrap_or_default();
        images.push(ImageLayers {
            id: summary.id,
            tags: summary
                .repo_tags
                .into_iter()
                .filter(|tag| tag != "<none>:<none>")
                .collect(),
            created: summary.created,
            layers: paired(&diffs, &steps),
        });
    }
    Ok(images)
}

/// Removes image `reference` (a tag, or its ID with `force`: every tag of
/// it), as `shipyard image rm`. A container using it is the API's refusal.
///
/// # Errors
///
/// [`crate::ClientError::BadReference`] for a reference of another shape; the
/// API's.
pub async fn remove(client: &Client, reference: &str, force: bool) -> Result<()> {
    let reference = image_ref(reference)?;
    let options = bollard::query_parameters::RemoveImageOptionsBuilder::default()
        .force(force)
        .build();
    let docker = client.docker().clone().with_timeout(ACTION_WAIT);
    within(
        ACTION_WAIT,
        "removing an image",
        docker.remove_image(reference, Some(options), None),
    )
    .await??;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    fn step(created_by: &str, size: u64) -> Step {
        Step {
            created_by: created_by.to_owned(),
            size,
        }
    }

    /// A layer is the step that made it: steps that made none skipped.
    #[test]
    fn layers_are_paired_with_the_steps_that_made_them() {
        let diffs = ["sha256:a".to_owned(), "sha256:b".to_owned()];
        let steps = [
            step("ADD rootfs.tar /", 100),
            step("ENV PATH=/bin", 0),
            step("RUN apk add x", 40),
            step("CMD [\"sh\"]", 0),
        ];
        let layers = paired(&diffs, &steps);
        assert_eq!(
            layers
                .iter()
                .map(|layer| (layer.diff.as_str(), layer.size, layer.created_by.as_str()))
                .collect::<Vec<_>>(),
            [
                ("sha256:a", 100, "ADD rootfs.tar /"),
                ("sha256:b", 40, "RUN apk add x")
            ]
        );
        // A history that doesn't say as many: the layers alone.
        let unknown = paired(&diffs, &steps[..1]);
        assert!(
            unknown
                .iter()
                .all(|layer| layer.size == 0 && layer.created_by.is_empty())
        );
        assert_eq!(unknown.len(), 2);
    }

    /// An image can't make the section unbounded: its layers and their
    /// commands are cut.
    #[test]
    fn layers_are_bounded() {
        let diffs: Vec<String> = (0..MAX_LAYERS + 5).map(|i| format!("sha256:{i}")).collect();
        let steps: Vec<Step> = (0..MAX_LAYERS + 5)
            .map(|_| step(&"x".repeat(MAX_COMMAND + 9), 1))
            .collect();
        let layers = paired(&diffs, &steps);
        assert_eq!(layers.len(), MAX_LAYERS);
        assert_eq!(layers[0].created_by.chars().count(), MAX_COMMAND);
    }
}
