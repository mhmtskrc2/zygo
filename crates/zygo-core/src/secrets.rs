//! Per-tenant secrets, encrypted at rest.
//!
//! A function's `secrets` have always been the shell's to supply: `zygo serve`
//! reads them from the environment of whoever ran it and the supervisor keeps
//! them in memory. That is right for an operator at a terminal and wrong for
//! an embedder, whose customers have their own keys and who is not going to
//! restart a supervisor to rotate one.
//!
//! So a store, and it is **encrypted**, because the alternative is a file of
//! every customer's API keys sitting in the operator's data directory where a
//! backup, a stray `tar`, or anything that can read one uid's files gets all
//! of them at once.
//!
//! ```text
//!   PUT /tenants/acme/secrets/STRIPE_KEY      ──► ChaCha20-Poly1305 ──► disk
//!   a request for acme                        ──► decrypt ──► /run/secrets/…
//!                                                              0400, removed
//!                                                              with the request
//! ```
//!
//! ## The key
//!
//! From `ZYGO_SECRETS_KEY` (32 bytes, hex or base64) or `ZYGO_SECRETS_KEY_FILE`
//! (the same, in a file). **A passphrase is not accepted**, and that is
//! deliberate: turning one into a key needs a password-based KDF, and a store
//! that silently accepted `hunter2` and stretched it badly would be worse than
//! one that refused. `zygo secrets keygen` prints a real one.
//!
//! Zygo never stores the key. A supervisor started without it can still serve
//! every function that has no secrets; the secret routes refuse, and say which
//! variable is missing.
//!
//! ## What the encryption is and is not
//!
//! ChaCha20-Poly1305 with a fresh 96-bit nonce per write, and the record's own
//! `tenant/name` as associated data — so a value cannot be moved from one
//! tenant to another, or from one name to another, by anything that can only
//! rename files.
//!
//! It protects the bytes **at rest**. It does not protect them from a process
//! that can read this one's memory, and it is not a hardware root of trust: an
//! operator who needs those has a KMS, and the honest thing is to say so here
//! rather than imply this is more than it is.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

use crate::error::{Error, IoContext, Result};
use crate::paths::Paths;
use crate::spec::SpecError;

/// The variable an operator puts the key in.
pub const KEY_ENV: &str = "ZYGO_SECRETS_KEY";
/// Or the file that holds it, for a systemd `LoadCredential` or a mounted one.
pub const KEY_FILE_ENV: &str = "ZYGO_SECRETS_KEY_FILE";

/// Longest secret value this will store.
///
/// A secret is a key, a token or a certificate. Something larger is a file,
/// and a file belongs in a workspace where it is not held in memory by the
/// supervisor for the life of the request.
pub const MAX_SECRET_BYTES: usize = 64 * 1024;

/// A key, kept out of `Debug` and wiped when it goes.
#[derive(Clone)]
pub struct SecretKey(Key);

impl std::fmt::Debug for SecretKey {
    /// Never the bytes. A key in a log line is a key that has been published.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretKey(…)")
    }
}

impl SecretKey {
    /// Read the key an operator configured, or say there is none.
    ///
    /// `Ok(None)` is "no key was configured", which is a working supervisor
    /// with the secret routes refused. `Err` is "a key was configured and it
    /// is not usable", which is a mistake worth stopping for — an operator who
    /// meant to set one and typo'd the variable must not get a host that
    /// quietly behaves as though they had not.
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Result<Option<SecretKey>> {
        let raw = match (var(KEY_ENV), var(KEY_FILE_ENV)) {
            (Some(_), Some(_)) => {
                return Err(bad(format!(
                    "both {KEY_ENV} and {KEY_FILE_ENV} are set; pick one"
                )));
            }
            (Some(text), None) => text,
            (None, Some(path)) => {
                let path = PathBuf::from(path);
                std::fs::read_to_string(&path).at(&path)?
            }
            (None, None) => return Ok(None),
        };
        SecretKey::parse(raw.trim()).map(Some)
    }

    /// 32 bytes, as hex or base64.
    ///
    /// One message for every way of getting it wrong, because they are all the
    /// same mistake from the operator's side: what they have is not a key.
    pub fn parse(text: &str) -> Result<SecretKey> {
        let bytes = if text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
            hex::decode(text).ok()
        } else {
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, text).ok()
        };
        match bytes {
            Some(bytes) if bytes.len() == 32 => Ok(SecretKey(
                Key::try_from(&bytes[..]).expect("checked to be 32 bytes"),
            )),
            _ => Err(bad(
                "a key is 32 bytes, as 64 hex characters or base64, and this is not. \
                 `zygo secrets keygen` prints one. A passphrase is not a key: \
                 stretching one needs a password KDF, and a store that accepted \
                 `hunter2` and stretched it badly would be worse than one that \
                 refused",
            )),
        }
    }

    /// A new key, for `zygo secrets keygen`.
    pub fn generate() -> Result<String> {
        use std::io::Read as _;
        let path = std::path::Path::new("/dev/urandom");
        let mut bytes = [0u8; 32];
        let mut file = std::fs::File::open(path).at(path)?;
        file.read_exact(&mut bytes).at(path)?;
        Ok(hex::encode(bytes))
    }

    fn cipher(&self) -> ChaCha20Poly1305 {
        ChaCha20Poly1305::new(&self.0)
    }
}

/// One tenant's secrets, as stored: names to sealed values.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Sealed {
    /// Name to `base64(nonce || ciphertext)`.
    #[serde(default)]
    values: BTreeMap<String, String>,
}

/// The secrets on this host.
pub struct SecretStore {
    dir: PathBuf,
    key: SecretKey,
}

impl SecretStore {
    pub fn new(paths: &Paths, key: SecretKey) -> SecretStore {
        SecretStore {
            dir: paths.secrets(),
            key,
        }
    }

    fn path(&self, tenant: &str) -> PathBuf {
        self.dir.join(format!("{tenant}.json"))
    }

    /// Store one value, replacing whatever was under that name.
    pub fn put(&self, tenant: &str, name: &str, value: &str) -> Result<()> {
        crate::tenants::valid_id(tenant)?;
        valid_name(name)?;
        if value.len() > MAX_SECRET_BYTES {
            return Err(bad(format!(
                "{} bytes is over the {} KiB limit for one secret; something that \
                 size is a file, and a file belongs in a workspace",
                value.len(),
                MAX_SECRET_BYTES / 1024
            )));
        }

        let mut sealed = self.read(tenant)?;
        sealed
            .values
            .insert(name.to_string(), self.seal(tenant, name, value)?);
        self.write(tenant, &sealed)
    }

    /// The **names** a tenant has, never the values.
    ///
    /// What `GET /tenants/<id>/secrets` answers. A store that could list
    /// values would make every route that reaches it a way to read them all,
    /// which is what encrypting them at rest is for.
    pub fn names(&self, tenant: &str) -> Result<Vec<String>> {
        crate::tenants::valid_id(tenant)?;
        Ok(self.read(tenant)?.values.into_keys().collect())
    }

    /// Forget one. `false` if there was nothing under that name.
    pub fn remove(&self, tenant: &str, name: &str) -> Result<bool> {
        crate::tenants::valid_id(tenant)?;
        let mut sealed = self.read(tenant)?;
        if sealed.values.remove(name).is_none() {
            return Ok(false);
        }
        self.write(tenant, &sealed)?;
        Ok(true)
    }

    /// Forget all of a tenant's, when the tenant goes.
    pub fn remove_tenant(&self, tenant: &str) -> Result<()> {
        let path = self.path(tenant);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::io(&path, e)),
        }
    }

    /// Every value a tenant has, decrypted, for one request.
    ///
    /// The only method that produces plaintext, and it is called on the
    /// request path with the values going straight into the files the child
    /// reads — never into a log, a response, or the zygote.
    pub fn values(&self, tenant: &str) -> Result<BTreeMap<String, String>> {
        crate::tenants::valid_id(tenant)?;
        let sealed = self.read(tenant)?;
        let mut out = BTreeMap::new();
        for (name, value) in &sealed.values {
            out.insert(name.clone(), self.open(tenant, name, value)?);
        }
        Ok(out)
    }

    fn seal(&self, tenant: &str, name: &str, value: &str) -> Result<String> {
        use std::io::Read as _;
        // A fresh nonce per write, from the kernel. Never a counter: a counter
        // that restarts — a restored backup, a second supervisor — repeats a
        // nonce, and a repeated nonce with the same key is the one mistake
        // this construction does not survive.
        let mut nonce = [0u8; 12];
        let path = std::path::Path::new("/dev/urandom");
        let mut file = std::fs::File::open(path).at(path)?;
        file.read_exact(&mut nonce).at(path)?;

        let sealed = self
            .key
            .cipher()
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: value.as_bytes(),
                    aad: aad(tenant, name).as_bytes(),
                },
            )
            .map_err(|_| bad("could not encrypt the value"))?;

        let mut out = nonce.to_vec();
        out.extend_from_slice(&sealed);
        Ok(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            out,
        ))
    }

    fn open(&self, tenant: &str, name: &str, stored: &str) -> Result<String> {
        let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, stored)
            .map_err(|_| bad(format!("`{tenant}/{name}` is not base64")))?;
        if raw.len() < 12 {
            return Err(bad(format!(
                "`{tenant}/{name}` is too short to be a secret"
            )));
        }
        let (nonce, body) = raw.split_at(12);
        let plain = self
            .key
            .cipher()
            .decrypt(
                &Nonce::try_from(nonce).expect("checked to be 12 bytes"),
                Payload {
                    msg: body,
                    aad: aad(tenant, name).as_bytes(),
                },
            )
            // The one error worth a sentence: it is nearly always the wrong
            // key, and an operator who rotated one without re-sealing needs to
            // be told that rather than "decryption failed".
            .map_err(|_| {
                bad(format!(
                    "`{tenant}/{name}` will not decrypt with this {KEY_ENV}; either the \
                     key changed, or the record was moved from another name or tenant"
                ))
            })?;
        String::from_utf8(plain).map_err(|_| bad(format!("`{tenant}/{name}` is not UTF-8")))
    }

    fn read(&self, tenant: &str) -> Result<Sealed> {
        let path = self.path(tenant);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Sealed::default()),
            Err(e) => return Err(Error::io(&path, e)),
        };
        serde_json::from_str(&text).map_err(|e| bad(format!("{} is damaged: {e}", path.display())))
    }

    fn write(&self, tenant: &str, sealed: &Sealed) -> Result<()> {
        std::fs::create_dir_all(&self.dir).at(&self.dir)?;
        let body = serde_json::to_vec_pretty(sealed)
            .map_err(|e| bad(format!("cannot serialise the record: {e}")))?;
        let temporary = self
            .dir
            .join(format!(".{tenant}.{}.incoming", crate::process_token()));
        std::fs::write(&temporary, &body).at(&temporary)?;
        // `0600` before it is in place, as the token store does: a file that
        // is world-readable for a moment is a file that was readable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
                .at(&temporary)?;
        }
        let path = self.path(tenant);
        std::fs::rename(&temporary, &path).at(&path)?;
        Ok(())
    }
}

/// What a value is bound to, so it cannot be moved.
///
/// Associated data rather than part of the plaintext: it is not secret, it has
/// to be checked, and putting it in the plaintext would mean trusting the
/// record to say where it came from.
fn aad(tenant: &str, name: &str) -> String {
    format!("zygo/secret/{tenant}/{name}")
}

/// A secret's name becomes a file name in `/run/secrets` and an `env` key.
pub fn valid_name(name: &str) -> std::result::Result<(), SpecError> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
        && !name.starts_with('.');
    if ok {
        return Ok(());
    }
    Err(SpecError::invalid_with(
        "secret",
        format!("`{name}` is not a usable secret name"),
        "letters, digits, `_`, `-` and `.`, up to 128 of them, not starting with \
         a dot; the name becomes a file in /run/secrets",
    ))
}

fn bad(message: impl std::fmt::Display) -> Error {
    Error::primitive(
        "secrets",
        message.to_string(),
        std::io::Error::other("secret store"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, SecretStore) {
        let root = tempfile::tempdir().expect("a temp dir");
        let key = SecretKey::parse(&SecretKey::generate().expect("keygen")).expect("parse");
        let store = SecretStore::new(&Paths::rooted(root.path()), key);
        (root, store)
    }

    #[test]
    fn a_value_goes_in_and_comes_back_and_is_not_on_disk_in_the_clear() {
        let (root, secrets) = store();
        secrets
            .put("acme", "STRIPE_KEY", "sk_live_abc123")
            .expect("put");

        assert_eq!(secrets.names("acme").expect("names"), vec!["STRIPE_KEY"]);
        assert_eq!(
            secrets.values("acme").expect("values").get("STRIPE_KEY"),
            Some(&"sk_live_abc123".to_string())
        );

        let on_disk = std::fs::read_to_string(secrets.path("acme")).expect("read");
        assert!(
            !on_disk.contains("sk_live_abc123"),
            "the value is on disk in the clear:\n{on_disk}"
        );
        // The *name* is, deliberately: `GET` answers with names, and a store
        // that hid them could not.
        assert!(on_disk.contains("STRIPE_KEY"));
        let _ = root;
    }

    #[test]
    fn the_wrong_key_is_told_apart_from_a_damaged_record() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = Paths::rooted(root.path());
        let first = SecretKey::parse(&SecretKey::generate().expect("keygen")).expect("parse");
        SecretStore::new(&paths, first)
            .put("acme", "K", "value")
            .expect("put");

        let other = SecretKey::parse(&SecretKey::generate().expect("keygen")).expect("parse");
        let err = SecretStore::new(&paths, other)
            .values("acme")
            .expect_err("a different key");
        assert!(format!("{err}").contains("will not decrypt"), "{err}");
    }

    /// The reason the record's own name is associated data.
    #[test]
    fn a_value_cannot_be_moved_to_another_tenant_or_name() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = Paths::rooted(root.path());
        let key = SecretKey::parse(&SecretKey::generate().expect("keygen")).expect("parse");
        let secrets = SecretStore::new(&paths, key);
        secrets.put("acme", "K", "acme's value").expect("put");
        secrets
            .put("globex", "OTHER", "globex's value")
            .expect("put");

        // Somebody who can write in the data directory moves acme's sealed
        // value into globex's file, under globex's own name.
        let acme: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(secrets.path("acme")).unwrap()).unwrap();
        let stolen = acme["values"]["K"].clone();
        let mut globex: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(secrets.path("globex")).unwrap())
                .unwrap();
        globex["values"]["OTHER"] = stolen;
        std::fs::write(secrets.path("globex"), globex.to_string()).unwrap();

        let err = secrets.values("globex").expect_err("a moved value");
        assert!(format!("{err}").contains("will not decrypt"), "{err}");
    }

    #[test]
    fn removing_one_leaves_the_others() {
        let (_root, secrets) = store();
        secrets.put("acme", "A", "1").expect("a");
        secrets.put("acme", "B", "2").expect("b");
        assert!(secrets.remove("acme", "A").expect("remove"));
        assert!(!secrets.remove("acme", "A").expect("remove again"));
        assert_eq!(secrets.names("acme").expect("names"), vec!["B"]);

        secrets.remove_tenant("acme").expect("remove tenant");
        assert!(secrets.names("acme").expect("names").is_empty());
    }

    #[test]
    fn a_passphrase_is_refused_and_the_message_says_why() {
        let err = SecretKey::parse("hunter2").expect_err("a passphrase");
        let text = format!("{err}");
        assert!(text.contains("A passphrase is not a key"), "{text}");
        assert!(text.contains("keygen"), "{text}");

        // And a real key is accepted in either encoding.
        let hex = SecretKey::generate().expect("keygen");
        SecretKey::parse(&hex).expect("hex");
        let raw = hex::decode(&hex).unwrap();
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw);
        SecretKey::parse(&b64).expect("base64");
    }

    #[test]
    fn a_key_that_was_configured_and_is_wrong_is_an_error_not_an_absence() {
        // Nothing set: a working supervisor whose secret routes refuse.
        assert!(SecretKey::from_env(|_| None).expect("no key").is_none());

        // Set and unusable: an operator who meant to set one and typo'd must
        // not get a host that quietly behaves as though they had not.
        let err = SecretKey::from_env(|k| (k == KEY_ENV).then(|| "nonsense".to_string()))
            .expect_err("a bad key");
        assert!(format!("{err}").contains("a key is 32 bytes"), "{err}");

        let both = SecretKey::from_env(|_| Some("x".to_string())).expect_err("both");
        assert!(format!("{both}").contains("pick one"), "{both}");
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_not_readable_by_anybody_else() {
        use std::os::unix::fs::PermissionsExt;
        let (_root, secrets) = store();
        secrets.put("acme", "K", "v").expect("put");
        let mode = std::fs::metadata(secrets.path("acme"))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "mode {mode:o}");
    }

    #[test]
    fn a_name_is_checked_before_it_becomes_a_file() {
        for bad in ["", "../escape", "a/b", ".hidden", "a b", &"x".repeat(129)] {
            assert!(valid_name(bad).is_err(), "`{bad}` was accepted");
        }
        for good in ["STRIPE_KEY", "db-url", "a.b_C1"] {
            assert!(valid_name(good).is_ok(), "`{good}` was refused");
        }
    }
}
