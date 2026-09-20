//! Registry authentication.
//!
//! Two pieces, both pure enough to test without a network: reading credentials
//! out of `~/.docker/config.json` (compatibility — principle P4, the user
//! should not have to log in twice), and the Bearer token dance that the OCI
//! Distribution spec defines.

use std::collections::BTreeMap;
use std::path::PathBuf;

use base64::Engine;
use serde::Deserialize;

use super::ImageError;

/// A username/password pair for one registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    pub username: String,
    pub password: String,
}

impl Credential {
    /// `Authorization: Basic …` header value.
    pub fn basic_header(&self) -> String {
        let raw = format!("{}:{}", self.username, self.password);
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(raw)
        )
    }
}

#[derive(Debug, Deserialize, serde::Serialize, Default)]
struct DockerConfig {
    #[serde(default)]
    auths: BTreeMap<String, DockerAuthEntry>,
}

#[derive(Debug, Deserialize, serde::Serialize, Default)]
struct DockerAuthEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}

/// Credentials read from Docker's config file.
#[derive(Debug, Clone, Default)]
pub struct CredentialStore {
    entries: BTreeMap<String, Credential>,
}

impl CredentialStore {
    /// Parse a `config.json`. Entries using a credential *helper* are skipped:
    /// shelling out to an arbitrary binary named by a config file is not
    /// something a sandbox runtime should do implicitly.
    pub fn parse(json: &str) -> Self {
        let cfg: DockerConfig = serde_json::from_str(json).unwrap_or_default();
        let mut entries = BTreeMap::new();

        for (host, entry) in cfg.auths {
            let cred = match (&entry.auth, &entry.username, &entry.password) {
                (Some(b64), _, _) => decode_basic(b64),
                (None, Some(u), Some(p)) => Some(Credential {
                    username: u.clone(),
                    password: p.clone(),
                }),
                _ => None,
            };
            if let Some(cred) = cred {
                entries.insert(normalise_host(&host), cred);
            }
        }

        Self { entries }
    }

    pub fn from_file(path: &PathBuf) -> Self {
        std::fs::read_to_string(path)
            .map(|s| Self::parse(&s))
            .unwrap_or_default()
    }

    /// The default location, honouring `DOCKER_CONFIG`.
    pub fn default_path() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("DOCKER_CONFIG") {
            return Some(PathBuf::from(dir).join("config.json"));
        }
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".docker/config.json"))
    }

    pub fn load_default() -> Self {
        Self::default_path()
            .map(|p| Self::from_file(&p))
            .unwrap_or_default()
    }

    /// Where `zygo login` keeps what it was told.
    ///
    /// Zygo's own directory, not `~/.docker/config.json`. Writing into
    /// another tool's configuration file is a surprise at best and a
    /// clobbered credential helper at worst, and the compatibility that
    /// matters is the *read* — principle P4 is that somebody who has already
    /// logged in with Docker does not log in again, not that Zygo starts
    /// editing Docker's files.
    pub fn zygo_path(paths: &crate::Paths) -> PathBuf {
        paths.data().join("auth.json")
    }

    /// Docker's credentials, then Zygo's own on top.
    ///
    /// Zygo's win, because `zygo login` is the more specific statement: a
    /// user who typed it meant that registry, now, with those credentials.
    pub fn load(paths: &crate::Paths) -> Self {
        let mut store = Self::load_default();
        for (host, cred) in Self::from_file(&Self::zygo_path(paths)).entries {
            store.entries.insert(host, cred);
        }
        store
    }

    /// Write the store in Docker's shape, readable by its owner alone.
    ///
    /// The same shape because there is no reason to invent another one, and
    /// because a user can then look at it, edit it, or delete a line with an
    /// editor rather than a subcommand.
    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let cfg = DockerConfig {
            auths: self
                .entries
                .iter()
                .map(|(host, cred)| {
                    (
                        host.clone(),
                        DockerAuthEntry {
                            auth: Some(
                                base64::engine::general_purpose::STANDARD
                                    .encode(format!("{}:{}", cred.username, cred.password)),
                            ),
                            username: None,
                            password: None,
                        },
                    )
                })
                .collect(),
        };
        let json = serde_json::to_string_pretty(&cfg).unwrap_or_else(|_| "{}".to_string());

        // Written with the mode it must end up with, not chmodded afterwards:
        // between `create` and `set_permissions` the file is world-readable,
        // and what it holds is a password.
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)?;
            f.write_all(json.as_bytes())
        }
        #[cfg(not(unix))]
        std::fs::write(path, json)
    }

    /// Look up a registry. Docker Hub is stored under a handful of legacy
    /// spellings, all of which mean the same thing.
    pub fn get(&self, registry: &str) -> Option<&Credential> {
        let key = normalise_host(registry);
        if let Some(c) = self.entries.get(&key) {
            return Some(c);
        }
        if key == "docker.io" || key == "registry-1.docker.io" {
            for alias in [
                "docker.io",
                "registry-1.docker.io",
                "index.docker.io",
                "https://index.docker.io/v1/",
            ] {
                if let Some(c) = self.entries.get(&normalise_host(alias)) {
                    return Some(c);
                }
            }
        }
        None
    }

    pub fn insert(&mut self, registry: &str, cred: Credential) {
        self.entries.insert(normalise_host(registry), cred);
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn decode_basic(b64: &str) -> Option<Credential> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    let text = String::from_utf8(raw).ok()?;
    // Split on the first colon only: passwords contain colons.
    let (username, password) = text.split_once(':')?;
    Some(Credential {
        username: username.to_string(),
        password: password.to_string(),
    })
}

/// Reduce a config key to a bare host: strip the scheme, any path, and the
/// `/v1/` suffix Docker has written since 2015.
fn normalise_host(host: &str) -> String {
    let h = host
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    h.split('/').next().unwrap_or(h).to_ascii_lowercase()
}

/// A parsed `WWW-Authenticate: Bearer realm="…",service="…",scope="…"` header.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Challenge {
    pub realm: String,
    pub service: Option<String>,
    pub scope: Option<String>,
}

impl Challenge {
    /// Parse the header value. Returns `None` when the scheme is not Bearer —
    /// Basic-only registries need no token exchange.
    pub fn parse(header: &str) -> Option<Challenge> {
        let rest = header
            .trim()
            .strip_prefix("Bearer ")
            .or_else(|| header.trim().strip_prefix("bearer "))?;

        let mut out = Challenge::default();
        for part in split_params(rest) {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            let value = value.trim().trim_matches('"').to_string();
            match key.trim() {
                "realm" => out.realm = value,
                "service" => out.service = Some(value),
                "scope" => out.scope = Some(value),
                _ => {}
            }
        }

        if out.realm.is_empty() {
            return None;
        }
        Some(out)
    }

    /// Token endpoint URL with the challenge's parameters, plus the scope
    /// needed to pull `repository`.
    pub fn token_url(&self, repository: &str) -> String {
        let mut url = format!("{}?", self.realm);
        let mut params = Vec::new();
        if let Some(s) = &self.service {
            params.push(format!("service={}", urlencode(s)));
        }
        // Prefer the scope the registry asked for; otherwise ask for exactly
        // what a pull needs, and nothing more.
        let scope = self
            .scope
            .clone()
            .unwrap_or_else(|| format!("repository:{repository}:pull"));
        params.push(format!("scope={}", urlencode(&scope)));
        url.push_str(&params.join("&"));
        url
    }
}

/// Split `a="1",b="2, 3",c=4` on commas that are not inside quotes.
fn split_params(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                current.push(c);
            }
            ',' if !in_quotes => {
                out.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A registry token response.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    #[serde(default)]
    pub token: Option<String>,
    /// Some registries (notably GHCR) return `access_token` instead.
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

impl TokenResponse {
    pub fn bearer(&self) -> std::result::Result<String, ImageError> {
        self.token
            .clone()
            .or_else(|| self.access_token.clone())
            .ok_or_else(|| ImageError::Registry("token response contained no token".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_saved_is_what_comes_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        let mut store = CredentialStore::default();
        store.insert(
            "ghcr.io",
            Credential {
                username: "someone".into(),
                // A colon, because the encoding splits on the first one and a
                // password is allowed to contain them.
                password: "pa:ss word".into(),
            },
        );
        store.save(&path).unwrap();

        let back = CredentialStore::from_file(&path);
        let cred = back
            .get("ghcr.io")
            .expect("the entry survived the round trip");
        assert_eq!(cred.username, "someone");
        assert_eq!(cred.password, "pa:ss word");
    }

    /// The file holds a password, so it is created with the mode it must end
    /// up with rather than chmodded afterwards — between `create` and
    /// `set_permissions` it would be readable by anyone.
    #[cfg(unix)]
    #[test]
    fn the_file_is_readable_by_its_owner_alone() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut store = CredentialStore::default();
        store.insert(
            "ghcr.io",
            Credential {
                username: "u".into(),
                password: "p".into(),
            },
        );
        store.save(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a file holding a password was {mode:o}");
    }

    /// A second `login` must not take the first one's line with it. Obvious,
    /// and exactly the kind of thing a write-the-whole-file implementation
    /// gets wrong once.
    #[test]
    fn saving_one_registry_keeps_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        let mut first = CredentialStore::default();
        first.insert(
            "ghcr.io",
            Credential {
                username: "a".into(),
                password: "1".into(),
            },
        );
        first.save(&path).unwrap();

        let mut second = CredentialStore::from_file(&path);
        second.insert(
            "registry.example.com",
            Credential {
                username: "b".into(),
                password: "2".into(),
            },
        );
        second.save(&path).unwrap();

        let back = CredentialStore::from_file(&path);
        assert!(back.get("ghcr.io").is_some(), "the first entry is gone");
        assert!(back.get("registry.example.com").is_some());
    }

    #[test]
    fn basic_auth_entries_decode() {
        let json = r#"{"auths":{"ghcr.io":{"auth":"dXNlcjpwYXNz"}}}"#;
        let store = CredentialStore::parse(json);
        assert_eq!(
            store.get("ghcr.io"),
            Some(&Credential {
                username: "user".into(),
                password: "pass".into()
            })
        );
    }

    #[test]
    fn passwords_containing_colons_survive() {
        // "user:pa:ss" — only the first colon is a separator.
        let b64 = base64::engine::general_purpose::STANDARD.encode("user:pa:ss");
        let json = format!(r#"{{"auths":{{"r.io":{{"auth":"{b64}"}}}}}}"#);
        assert_eq!(
            CredentialStore::parse(&json).get("r.io").unwrap().password,
            "pa:ss"
        );
    }

    #[test]
    fn plain_username_and_password_entries_work() {
        let json = r#"{"auths":{"r.io":{"username":"u","password":"p"}}}"#;
        assert_eq!(
            CredentialStore::parse(json).get("r.io").unwrap().username,
            "u"
        );
    }

    #[test]
    fn docker_hub_legacy_spellings_resolve() {
        let json = r#"{"auths":{"https://index.docker.io/v1/":{"auth":"dXNlcjpwYXNz"}}}"#;
        let store = CredentialStore::parse(json);
        assert!(store.get("docker.io").is_some());
        assert!(store.get("registry-1.docker.io").is_some());
    }

    #[test]
    fn credential_helper_entries_are_skipped_not_executed() {
        let json = r#"{"auths":{"r.io":{}},"credsStore":"osxkeychain"}"#;
        assert!(CredentialStore::parse(json).is_empty());
    }

    #[test]
    fn a_malformed_config_yields_no_credentials_rather_than_an_error() {
        assert!(CredentialStore::parse("not json at all").is_empty());
        assert!(CredentialStore::parse("{}").is_empty());
    }

    #[test]
    fn basic_header_is_well_formed() {
        let c = Credential {
            username: "user".into(),
            password: "pass".into(),
        };
        assert_eq!(c.basic_header(), "Basic dXNlcjpwYXNz");
    }

    #[test]
    fn bearer_challenges_parse() {
        let c = Challenge::parse(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io""#,
        )
        .unwrap();
        assert_eq!(c.realm, "https://auth.docker.io/token");
        assert_eq!(c.service.as_deref(), Some("registry.docker.io"));
        assert_eq!(c.scope, None);
    }

    #[test]
    fn a_scope_containing_commas_is_not_split() {
        let c = Challenge::parse(
            r#"Bearer realm="https://r/token",service="s",scope="repository:a/b:pull,push""#,
        )
        .unwrap();
        assert_eq!(c.scope.as_deref(), Some("repository:a/b:pull,push"));
    }

    #[test]
    fn non_bearer_challenges_are_ignored() {
        assert!(Challenge::parse("Basic realm=\"x\"").is_none());
        assert!(
            Challenge::parse("Bearer service=\"s\"").is_none(),
            "realm is required"
        );
    }

    #[test]
    fn token_urls_request_only_pull_scope_by_default() {
        let c = Challenge::parse(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io""#,
        )
        .unwrap();
        let url = c.token_url("library/python");
        assert!(url.starts_with("https://auth.docker.io/token?"), "{url}");
        assert!(url.contains("service=registry.docker.io"), "{url}");
        assert!(
            url.contains("scope=repository%3Alibrary%2Fpython%3Apull"),
            "{url}"
        );
        assert!(!url.contains("push"), "a pull must not ask for push: {url}");
    }

    #[test]
    fn a_registry_supplied_scope_is_honoured() {
        let c = Challenge::parse(r#"Bearer realm="https://r/token",scope="repository:x:pull""#)
            .unwrap();
        assert!(c.token_url("ignored").contains("repository%3Ax%3Apull"));
    }

    #[test]
    fn both_token_field_spellings_are_accepted() {
        let t: TokenResponse = serde_json::from_str(r#"{"token":"a"}"#).unwrap();
        assert_eq!(t.bearer().unwrap(), "a");
        let t: TokenResponse = serde_json::from_str(r#"{"access_token":"b"}"#).unwrap();
        assert_eq!(t.bearer().unwrap(), "b");
        let t: TokenResponse = serde_json::from_str(r#"{"expires_in":300}"#).unwrap();
        assert!(t.bearer().is_err());
    }
}
