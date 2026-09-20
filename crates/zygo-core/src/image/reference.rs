//! OCI image reference parsing.
//!
//! Follows the same rules as `docker pull` and `podman pull`, because
//! principle P4 says compatibility is adoption: whatever the user pastes from a
//! registry's web page has to work.

use std::fmt;
use std::str::FromStr;

use super::ImageError;

/// Registry used when a reference names none.
pub const DEFAULT_REGISTRY: &str = "docker.io";
/// Docker Hub's actual API endpoint; `docker.io` is only ever a display name.
pub const DEFAULT_REGISTRY_ENDPOINT: &str = "registry-1.docker.io";
/// Namespace prepended to single-component Docker Hub names (`alpine` →
/// `library/alpine`).
pub const DEFAULT_NAMESPACE: &str = "library";
pub const DEFAULT_TAG: &str = "latest";

/// The API endpoint for a bare registry name, without parsing a reference.
///
/// `docker.io` is a display name and has been since 2015; the requests go to
/// `registry-1.docker.io`. `zygo login docker.io` has to reach the same place
/// a pull does, or it would check a password against a host that never sees
/// one.
pub fn endpoint_for(registry: &str) -> &str {
    if registry == DEFAULT_REGISTRY {
        DEFAULT_REGISTRY_ENDPOINT
    } else {
        registry
    }
}

/// Whether a registry is one of the loopback names, which are reached over
/// plain HTTP. Same rule as [`Reference::is_insecure_local`], for a bare name.
pub fn is_insecure_local(registry: &str) -> bool {
    let host = registry.split(':').next().unwrap_or("");
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

/// A parsed reference: `[registry/]repository[:tag][@digest]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Reference {
    /// Display form of the registry, e.g. `docker.io` or `ghcr.io`.
    pub registry: String,
    /// Full repository path, e.g. `library/python` or `astral-sh/ruff`.
    pub repository: String,
    pub tag: Option<String>,
    /// `sha256:…`, when pinned.
    pub digest: Option<String>,
}

impl Reference {
    /// Host to open a connection to. Differs from [`Self::registry`] only for
    /// Docker Hub.
    pub fn endpoint(&self) -> &str {
        if self.registry == DEFAULT_REGISTRY {
            DEFAULT_REGISTRY_ENDPOINT
        } else {
            &self.registry
        }
    }

    /// What to put in a manifest URL: the digest when pinned, else the tag.
    pub fn version(&self) -> &str {
        self.digest
            .as_deref()
            .or(self.tag.as_deref())
            .unwrap_or(DEFAULT_TAG)
    }

    /// Whether this reference names an exact content digest, and so never
    /// needs to be re-resolved.
    pub fn is_pinned(&self) -> bool {
        self.digest.is_some()
    }

    /// Whether plain HTTP is acceptable. Only for loopback registries, which
    /// are overwhelmingly `localhost:5000` test registries; anything else must
    /// be TLS.
    pub fn is_insecure_local(&self) -> bool {
        is_insecure_local(&self.registry)
    }

    /// Filesystem-safe key for the image index.
    pub fn store_key(&self) -> String {
        self.to_string().replace(['/', ':', '@'], "_")
    }
}

impl FromStr for Reference {
    type Err = ImageError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let input = s.trim();
        if input.is_empty() {
            return Err(ImageError::reference(s, "empty reference"));
        }

        // Digest first: it is the only part that may contain `:` after a `@`.
        let (rest, digest) = match input.split_once('@') {
            Some((r, d)) => {
                validate_digest(d).map_err(|e| ImageError::reference(s, e))?;
                (r, Some(d.to_string()))
            }
            None => (input, None),
        };

        // A leading component is a registry only if it looks like a host: it
        // contains a dot or a colon, or is exactly `localhost`. Otherwise
        // `astral-sh/ruff` would be read as host `astral-sh`.
        let (registry, remainder) = match rest.split_once('/') {
            Some((head, tail))
                if head == "localhost" || head.contains('.') || head.contains(':') =>
            {
                (head.to_string(), tail)
            }
            _ => (DEFAULT_REGISTRY.to_string(), rest),
        };

        // The tag is after the last `:` — but only when no `/` follows it, so
        // that a port in `localhost:5000/foo` is not mistaken for a tag.
        let (name, tag) = match remainder.rsplit_once(':') {
            Some((n, t)) if !t.contains('/') => {
                validate_tag(t).map_err(|e| ImageError::reference(s, e))?;
                (n, Some(t.to_string()))
            }
            _ => (remainder, None),
        };

        if name.is_empty() {
            return Err(ImageError::reference(s, "repository name is empty"));
        }
        for component in name.split('/') {
            if component.is_empty() {
                return Err(ImageError::reference(
                    s,
                    "repository has an empty path component",
                ));
            }
            if !component.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
            }) {
                return Err(ImageError::reference(
                    s,
                    format!(
                        "`{component}` is not a valid repository component \
                         (lowercase letters, digits, `.`, `_` and `-` only)"
                    ),
                ));
            }
        }

        // Docker Hub's implicit `library/` namespace.
        let repository = if registry == DEFAULT_REGISTRY && !name.contains('/') {
            format!("{DEFAULT_NAMESPACE}/{name}")
        } else {
            name.to_string()
        };

        // An untagged, undigested reference means `:latest`.
        let tag = match (&tag, &digest) {
            (None, None) => Some(DEFAULT_TAG.to_string()),
            _ => tag,
        };

        Ok(Reference {
            registry,
            repository,
            tag,
            digest,
        })
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Round-trip the shorthand: `docker.io/library/alpine:latest` prints as
        // `alpine:latest`, which is what the user typed.
        let repo = if self.registry == DEFAULT_REGISTRY {
            match self.repository.strip_prefix("library/") {
                Some(short) if !short.contains('/') => short.to_string(),
                _ => self.repository.clone(),
            }
        } else {
            format!("{}/{}", self.registry, self.repository)
        };
        write!(f, "{repo}")?;
        if let Some(t) = &self.tag {
            write!(f, ":{t}")?;
        }
        if let Some(d) = &self.digest {
            write!(f, "@{d}")?;
        }
        Ok(())
    }
}

fn validate_digest(d: &str) -> Result<(), String> {
    let Some(hex) = d.strip_prefix("sha256:") else {
        return Err(format!(
            "unsupported digest `{d}` (only sha256 is supported)"
        ));
    };
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("`{d}` is not a 64-character sha256 digest"));
    }
    Ok(())
}

fn validate_tag(t: &str) -> Result<(), String> {
    if t.is_empty() {
        return Err("tag is empty".to_string());
    }
    if t.len() > 128 {
        return Err("tag is longer than 128 characters".to_string());
    }
    if t.starts_with(['.', '-']) {
        return Err(format!("tag `{t}` may not start with `.` or `-`"));
    }
    if !t
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(format!("tag `{t}` contains an invalid character"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> Reference {
        s.parse().unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn bare_name_gets_hub_defaults() {
        let x = r("alpine");
        assert_eq!(x.registry, "docker.io");
        assert_eq!(x.repository, "library/alpine");
        assert_eq!(x.tag.as_deref(), Some("latest"));
        assert_eq!(x.endpoint(), "registry-1.docker.io");
    }

    #[test]
    fn tagged_hub_image() {
        let x = r("python:3.12-slim");
        assert_eq!(x.repository, "library/python");
        assert_eq!(x.tag.as_deref(), Some("3.12-slim"));
        assert_eq!(x.to_string(), "python:3.12-slim");
    }

    #[test]
    fn user_namespace_is_not_mistaken_for_a_registry() {
        let x = r("windmill-labs/windmill:main");
        assert_eq!(x.registry, "docker.io");
        assert_eq!(x.repository, "windmill-labs/windmill");
    }

    #[test]
    fn explicit_registry_is_kept() {
        let x = r("ghcr.io/astral-sh/ruff:0.6.0");
        assert_eq!(x.registry, "ghcr.io");
        assert_eq!(x.repository, "astral-sh/ruff");
        assert_eq!(x.endpoint(), "ghcr.io");
        assert_eq!(x.to_string(), "ghcr.io/astral-sh/ruff:0.6.0");
    }

    #[test]
    fn a_registry_port_is_not_a_tag() {
        let x = r("localhost:5000/myimage");
        assert_eq!(x.registry, "localhost:5000");
        assert_eq!(x.repository, "myimage");
        assert_eq!(x.tag.as_deref(), Some("latest"));
        assert!(x.is_insecure_local());

        let x = r("localhost:5000/myimage:v2");
        assert_eq!(x.registry, "localhost:5000");
        assert_eq!(x.tag.as_deref(), Some("v2"));
    }

    #[test]
    fn digests_pin_and_suppress_the_default_tag() {
        let d = "sha256:".to_string() + &"a".repeat(64);
        let x = r(&format!("python@{d}"));
        assert_eq!(x.digest.as_deref(), Some(d.as_str()));
        assert_eq!(x.tag, None, "a pinned reference needs no default tag");
        assert!(x.is_pinned());
        assert_eq!(x.version(), d);
    }

    #[test]
    fn tag_and_digest_can_coexist() {
        let d = "sha256:".to_string() + &"b".repeat(64);
        let x = r(&format!("python:3.12@{d}"));
        assert_eq!(x.tag.as_deref(), Some("3.12"));
        assert_eq!(x.version(), d, "the digest wins when resolving");
    }

    #[test]
    fn non_local_registries_are_never_insecure() {
        assert!(!r("ghcr.io/a/b").is_insecure_local());
        assert!(r("127.0.0.1:5000/b").is_insecure_local());
    }

    #[test]
    fn malformed_references_are_rejected() {
        let long_tag = format!("a:{}", "x".repeat(129));
        let bad = [
            "",
            "UPPERCASE",
            "a//b",
            "a@sha256:tooshort",
            "a@md5:abc",
            "a:",
            "a:.bad",
            &long_tag,
        ];
        for s in bad {
            assert!(s.parse::<Reference>().is_err(), "`{s}` should not parse");
        }
    }

    #[test]
    fn store_keys_are_filesystem_safe() {
        let k = r("ghcr.io/astral-sh/ruff:0.6.0").store_key();
        assert!(!k.contains('/'), "{k}");
        assert!(!k.contains(':'), "{k}");
    }

    #[test]
    fn display_round_trips_through_parse() {
        for s in [
            "alpine:latest",
            "python:3.12-slim",
            "ghcr.io/astral-sh/ruff:0.6.0",
            "localhost:5000/myimage:v2",
            "windmill-labs/windmill:main",
        ] {
            assert_eq!(r(s).to_string(), s);
            assert_eq!(r(&r(s).to_string()), r(s));
        }
    }
}
