//! Tenants: who the work is for.
//!
//! Zygo's own word for "tenant" used to mean one function — one cgroup, one
//! set of limits, one warm sandbox. That is the operator's view. An embedder
//! has a different one: they have *customers*, each with scripts, secrets and
//! a budget, and every request belongs to one of them. This is that layer.
//!
//! A tenant is deliberately thin. It owns:
//!
//! * an **id**, which becomes a cgroup directory and a filename, so it is
//!   checked as strictly as a function name;
//! * the **scripts** registered for it, by digest — which is what makes
//!   deleting a tenant able to take its code with it without taking anybody
//!   else's;
//! * later, its limits, its allowlist, its secrets and its tokens (roadmap
//!   2.2, 2.7, 2.8). They are not here yet and the struct says so rather than
//!   carrying empty fields nothing reads.
//!
//! **Persisted**, unlike everything else the supervisor holds. A warm sandbox
//! does not survive a restart and is not meant to; the existence of a customer
//! is not the supervisor's to forget. One JSON file per tenant under
//! `Paths::tenants`, written whole and renamed into place, so a reader never
//! sees half of one.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::error::{IoContext, Result};
use crate::paths::Paths;
use crate::spec::SpecError;

/// One customer of whoever embedded Zygo.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Tenant {
    pub id: String,
    /// When this tenant was registered, as milliseconds since the epoch.
    ///
    /// Wall-clock rather than an `Instant`, because it is written to disk and
    /// read back by a different process than the one that wrote it.
    pub created_ms: u64,
    /// Digests of the scripts registered for this tenant.
    ///
    /// A set, and shared: two tenants that register identical bytes both name
    /// the same digest, which is the point of a content-addressed store. What
    /// it is *for* is deletion — a tenant that goes takes the scripts only it
    /// referenced.
    #[serde(default)]
    pub scripts: BTreeSet<String>,
}

impl Tenant {
    fn new(id: String) -> Tenant {
        Tenant {
            id,
            created_ms: now_ms(),
            scripts: BTreeSet::new(),
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Check a tenant id before it becomes a path or a cgroup.
///
/// The same rules a function name follows, and for the same reason: an
/// embedder passes their own customer identifiers straight through, so `../`
/// has to be a refusal here rather than a directory somewhere else. Length is
/// capped because a cgroup name is a filename.
pub fn valid_id(id: &str) -> std::result::Result<(), SpecError> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        && !id.starts_with('.');
    if ok {
        return Ok(());
    }
    Err(SpecError::invalid_with(
        "tenant",
        format!("`{id}` is not a usable tenant id"),
        "letters, digits, `-`, `_` and `.`, up to 64 of them, not starting \
         with a dot; the id becomes a cgroup and a file name",
    ))
}

/// Every tenant this host knows about.
pub struct Tenants {
    dir: PathBuf,
}

impl Tenants {
    pub fn new(paths: &Paths) -> Tenants {
        Tenants {
            dir: paths.tenants(),
        }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Register a tenant, or find the one already registered.
    ///
    /// Idempotent, like `PUT /scripts`: an embedder creating a tenant they
    /// already have is not an error worth failing a deploy over, and `existed`
    /// is there for the caller that wants to know.
    pub fn create(&self, id: &str) -> Result<(Tenant, bool)> {
        valid_id(id)?;
        if let Some(existing) = self.get(id)? {
            return Ok((existing, true));
        }
        let tenant = Tenant::new(id.to_string());
        self.write(&tenant)?;
        Ok((tenant, false))
    }

    pub fn get(&self, id: &str) -> Result<Option<Tenant>> {
        valid_id(id)?;
        let path = self.path(id);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(crate::error::Error::io(&path, e)),
        };
        match serde_json::from_str(&text) {
            Ok(tenant) => Ok(Some(tenant)),
            // A file that cannot be parsed is not "no tenant": answering
            // `None` would let a `create` overwrite it and lose whatever it
            // held. Say what is wrong and let somebody look.
            Err(e) => Err(crate::error::Error::primitive(
                "tenant store",
                "remove the file if the tenant is really gone",
                std::io::Error::other(format!("{} is not a tenant record: {e}", path.display())),
            )),
        }
    }

    pub fn list(&self) -> Result<Vec<Tenant>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(crate::error::Error::io(&self.dir, e)),
        };
        let mut tenants = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path)
                && let Ok(tenant) = serde_json::from_str::<Tenant>(&text)
            {
                tenants.push(tenant);
            }
        }
        tenants.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(tenants)
    }

    /// Remember that a tenant registered a script.
    pub fn add_script(&self, id: &str, digest: &str) -> Result<()> {
        let mut tenant = match self.get(id)? {
            Some(t) => t,
            None => self.create(id)?.0,
        };
        if tenant.scripts.insert(digest.to_string()) {
            self.write(&tenant)?;
        }
        Ok(())
    }

    /// Forget one, and say which of its scripts nothing else refers to.
    ///
    /// The caller removes those from the store; this only decides which they
    /// are, because "which scripts are now unreferenced" is a question about
    /// every tenant and not about the one being deleted.
    pub fn remove(&self, id: &str) -> Result<Option<Vec<String>>> {
        let Some(tenant) = self.get(id)? else {
            return Ok(None);
        };
        let others: BTreeSet<String> = self
            .list()?
            .into_iter()
            .filter(|t| t.id != tenant.id)
            .flat_map(|t| t.scripts)
            .collect();
        let orphaned = tenant
            .scripts
            .iter()
            .filter(|d| !others.contains(*d))
            .cloned()
            .collect();

        let path = self.path(id);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(crate::error::Error::io(&path, e)),
        }
        Ok(Some(orphaned))
    }

    /// Write a record whole, then rename it into place.
    fn write(&self, tenant: &Tenant) -> Result<()> {
        std::fs::create_dir_all(&self.dir).at(&self.dir)?;
        let body = serde_json::to_vec_pretty(tenant).map_err(|e| {
            crate::error::Error::primitive(
                "serialise",
                "the tenant record",
                std::io::Error::other(e),
            )
        })?;
        let temporary = self
            .dir
            .join(format!(".{}.{}.incoming", tenant.id, std::process::id()));
        std::fs::write(&temporary, &body).at(&temporary)?;
        let path = self.path(&tenant.id);
        std::fs::rename(&temporary, &path).at(&path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Tenants) {
        let root = tempfile::tempdir().expect("a temp dir");
        let paths = Paths::rooted(root.path());
        (root, Tenants::new(&paths))
    }

    #[test]
    fn a_tenant_survives_the_process_that_made_it() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = Paths::rooted(root.path());
        {
            let tenants = Tenants::new(&paths);
            let (tenant, existed) = tenants.create("acme").expect("create");
            assert_eq!(tenant.id, "acme");
            assert!(!existed);
            tenants.add_script("acme", "sha256:aaa").expect("add");
        }
        // A different `Tenants`, as a restarted supervisor would build.
        let tenants = Tenants::new(&paths);
        let back = tenants.get("acme").expect("get").expect("still there");
        assert!(back.scripts.contains("sha256:aaa"));
        assert_eq!(tenants.list().expect("list").len(), 1);
    }

    #[test]
    fn creating_one_twice_is_the_same_tenant() {
        let (_root, tenants) = store();
        let (first, existed) = tenants.create("acme").expect("create");
        assert!(!existed);
        tenants.add_script("acme", "sha256:aaa").expect("add");
        let (again, existed) = tenants.create("acme").expect("create again");
        assert!(existed, "a second create is not a second tenant");
        assert_eq!(first.created_ms, again.created_ms);
        assert!(
            again.scripts.contains("sha256:aaa"),
            "and it did not lose what the first one had"
        );
    }

    /// Deleting a tenant takes its scripts and nobody else's.
    #[test]
    fn only_the_scripts_nothing_else_refers_to_are_orphaned() {
        let (_root, tenants) = store();
        tenants.create("a").expect("a");
        tenants.create("b").expect("b");
        // `shared` is registered by both — the same bytes, so the same digest.
        for id in ["a", "b"] {
            tenants.add_script(id, "sha256:shared").expect("shared");
        }
        tenants.add_script("a", "sha256:only-a").expect("only-a");

        let orphaned = tenants.remove("a").expect("remove").expect("was there");
        assert_eq!(orphaned, vec!["sha256:only-a".to_string()]);
        assert!(tenants.get("a").expect("get").is_none());
        assert!(
            tenants.get("b").expect("get").expect("b").scripts.len() == 1,
            "b kept the script it registered"
        );
    }

    #[test]
    fn removing_one_that_was_never_there_is_none_not_an_error() {
        let (_root, tenants) = store();
        assert!(tenants.remove("ghost").expect("remove").is_none());
    }

    #[test]
    fn an_id_is_checked_before_it_becomes_a_path() {
        let (_root, tenants) = store();
        for bad in [
            "",
            "../../etc",
            "a/b",
            ".hidden",
            "a b",
            &"x".repeat(65),
            "acme\0",
        ] {
            assert!(valid_id(bad).is_err(), "`{bad}` was accepted");
            assert!(tenants.create(bad).is_err(), "`{bad}` was created");
        }
        for good in ["acme", "customer-42", "a.b_c", "A1"] {
            assert!(valid_id(good).is_ok(), "`{good}` was refused");
        }
    }

    /// A record that cannot be read is an error rather than an absence: a
    /// `create` that answered "no such tenant" would write over whatever the
    /// file held.
    #[test]
    fn a_damaged_record_is_not_a_missing_tenant() {
        let (_root, tenants) = store();
        tenants.create("acme").expect("create");
        std::fs::write(tenants.path("acme"), "{ not json").expect("damage");
        let err = tenants.get("acme").expect_err("a damaged record");
        assert!(format!("{err}").contains("not a tenant record"), "{err}");
    }
}
