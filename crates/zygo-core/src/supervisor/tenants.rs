// SPDX-License-Identifier: Apache-2.0
//! Tenants over the control socket: who they are, what they may use.
//!
//! A tenant is an embedder's customer. This is the supervisor's side of
//! every route that is about one: creating and forgetting them, the limits
//! that narrow their functions, the secrets sealed for them, and the
//! tokens that act for them. The stores themselves are
//! [`crate::tenants`], [`crate::secrets`] and [`crate::tokens`]; this
//! module is what checks a change against what is running before it is
//! written.

use super::{ControlError, Response, Supervisor};

/// Set to `1` to register tenants on a host whose user has no subordinate
/// uid range, where every tenant's sandbox runs as the same host uid.
pub const ALLOW_SHARED_UID_ENV: &str = "ZYGO_ALLOW_SHARED_UID";

/// Whether this host can give each tenant a host uid of its own.
///
/// Without a subordinate range, every sandbox maps to the caller's one uid,
/// and the wall between two tenants' files is only what the mounts and
/// Landlock add. That is a fine single-tenant host and a degraded
/// multi-tenant one, which is why it is refused at the moment a host becomes
/// multi-tenant — a second tenant — rather than at every launch. The
/// operator who accepts it says so once, in the environment.
///
/// Answered as `true` off Linux: the shim forwards to a VM whose own
/// supervisor makes this decision, and the unit tests are about the
/// supervisor, not about the developer machine's `/etc/subuid`.
fn uid_separation_or_allowed() -> bool {
    if cfg!(test) || std::env::var_os(ALLOW_SHARED_UID_ENV).is_some_and(|v| v == "1") {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        crate::backend::ns::idmap::subuid_range_for_current_user().is_some()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

impl Supervisor {
    /// Register a tenant, or find the one already registered.
    ///
    /// A tenant is what makes a host multi-tenant, so this is where the host
    /// is asked whether it can keep tenants apart — a subordinate uid range,
    /// or the operator's `ZYGO_ALLOW_SHARED_UID=1`.
    pub fn create_tenant(&self, id: &str) -> std::result::Result<Response, Response> {
        if !uid_separation_or_allowed() {
            return Err(Response::error(
                ControlError::BadSpec,
                format!(
                    "this host has no subordinate uid range for the user running Zygo, so \
                     every tenant's sandbox would run as the same host uid and one tenant \
                     could reach another's files. Install `uidmap` and give this user a \
                     range in /etc/subuid and /etc/subgid (`zygo doctor` says how), or set \
                     {ALLOW_SHARED_UID_ENV}=1 on the supervisor to run multi-tenant \
                     without uid separation anyway"
                ),
            ));
        }
        let (tenant, existed) = crate::tenants::Tenants::new(&self.paths)
            .create(id)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        Ok(Response::Tenants {
            tenants: vec![tenant],
            existed,
            removed_scripts: Vec::new(),
            stopped: Vec::new(),
        })
    }

    /// Every tenant, or one of them.
    pub fn tenants(&self, id: Option<&str>) -> std::result::Result<Response, Response> {
        let store = crate::tenants::Tenants::new(&self.paths);
        let tenants = match id {
            Some(id) => match store
                .get(id)
                .map_err(|e| Response::error(ControlError::BadSpec, e))?
            {
                Some(tenant) => vec![tenant],
                None => {
                    return Err(Response::error(
                        ControlError::NotFound,
                        format!("no tenant `{id}`"),
                    ));
                }
            },
            None => store
                .list()
                .map_err(|e| Response::error(ControlError::CallFailed, e))?,
        };
        Ok(Response::Tenants {
            tenants,
            existed: false,
            removed_scripts: Vec::new(),
            stopped: Vec::new(),
        })
    }

    /// Forget a tenant: stop what it was running, then take the scripts only
    /// it referred to.
    ///
    /// In that order, and the order matters. A script removed while a request
    /// is still loading it would fail that request for a reason the caller
    /// cannot see; stopping first means there is nothing left to be reading.
    pub fn delete_tenant(&self, id: &str) -> std::result::Result<Response, Response> {
        let store = crate::tenants::Tenants::new(&self.paths);
        let Some(tenant) = store
            .get(id)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?
        else {
            return Err(Response::error(
                ControlError::NotFound,
                format!("no tenant `{id}`"),
            ));
        };

        let stopped = self.stop_everything_for(id);
        // Before the record goes, not after: a token whose tenant no longer
        // exists would resolve to a customer nobody can see, and "the tenant
        // is gone but their key still opens the door" is the failure this
        // whole layer exists to prevent.
        crate::tokens::Tokens::new(&self.paths)
            .revoke_tenants(id)
            .map_err(|e| Response::error(ControlError::CallFailed, e))?;
        // And their secrets, for the same reason: a customer that is gone
        // should leave nothing of theirs on this host.
        if let Some(store) = self.secret_store() {
            store
                .remove_tenant(id)
                .map_err(|e| Response::error(ControlError::CallFailed, e))?;
        }
        let removed_scripts = store
            .remove(id)
            .map_err(|e| Response::error(ControlError::CallFailed, e))?
            .unwrap_or_default();
        let script_store = crate::scripts::ScriptStore::new(&self.paths);
        for digest in &removed_scripts {
            if let Ok(digest) = crate::scripts::ScriptDigest::parse(digest) {
                let _ = script_store.remove(&digest);
            }
        }
        // The tenant's cgroup, now that nothing of its is running. Left behind
        // it would be one empty directory per deleted customer, for ever.
        if let Ok(hierarchy) = crate::cgroup::Hierarchy::discover() {
            let _ = crate::cgroup::Hierarchy::remove(&hierarchy.tenant(id));
        }

        Ok(Response::Tenants {
            tenants: vec![tenant],
            existed: false,
            removed_scripts,
            stopped,
        })
    }

    /// Replace a tenant's limits, refusing one that could never take effect.
    ///
    /// The refusal is the useful part. A tenant's limits only ever narrow, so
    /// a value *above* every ceiling this tenant currently has is not
    /// dangerous — it simply does nothing. Storing it silently would leave an
    /// operator believing they had tightened something they had not, which is
    /// the worse outcome, so it is a `422` naming the ceiling.
    ///
    /// A tenant with nothing running has no ceiling to compare against and the
    /// value is accepted: it will narrow whatever is served later.
    pub fn set_limits(
        &self,
        tenant: &str,
        limits: crate::tenants::TenantLimits,
    ) -> std::result::Result<Response, Response> {
        crate::tenants::valid_id(tenant).map_err(|e| Response::error(ControlError::BadSpec, e))?;

        let over = self.over_every_ceiling(tenant, &limits);
        if !over.is_empty() {
            let named: Vec<String> = over
                .iter()
                .map(|(key, value)| format!("`{key}` is {value}"))
                .collect();
            return Err(Response::error(
                ControlError::AboveCeiling,
                format!(
                    "{}, which is above every ceiling tenant `{tenant}` currently                      has — a tenant's limits only narrow, so this would do nothing.                      Lower it, or raise the function's own first",
                    named.join(", ")
                ),
            ));
        }

        let updated = crate::tenants::Tenants::new(&self.paths)
            .set_limits(tenant, limits)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        Ok(Response::Tenants {
            tenants: vec![updated],
            existed: true,
            removed_scripts: Vec::new(),
            stopped: Vec::new(),
        })
    }

    /// Keys whose value is above *every* ceiling this tenant can reach.
    ///
    /// Every, not any: a tenant who can reach a small function and a large one
    /// may legitimately set a limit between them — it narrows the large one
    /// and does nothing to the small one, which is what narrowing means.
    ///
    /// **Reach**, not own. A tenant's own functions, plus every pool on the
    /// host: a pool is shared by design and `EXEC_SCRIPT` does not check whose
    /// it is — only whose *script* is. Looking at the tenant's own sandboxes
    /// alone would have found no ceiling at all in the usual shape, where the
    /// operator declares one pool and every customer calls it, and the check
    /// would have been an ornament.
    fn over_every_ceiling(
        &self,
        tenant: &str,
        limits: &crate::tenants::TenantLimits,
    ) -> Vec<(&'static str, String)> {
        let ceilings: Vec<crate::sandbox::limits::Limits> = self
            .functions
            .lock()
            .expect("registry")
            .values()
            .filter(|e| e.resolved.tenant == tenant)
            .map(|e| e.resolved.limits.clone())
            .chain(
                self.runtimes
                    .lock()
                    .expect("runtimes")
                    .values()
                    .map(|p| p.resolved.limits.clone()),
            )
            .collect();
        if ceilings.is_empty() {
            return Vec::new();
        }

        let mut over = Vec::new();
        if let Some(mem) = limits.mem
            && ceilings.iter().all(|c| mem.get() > c.mem.get())
        {
            over.push(("mem", mem.to_string()));
        }
        if let Some(cpu) = limits.cpu
            && ceilings.iter().all(|c| cpu.0 > c.cpu.0)
        {
            over.push(("cpu", cpu.to_string()));
        }
        if let Some(pids) = limits.pids
            && ceilings.iter().all(|c| pids > c.pids)
        {
            over.push(("pids", pids.to_string()));
        }
        if let Some(timeout) = limits.timeout
            && ceilings.iter().all(|c| timeout.get() > c.timeout.get())
        {
            over.push(("timeout", timeout.to_string()));
        }
        if let Some(scratch) = limits.scratch
            && ceilings.iter().all(|c| scratch.get() > c.scratch.get())
        {
            over.push(("scratch", scratch.to_string()));
        }
        over
    }

    /// A tenant's own limits, if they have any that narrow anything.
    ///
    /// `None` for the operator's own requests and for a tenant with nothing
    /// set, which is the common case and costs one file read that finds
    /// nothing.
    pub(crate) fn limits_for(
        &self,
        tenant: Option<&str>,
    ) -> std::result::Result<Option<crate::tenants::TenantLimits>, Response> {
        let Some(tenant) = tenant else {
            return Ok(None);
        };
        let found = crate::tenants::Tenants::new(&self.paths)
            .get(tenant)
            .map_err(|e| Response::error(ControlError::CallFailed, e))?;
        Ok(found.map(|t| t.limits).filter(|l| !l.is_empty()))
    }

    /// Whose function this is, for the limits above.
    pub(super) fn tenant_of(&self, name: &str) -> Option<String> {
        if let Some(entry) = self.functions.lock().expect("registry").get(name) {
            return Some(entry.resolved.tenant.clone());
        }
        self.cold
            .lock()
            .expect("cold")
            .get(name)
            .map(|c| c.resolved.tenant.clone())
    }

    /// The secret store, when this host has a key for one.
    ///
    /// `None` is a working supervisor whose secret routes refuse: a
    /// deployment with no secrets needs no key, and demanding one would be a
    /// ceremony for a risk that is not there.
    pub fn secret_store(&self) -> Option<crate::secrets::SecretStore> {
        self.secret_key
            .as_ref()
            .map(|key| crate::secrets::SecretStore::new(&self.paths, std::sync::Arc::clone(key)))
    }

    /// The store, or the error a caller gets when there is no key.
    pub(super) fn secrets_or_refuse(
        &self,
    ) -> std::result::Result<crate::secrets::SecretStore, Response> {
        self.secret_store().ok_or_else(|| {
            Response::error(
                ControlError::BadSpec,
                format!(
                    "this host has no secrets key, so it cannot store one. Set {} \
                     (or {}) to 32 bytes from `zygo secrets keygen` and restart the \
                     supervisor",
                    crate::secrets::KEY_ENV,
                    crate::secrets::KEY_FILE_ENV,
                ),
            )
        })
    }

    /// Store one of a tenant's secrets.
    pub fn put_secret(
        &self,
        tenant: &str,
        name: &str,
        value: &str,
    ) -> std::result::Result<Response, Response> {
        let store = self.secrets_or_refuse()?;
        // A secret for a tenant that does not exist would be a secret nobody
        // could ever use, and a typo nobody would notice.
        crate::tenants::Tenants::new(&self.paths)
            .create(tenant)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        store
            .put(tenant, name, value)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        self.secret_names(tenant)
    }

    /// The names a tenant has. **Never the values.**
    pub fn secret_names(&self, tenant: &str) -> std::result::Result<Response, Response> {
        let store = self.secrets_or_refuse()?;
        let names = store
            .names(tenant)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        Ok(Response::Secrets { names })
    }

    pub fn delete_secret(
        &self,
        tenant: &str,
        name: &str,
    ) -> std::result::Result<Response, Response> {
        let store = self.secrets_or_refuse()?;
        match store.remove(tenant, name) {
            Ok(true) => self.secret_names(tenant),
            Ok(false) => Err(Response::error(
                ControlError::NotFound,
                format!("tenant `{tenant}` has no secret `{name}`"),
            )),
            Err(e) => Err(Response::error(ControlError::BadSpec, e)),
        }
    }

    /// Mint an API token, and answer with the secret exactly once.
    ///
    /// Minting for a tenant registers that tenant if it is new, like
    /// `PutScript` does: onboarding a customer should be one call, not two in
    /// an order the embedder has to remember.
    pub fn mint_token(&self, tenant: Option<&str>) -> std::result::Result<Response, Response> {
        let kind = match tenant {
            Some(id) => {
                crate::tenants::Tenants::new(&self.paths)
                    .create(id)
                    .map_err(|e| Response::error(ControlError::BadSpec, e))?;
                crate::tokens::TokenKind::Tenant {
                    tenant: id.to_string(),
                }
            }
            None => crate::tokens::TokenKind::Operator,
        };
        let minted = crate::tokens::Tokens::new(&self.paths)
            .mint(kind)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        Ok(Response::Tokens {
            tokens: vec![minted.token],
            secret: Some(minted.secret),
        })
    }

    /// Every token, hashes and all. The secret is not in the store to return.
    pub fn list_tokens(&self) -> std::result::Result<Response, Response> {
        let tokens = crate::tokens::Tokens::new(&self.paths)
            .list()
            .map_err(|e| Response::error(ControlError::CallFailed, e))?;
        Ok(Response::Tokens {
            tokens,
            secret: None,
        })
    }

    /// Revoke one. The record stays, marked, so a log line naming it still
    /// resolves to something.
    pub fn revoke_token(&self, id: &str) -> std::result::Result<Response, Response> {
        crate::tokens::valid_token_id(id).map_err(|e| Response::error(ControlError::BadSpec, e))?;
        let store = crate::tokens::Tokens::new(&self.paths);
        match store.revoke(id) {
            Ok(true) => self.list_tokens(),
            Ok(false) => Err(Response::error(
                ControlError::NotFound,
                format!("no token `{id}`"),
            )),
            Err(e) => Err(Response::error(ControlError::CallFailed, e)),
        }
    }

    /// Stop every function and pool that belongs to a tenant. Returns their
    /// names.
    fn stop_everything_for(&self, tenant: &str) -> Vec<String> {
        let functions: Vec<String> = self
            .functions
            .lock()
            .expect("registry")
            .iter()
            .filter(|(_, entry)| entry.resolved.tenant == tenant)
            .map(|(name, _)| name.clone())
            .collect();
        let runtimes: Vec<String> = self
            .runtimes
            .lock()
            .expect("runtimes")
            .iter()
            .filter(|(_, pool)| pool.resolved.tenant == tenant)
            .map(|(name, _)| name.clone())
            .collect();

        let mut stopped = Vec::new();
        for name in functions {
            if self.stop(Some(&name), false).is_ok() {
                stopped.push(name);
            }
        }
        for name in runtimes {
            if self.stop_runtime(&name).is_ok() {
                stopped.push(format!("runtime.{name}"));
            }
        }
        stopped
    }
}
