// SPDX-License-Identifier: Apache-2.0
//! API tokens, and what each one is allowed to be.
//!
//! One bearer token for the whole API answers "is this Zygo's owner?" and
//! nothing else. An embedder needs a second question answered — *whose*
//! request is this? — and needs the answer to come from something the caller
//! cannot choose. A token is that something.
//!
//! Two kinds, and deliberately not a scope lattice:
//!
//! * **Operator.** Whoever runs this Zygo. Creates tenants, serves functions
//!   and pools, mints and revokes tokens.
//! * **Tenant.** One customer. Registers scripts for themselves, calls the
//!   pools and functions the operator declared, reads their own record — and
//!   cannot see that any other tenant exists.
//!
//! A general scope system would be more flexible and would, today, encode
//! exactly these two rows with more machinery to get wrong. When an embedder
//! needs a third it will be obvious what it is; until then the enum is the
//! documentation.
//!
//! **Stored hashed.** The secret is returned once, at creation, and never
//! again — the file holds a SHA-256 and could not answer "what is Bob's
//! token" if asked. That is the same reason `/etc/shadow` holds what it holds:
//! a token store that can be read back is a token store whose theft is total.

use std::path::PathBuf;

use sha2::{Digest as _, Sha256};

use crate::error::{IoContext, Result};
use crate::paths::Paths;
use crate::spec::SpecError;

/// What a token is allowed to be.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TokenKind {
    /// Zygo's owner: everything, including minting more tokens.
    Operator,
    /// One customer: their own scripts, their own calls, nothing about
    /// anybody else.
    Tenant { tenant: String },
}

/// One token, as stored. Never the secret.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Token {
    /// Short public name, used to revoke it and printed in logs.
    pub id: String,
    #[serde(flatten)]
    pub kind: TokenKind,
    pub created_ms: u64,
    /// When it was revoked, if it was. Kept rather than deleted so that a
    /// log line naming the id still resolves to something afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_ms: Option<u64>,
    /// `sha256:…` of the secret. The only copy of anything about it.
    pub hash: String,
}

impl Token {
    pub fn tenant(&self) -> Option<&str> {
        match &self.kind {
            TokenKind::Operator => None,
            TokenKind::Tenant { tenant } => Some(tenant),
        }
    }

    pub fn is_operator(&self) -> bool {
        matches!(self.kind, TokenKind::Operator)
    }

    pub fn revoked(&self) -> bool {
        self.revoked_ms.is_some()
    }
}

/// What a caller gets back the one time a token is minted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Minted {
    pub token: Token,
    /// The secret, in the clear, exactly once. Nothing stores this.
    pub secret: String,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `sha256:…` of a presented secret, which is how one is looked up.
pub fn hash(secret: &str) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(secret.as_bytes())))
}

/// A secret with enough entropy that guessing is not a strategy.
///
/// 256 bits from the kernel, hex-encoded. Not a UUID and not a counter: this
/// is the only thing standing between a network and somebody else's scripts.
///
/// From `/dev/urandom` rather than the `getrandom` syscall, because this runs
/// on the host — in the supervisor, which has a `/dev` — and one code path
/// that works everywhere beats two that differ by platform. If the read is
/// short the token is not minted: a token built from fewer bytes than asked
/// for is worse than no token, because nothing downstream would notice.
fn secret() -> Result<String> {
    use std::io::Read as _;
    let path = std::path::Path::new("/dev/urandom");
    let mut bytes = [0u8; 32];
    let mut file = std::fs::File::open(path).at(path)?;
    file.read_exact(&mut bytes).at(path)?;
    Ok(format!("zygo_{}", hex::encode(bytes)))
}

/// The tokens on this host.
///
/// One file rather than one per token: the whole set is read on every
/// resolution, and a directory walk per request would be the wrong shape.
/// It is small — an embedder has one token per customer, not per request.
pub struct Tokens {
    path: PathBuf,
}

impl Tokens {
    pub fn new(paths: &Paths) -> Tokens {
        Tokens {
            path: paths.tokens(),
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn list(&self) -> Result<Vec<Token>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(crate::error::Error::io(&self.path, e)),
        };
        serde_json::from_str(&text).map_err(|e| {
            crate::error::Error::primitive(
                "token store",
                "the file is not a token list; move it aside to start again",
                std::io::Error::other(format!("{}: {e}", self.path.display())),
            )
        })
    }

    /// Mint one. The secret is in the answer and nowhere else, ever.
    pub fn mint(&self, kind: TokenKind) -> Result<Minted> {
        if let TokenKind::Tenant { tenant } = &kind {
            crate::tenants::valid_id(tenant)?;
        }
        let secret = secret()?;
        let token = Token {
            id: format!("tok_{}", &hash(&secret)["sha256:".len()..][..12]),
            kind,
            created_ms: now_ms(),
            revoked_ms: None,
            hash: hash(&secret),
        };
        let mut tokens = self.list()?;
        tokens.push(token.clone());
        self.write(&tokens)?;
        Ok(Minted { token, secret })
    }

    /// Revoke one by its public id. `false` when there was no such token.
    pub fn revoke(&self, id: &str) -> Result<bool> {
        let mut tokens = self.list()?;
        let Some(token) = tokens.iter_mut().find(|t| t.id == id) else {
            return Ok(false);
        };
        if token.revoked_ms.is_none() {
            token.revoked_ms = Some(now_ms());
            self.write(&tokens)?;
        }
        Ok(true)
    }

    /// Forget every token belonging to a tenant, when the tenant goes.
    pub fn revoke_tenants(&self, tenant: &str) -> Result<usize> {
        let mut tokens = self.list()?;
        let before = tokens.len();
        tokens.retain(|t| t.tenant() != Some(tenant));
        if tokens.len() != before {
            self.write(&tokens)?;
        }
        Ok(before - tokens.len())
    }

    /// Resolve a presented secret. `None` for unknown or revoked.
    ///
    /// The comparison is on the hash, so an attacker who can time this learns
    /// how long SHA-256 takes. Finding the record is a scan of a list an
    /// embedder counts in tens.
    pub fn resolve(&self, secret: &str) -> Result<Option<Token>> {
        let presented = hash(secret);
        Ok(self
            .list()?
            .into_iter()
            .find(|t| t.hash == presented && !t.revoked()))
    }

    fn write(&self, tokens: &[Token]) -> Result<()> {
        let dir = self
            .path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(dir).at(dir)?;
        let body = serde_json::to_vec_pretty(tokens).map_err(|e| {
            crate::error::Error::primitive("serialise", "the token list", std::io::Error::other(e))
        })?;
        let temporary = dir.join(format!(".tokens.{}.incoming", crate::process_token()));
        std::fs::write(&temporary, &body).at(&temporary)?;
        // `0600` before it is in place, not after: a token file that is
        // world-readable for a moment is a token file that was readable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
                .at(&temporary)?;
        }
        std::fs::rename(&temporary, &self.path).at(&self.path)?;
        Ok(())
    }
}

/// Refuse a token id that is not one this host issued.
pub fn valid_token_id(id: &str) -> std::result::Result<(), SpecError> {
    let ok = id.starts_with("tok_")
        && id.len() <= 32
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if ok {
        return Ok(());
    }
    Err(SpecError::invalid(
        "token",
        format!("`{id}` is not a token id; they look like `tok_1a2b3c4d5e6f`"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Tokens) {
        let root = tempfile::tempdir().expect("a temp dir");
        let paths = Paths::rooted(root.path());
        (root, Tokens::new(&paths))
    }

    #[test]
    fn a_secret_is_returned_once_and_never_stored() {
        let (_root, tokens) = store();
        let minted = tokens.mint(TokenKind::Operator).expect("mint");
        assert!(minted.secret.starts_with("zygo_"));
        assert!(minted.secret.len() > 40, "{}", minted.secret);

        let on_disk = std::fs::read_to_string(tokens.path()).expect("read");
        assert!(
            !on_disk.contains(&minted.secret),
            "the secret itself is in the token file"
        );
        assert!(on_disk.contains(&minted.token.hash));

        // And it still resolves, which is the whole point of the hash.
        let back = tokens.resolve(&minted.secret).expect("resolve");
        assert_eq!(back.map(|t| t.id), Some(minted.token.id));
    }

    #[test]
    fn two_tokens_are_two_secrets() {
        let (_root, tokens) = store();
        let a = tokens.mint(TokenKind::Operator).expect("a");
        let b = tokens.mint(TokenKind::Operator).expect("b");
        assert_ne!(a.secret, b.secret);
        assert_ne!(a.token.id, b.token.id);
        assert_eq!(tokens.list().expect("list").len(), 2);
    }

    #[test]
    fn a_tenant_token_names_its_tenant_and_an_operator_token_names_nobody() {
        let (_root, tokens) = store();
        let operator = tokens.mint(TokenKind::Operator).expect("operator");
        let acme = tokens
            .mint(TokenKind::Tenant {
                tenant: "acme".into(),
            })
            .expect("acme");

        assert!(operator.token.is_operator());
        assert_eq!(operator.token.tenant(), None);
        assert!(!acme.token.is_operator());
        assert_eq!(acme.token.tenant(), Some("acme"));
    }

    #[test]
    fn a_revoked_token_stops_resolving_and_stays_on_the_list() {
        let (_root, tokens) = store();
        let minted = tokens.mint(TokenKind::Operator).expect("mint");
        assert!(tokens.revoke(&minted.token.id).expect("revoke"));

        assert!(
            tokens.resolve(&minted.secret).expect("resolve").is_none(),
            "a revoked token still answers"
        );
        let listed = tokens.list().expect("list");
        assert_eq!(listed.len(), 1, "a revoked token is kept, not deleted");
        assert!(listed[0].revoked());
        assert!(!tokens.revoke("tok_nothing").expect("revoke"));
    }

    #[test]
    fn deleting_a_tenant_takes_its_tokens() {
        let (_root, tokens) = store();
        let acme = tokens
            .mint(TokenKind::Tenant {
                tenant: "acme".into(),
            })
            .expect("acme");
        let other = tokens
            .mint(TokenKind::Tenant {
                tenant: "globex".into(),
            })
            .expect("globex");

        assert_eq!(tokens.revoke_tenants("acme").expect("revoke"), 1);
        assert!(tokens.resolve(&acme.secret).expect("resolve").is_none());
        assert!(
            tokens.resolve(&other.secret).expect("resolve").is_some(),
            "another tenant's token was revoked with it"
        );
    }

    #[test]
    fn a_secret_nobody_minted_resolves_to_nothing() {
        let (_root, tokens) = store();
        tokens.mint(TokenKind::Operator).expect("mint");
        assert!(
            tokens
                .resolve("zygo_not-a-real-token")
                .expect("resolve")
                .is_none()
        );
        assert!(tokens.resolve("").expect("resolve").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn the_token_file_is_not_readable_by_anybody_else() {
        use std::os::unix::fs::PermissionsExt;
        let (_root, tokens) = store();
        tokens.mint(TokenKind::Operator).expect("mint");
        let mode = std::fs::metadata(tokens.path())
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "mode {mode:o}");
    }

    #[test]
    fn a_token_id_is_checked_before_it_is_looked_up() {
        for bad in [
            "",
            "../etc",
            "nope",
            "tok_../x",
            &format!("tok_{}", "x".repeat(40)),
        ] {
            assert!(valid_token_id(bad).is_err(), "`{bad}` was accepted");
        }
        assert!(valid_token_id("tok_1a2b3c4d5e6f").is_ok());
    }
}
