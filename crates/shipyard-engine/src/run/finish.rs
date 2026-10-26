//! A planned run finished with what its image brings
//! ([`Planned::finish`]): its process, its root and volumes (named ones
//! made and held), and what it warns of.

use shipyard_runtime::isolation::{IdMaps, Isolation, Root};
use shipyard_runtime::process::{Env, ProcessSpec};
use shipyard_storage::{VolumeHold, VolumeMount};

use super::RunRequest;
use super::command::{add_env, add_image_env, last_wins};
use super::plan::{Generated, Invocation, Planned};
use super::prepare::ImageDefaults;
use super::volumes::{bind_etc_files, bind_volumes, hold_volumes};
use crate::Engine;
use crate::error::{Error, ErrorKind, Result};
use crate::network::RunDir;
use crate::report::{Warning, WarningKind};

/// The warning for a container that is root on the host's own files: no
/// root filesystem and no user namespace, run by the host's root.
pub(super) fn root_on_host_files(isolation: &Isolation) -> Option<Warning> {
    (isolation.root.is_none() && isolation.id_maps.is_none() && crate::network::host_root()).then(
        || Warning {
            kind: WarningKind::RootOnHostFiles,
            message: "the container is root on the host's own files: with no image or \
                      --rootfs, its root filesystem is the host's, and root in it can change \
                      them (give it an image, or a user namespace with --uidmap)"
                .to_owned(),
        },
    )
}

impl Planned {
    /// The ports this run exposes: its image's (`defaults`, if it has
    /// one), then its own (`--expose`): what `-P` publishes.
    pub(crate) fn exposed(&self, defaults: Option<&super::ImageDefaults>) -> Vec<String> {
        crate::containers::exposed_ports(
            defaults.map_or(&[][..], |defaults| &defaults.config.exposed_ports),
            &self.request.expose,
        )
    }

    /// Finishes the run with `image`'s root and defaults, if it has an
    /// image: each applies only where the request didn't say otherwise. Named
    /// volumes are made if new, and held: keep what's returned with them
    /// until the run ends.
    ///
    /// # Errors
    ///
    /// What the image's defaults make wrong (no command, a user it doesn't
    /// have), or a volume that can't be made or held.
    pub fn finish(
        self,
        engine: &Engine,
        image: Option<ImageDefaults>,
        etc: Option<&RunDir>,
    ) -> Result<(Invocation, Vec<VolumeHold>)> {
        let Self {
            request,
            stop,
            mut isolation,
            stdio,
            short_maps,
            volumes,
            network: _,
        } = self;
        let config = image.as_ref().map(|image| &image.config);
        let mut warnings: Vec<Warning> = short_maps
            .into_iter()
            .map(|message| Warning {
                kind: WarningKind::ShortIdMaps,
                message,
            })
            .collect();

        let spec = process_spec(&request, isolation.as_ref(), config, &mut warnings)?;
        let held = match isolation.as_mut() {
            Some(isolation) => root_and_mounts(engine, isolation, image, etc, volumes, &request)?,
            None => Vec::new(),
        };
        if isolation
            .as_ref()
            .and_then(|isolation| isolation.id_maps.as_ref())
            .is_some_and(IdMaps::maps_host_root)
        {
            warnings.push(Warning {
                kind: WarningKind::RootOnHostFiles,
                message: "--uidmap/--gidmap map host ID 0: a user inside is root on the host's \
                          files, which a user namespace is meant to prevent"
                    .to_owned(),
            });
        }
        warnings.extend(isolation.as_ref().and_then(root_on_host_files));
        let generated = Generated {
            hostname: isolation.is_some() && request.hostname.is_none(),
            network: isolation.is_some() && request.network.is_empty(),
            sysctls: isolation.is_some() && request.sysctl.is_empty(),
        };
        Ok((
            Invocation {
                spec,
                stop,
                isolation,
                stdio,
                warnings,
                generated,
            },
            held,
        ))
    }
}

/// The run's process: its user, environment, command, working directory and
/// limits, the image's `config` (if it has one) where the request says
/// nothing. What the image's environment holds wrong is added to `warnings`.
///
/// # Errors
///
/// No command, a user the image doesn't have, an environment entry or a
/// process spec that's wrong.
fn process_spec(
    request: &RunRequest,
    isolation: Option<&Isolation>,
    config: Option<&shipyard_image::RuntimeConfig>,
    warnings: &mut Vec<Warning>,
) -> Result<ProcessSpec> {
    // The user: an explicit --user wins; else the image's User; else the
    // isolation default (root inside) or the caller (unisolated).
    let user = request.resolve_user(isolation, config)?;

    // Environment: the image's variables first, then --env (which wins),
    // then a default PATH if none. The image's are taken as they are:
    // one without a value is never looked up here, on the host.
    let mut env = Env::new();
    if let Some(config) = config {
        add_image_env(&mut env, &config.env, warnings)?;
    }
    for entry in &request.env {
        add_env(&mut env, entry, &request.caller_env)?;
    }
    env.fill_default_path();

    let mut spec = ProcessSpec::new(request.resolve_command(config)?);
    spec.env = env;
    spec.cwd = request.resolve_workdir(config);
    spec.user = user;
    spec.rlimits = last_wins(request.ulimit.clone());
    spec.oom_score_adj = request.oom_score_adj;
    spec.validate()?;
    Ok(spec)
}

/// The run's root in `isolation`: the image's layers (if it has one), the
/// run's `/etc` files, its volumes and tmpfs mounts; all checked, then the
/// named volumes held (keep what's returned until the run ends).
///
/// # Errors
///
/// An isolation its checks refuse (an overlay on a path that can't take
/// one: the image store's), or a volume that can't be made or held.
fn root_and_mounts(
    engine: &Engine,
    isolation: &mut Isolation,
    image: Option<ImageDefaults>,
    etc: Option<&RunDir>,
    volumes: Vec<VolumeMount>,
    request: &RunRequest,
) -> Result<Vec<VolumeHold>> {
    if let Some(image) = image {
        // The image's layers become the root: an overlay mounted at
        // this path inside the container's namespaces.
        let mut root = Root::new(image.root);
        root.read_only = request.read_only;
        root.overlay = Some(image.overlay);
        isolation.root = Some(root);
    }
    if let (Some(root), Some(etc)) = (isolation.root.as_mut(), etc) {
        bind_etc_files(root, etc);
    }
    let named = match isolation.root.as_mut() {
        Some(root) => {
            let named = bind_volumes(engine, root, volumes, &request.made_sources)?;
            // Fresh at each start (`check` saw no two on one place).
            root.mounts
                .extend(request.tmpfs.iter().map(super::TmpfsMount::to_runtime));
            // A parent before what's in it (Docker's `sortMounts`):
            // a tmpfs on `/data` mounted after a volume on
            // `/data/db` would hide it, and `/etc`'s after the
            // run's own `/etc` files. In the order given among
            // equals.
            root.mounts
                .sort_by_key(|mount| mount.destination.components().count());
            named
        }
        // `check` refused volumes and tmpfs mounts without a root.
        None => Vec::new(),
    };
    // All checked before a volume is made. An overlay path that can't
    // be used is the image store's.
    isolation.validate().map_err(|err| {
        let store = matches!(
            err,
            shipyard_runtime::isolation::IsolationError::InvalidOverlayPath { .. }
        );
        if store {
            // Where the store is: the host's setting, not the request.
            Error::wrap(ErrorKind::Environment, err).context(
                "the image store's path can't be used for an overlay (set \
                 SHIPYARD_IMAGE_STORE to another directory)",
            )
        } else {
            Error::from(err)
        }
    })?;
    hold_volumes(engine, &named)
}
