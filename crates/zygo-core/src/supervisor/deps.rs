// SPDX-License-Identifier: Apache-2.0
//! Dependency sets over the control protocol.
//!
//! [`crate::deps`] knows how to build one; this is when. Three rules, and all
//! three are about the request path:
//!
//! * **`PutDeps` answers immediately.** A `pip install` is minutes. The files
//!   go on disk, the status says `building`, and the build runs on a thread of
//!   its own.
//! * **One build at a time.** Two `npm ci`s at 2 CPUs each on a host that is
//!   also serving requests is a host that stops serving them. They queue on
//!   one mutex, in the order they arrived.
//! * **A pool is never warmed without its dependencies.** A zygote warmed
//!   against a dependency set that is still building would serve requests that
//!   fail at `import`, which is worse than not serving them: the caller's
//!   retry is a code change rather than a retry. `DepsBuilding` is its own
//!   error code for exactly that reason — the right reaction is to send the
//!   same request again.

use std::collections::BTreeMap;

use super::{ControlError, Response, Supervisor};

impl Supervisor {
    /// Take the files for a dependency set and start building it.
    pub fn put_deps(
        &self,
        image: &str,
        files: &BTreeMap<String, String>,
        tenant: Option<&str>,
    ) -> std::result::Result<Response, Response> {
        let input = crate::deps::Input::read(
            crate::deps::decode_files(files)
                .map_err(|e| Response::error(ControlError::BadSpec, e))?,
        )
        .map_err(|e| Response::error(ControlError::BadSpec, e))?;

        let reference: crate::image::Reference = image
            .parse()
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        let store = crate::image::Store::new(self.paths.clone());
        let Some(entry) = store.get(&reference) else {
            return Err(Response::error(
                ControlError::NotFound,
                format!(
                    "`{image}` is not in this host's image store; pull it first — the API \
                     does not fetch images on a caller's behalf"
                ),
            ));
        };

        let (id, existing) = crate::deps::begin(&self.paths, &input, &entry, tenant)
            .map_err(|e| Response::error(ControlError::CallFailed, e))?;

        if let Some(status) = existing {
            // The same files against the same image. Whatever state it is in
            // is the answer — including `failed`, which is not rebuilt: a
            // build that failed on a typo fails the same way twice, and the
            // caller has the log.
            return Ok(Response::Dependencies {
                deps: vec![status],
                log: String::new(),
                existed: true,
            });
        }

        let paths = self.paths.clone();
        let lock = std::sync::Arc::clone(&self.deps_build);
        let building = id.clone();
        // Detached on purpose. Nothing waits for this: the caller has the id
        // and asks again, and a supervisor that went away mid-build leaves a
        // `building` on disk that `fail_interrupted` turns into a `failed` the
        // next time one starts.
        std::thread::Builder::new()
            .name(format!("zygo-deps-{}", &building[..12]))
            .spawn(move || {
                let _serialised = lock.lock().unwrap_or_else(|e| e.into_inner());
                if let Err(e) = crate::deps::build(&paths, &building) {
                    tracing::warn!(deps = building, error = %e, "the dependency build failed");
                }
            })
            .map_err(|e| {
                Response::error(
                    ControlError::CallFailed,
                    format!("could not start the build thread: {e}"),
                )
            })?;

        let status = crate::deps::status(&self.paths, &id).ok_or_else(|| {
            Response::error(
                ControlError::CallFailed,
                format!("{id} was recorded and then could not be read back"),
            )
        })?;
        Ok(Response::Dependencies {
            deps: vec![status],
            log: String::new(),
            existed: false,
        })
    }

    /// One dependency set with its log, or all of a caller's.
    pub fn deps(
        &self,
        id: Option<&str>,
        tenant: Option<&str>,
    ) -> std::result::Result<Response, Response> {
        match id {
            Some(id) => {
                let status = crate::deps::status(&self.paths, id).ok_or_else(|| {
                    Response::error(ControlError::NotFound, format!("no dependency set {id}"))
                })?;
                self.deps_owned_by(&status, tenant)?;
                Ok(Response::Dependencies {
                    log: crate::deps::log(&self.paths, id),
                    deps: vec![status],
                    existed: true,
                })
            }
            None => Ok(Response::Dependencies {
                deps: crate::deps::list(&self.paths)
                    .into_iter()
                    .filter(|s| self.deps_owned_by(s, tenant).is_ok())
                    .collect(),
                log: String::new(),
                existed: false,
            }),
        }
    }

    /// Forget a dependency set, unless a pool is built on it.
    ///
    /// Refused rather than reference-counted down to nothing: a pool holds a
    /// read-only mount of this directory, and removing it under a warm zygote
    /// would leave the pool serving requests whose imports fail one at a time.
    /// The operator stops the pool first, which is a decision rather than an
    /// accident.
    pub fn delete_deps(&self, id: &str) -> std::result::Result<Response, Response> {
        let Some(status) = crate::deps::status(&self.paths, id) else {
            return Err(Response::error(
                ControlError::NotFound,
                format!("no dependency set {id}"),
            ));
        };
        let holders: Vec<String> = self
            .runtimes
            .lock()
            .expect("runtimes")
            .iter()
            .filter(|(_, pool)| pool.deps.as_deref() == Some(id))
            .map(|(name, _)| name.clone())
            .collect();
        if !holders.is_empty() {
            return Err(Response::error(
                ControlError::BadSpec,
                format!(
                    "{id} is what {} built on; stop {} first",
                    holders.join(", "),
                    if holders.len() == 1 { "it" } else { "them" }
                ),
            ));
        }
        if status.state == crate::deps::State::Building {
            return Err(Response::error(
                ControlError::DepsBuilding,
                format!("{id} is still building; wait for it to finish, then delete it"),
            ));
        }
        crate::deps::remove(&self.paths, id)
            .map_err(|e| Response::error(ControlError::CallFailed, e))?;
        Ok(Response::Ok)
    }

    /// A tenant sees the dependency sets they uploaded; the operator sees all.
    ///
    /// `not_found` rather than `forbidden` for somebody else's, as everywhere
    /// else a tenant names something that is not theirs: the two answers are
    /// distinguishable, and the difference tells a caller whether an id they
    /// guessed exists.
    fn deps_owned_by(
        &self,
        status: &crate::deps::Status,
        tenant: Option<&str>,
    ) -> std::result::Result<(), Response> {
        match tenant {
            // The operator's own connection.
            None => Ok(()),
            Some(id) if status.tenants.iter().any(|t| t == id) => Ok(()),
            Some(_) => Err(Response::error(
                ControlError::NotFound,
                format!("no dependency set {}", status.id),
            )),
        }
    }

    /// Put a built dependency set into a pool's shape: one mount, some
    /// environment.
    ///
    /// Injected into the resolved function rather than carried through the
    /// spec, because it is not something a spec file can say: an id names
    /// bytes that arrived over the API, and a `sandbox.toml` with one in it
    /// would be a file that only works on the host that holds it.
    pub(super) fn apply_deps(
        &self,
        resolved: &mut crate::spec::ResolvedFn,
        id: &str,
    ) -> std::result::Result<crate::deps::Status, Response> {
        let Some(status) = crate::deps::status(&self.paths, id) else {
            return Err(Response::error(
                ControlError::NotFound,
                format!("no dependency set {id}"),
            ));
        };
        match status.state {
            crate::deps::State::Building => {
                return Err(Response::error(
                    ControlError::DepsBuilding,
                    format!(
                        "{id} is still building; this pool is not started, because a zygote \
                         warmed without the dependencies it was promised serves requests that \
                         fail at import"
                    ),
                ));
            }
            crate::deps::State::Failed => {
                return Err(Response::error(
                    ControlError::BadSpec,
                    format!(
                        "{id} failed to build: {}",
                        status
                            .error
                            .as_deref()
                            .unwrap_or("no reason was recorded")
                            .lines()
                            .next()
                            .unwrap_or_default()
                    ),
                ));
            }
            crate::deps::State::Ready => {}
        }

        // The dependency set was built *inside* an image, and a wheel built
        // for one interpreter fails at import in another. Checked against the
        // manifest rather than the reference, because `python:3.12-slim` moves.
        let reference: crate::image::Reference = resolved
            .image
            .parse()
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        let store = crate::image::Store::new(self.paths.clone());
        match store.get(&reference) {
            Some(entry) if entry.manifest == status.manifest => {}
            Some(entry) => {
                return Err(Response::error(
                    ControlError::BadSpec,
                    format!(
                        "{id} was built inside {} ({}) and this pool runs {} ({}); build the \
                         dependencies against the image that will run them",
                        status.image, status.manifest, resolved.image, entry.manifest
                    ),
                ));
            }
            None => {
                return Err(Response::error(
                    ControlError::NotFound,
                    format!("`{}` is not in this host's image store", resolved.image),
                ));
            }
        }

        resolved.mounts.push(crate::deps::mount(&self.paths, id));
        for (key, value) in status.kind.env() {
            // Not `insert`: an `env` the caller set explicitly is theirs, and
            // silently replacing their `PATH` with ours would be a pool that
            // ignores what it was asked for.
            resolved.env.entry(key).or_insert(value);
        }
        Ok(status)
    }
}
