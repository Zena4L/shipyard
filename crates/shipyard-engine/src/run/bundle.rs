//! `run --bundle`: an OCI bundle's run, all from its `config.json`.

use std::path::Path;

use shipyard_runtime::hooks::ContainerHooks;
use shipyard_runtime::process::{self, StopPolicy};

use super::Invocation;
use super::plan::Generated;
use crate::error::{Context as _, Result, bail};
use crate::report::{Warning, WarningKind};

/// Loads the bundle in `dir` for `run --bundle`.
pub(super) fn load_bundle(dir: &Path) -> Result<shipyard_runtime::oci::Bundle> {
    if !cfg!(target_os = "linux") {
        bail!(
            Unsupported,
            "--bundle needs Linux namespaces, which aren't available on {} yet",
            std::env::consts::OS
        );
    }
    // Without root, it runs rootless (see `load_bundle_for_caller`).
    #[cfg(target_os = "linux")]
    let bundle = shipyard_runtime::oci::load_bundle_for_caller(dir)?;
    #[cfg(not(target_os = "linux"))]
    let bundle = shipyard_runtime::oci::load_bundle(dir)?;
    Ok(bundle)
}

/// A run of `bundle`, the OCI bundle in `dir`: everything but the stop
/// policy and the streams comes from its `config.json`.
pub(super) fn from_bundle(
    dir: &Path,
    bundle: shipyard_runtime::oci::Bundle,
    stop: StopPolicy,
    stdio: process::Stdio,
) -> Result<Invocation> {
    let ignored = |message: String| Warning {
        kind: WarningKind::BundleIgnored,
        message,
    };
    let mut warnings: Vec<Warning> = bundle.warnings.into_iter().map(ignored).collect();
    if let Some(path) = &bundle.cgroups_path {
        warnings.push(ignored(format!(
            "linux.cgroupsPath ({}) is ignored by shipyard run: each run's cgroup is its own \
             (shipyard-runtime honors it)",
            path.display()
        )));
    }
    let mut isolation = bundle.isolation;
    if !bundle.hooks.is_empty() {
        // Their state names the run as its cgroup does (`run/<pid>`), and
        // the bundle by its absolute path, as `shipyard-runtime` records it.
        let dir =
            std::fs::canonicalize(dir).with_context(|| format!("the bundle {}", dir.display()))?;
        // The hooks' state is JSON, which only takes UTF-8.
        if dir.to_str().is_none() {
            bail!(
                InvalidRequest,
                "the bundle {}: a bundle with hooks needs a path that is UTF-8 (its hooks get \
                 it in JSON)",
                dir.display()
            );
        }
        isolation.hooks = Some(ContainerHooks::new(
            bundle.hooks,
            format!("run-{}", std::process::id()),
            dir,
            bundle.annotations,
        ));
    }
    warnings.extend(super::finish::root_on_host_files(&isolation));
    Ok(Invocation {
        spec: bundle.spec,
        stop,
        isolation: Some(isolation),
        stdio,
        warnings,
        generated: Generated::default(),
    })
}
