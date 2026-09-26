// SPDX-License-Identifier: Apache-2.0
//! OCI manifest and config types.
//!
//! Only the fields Zygo acts on are modelled; the rest of the document is
//! ignored on purpose, so a registry adding a field does not break a pull.

use serde::{Deserialize, Serialize};

pub const MEDIA_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const MEDIA_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const MEDIA_DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
pub const MEDIA_DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";

/// `Accept` header for a manifest request: everything Zygo can read, most
/// preferred first.
pub const MANIFEST_ACCEPT: &str = concat!(
    "application/vnd.oci.image.index.v1+json",
    ", application/vnd.oci.image.manifest.v1+json",
    ", application/vnd.docker.distribution.manifest.list.v2+json",
    ", application/vnd.docker.distribution.manifest.v2+json",
);

/// A blob reference inside a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Descriptor {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub digest: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub architecture: String,
    pub os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

impl Platform {
    /// The platform of the host this binary is running on, in OCI spelling.
    pub fn host() -> Platform {
        let architecture = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            "arm" => "arm",
            "powerpc64" => "ppc64le",
            "s390x" => "s390x",
            "riscv64" => "riscv64",
            other => other,
        }
        .to_string();
        Platform {
            architecture,
            // Images are always Linux: even the `vm` backend boots a Linux
            // guest, and macOS runs Zygo inside a Linux VM.
            os: "linux".to_string(),
            variant: None,
        }
    }

    /// Whether `self` (from a registry index) satisfies `want`.
    pub fn satisfies(&self, want: &Platform) -> bool {
        if self.os != want.os || self.architecture != want.architecture {
            return false;
        }
        // A missing variant on either side means "don't care"; `arm64/v8` and
        // `arm64` are the same thing in practice.
        match (&self.variant, &want.variant) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }
}

/// A multi-platform index (OCI) or manifest list (Docker).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Index {
    #[serde(rename = "schemaVersion", default)]
    pub schema_version: u32,
    #[serde(rename = "mediaType", default)]
    pub media_type: String,
    #[serde(default)]
    pub manifests: Vec<Descriptor>,
}

impl Index {
    /// Pick the manifest for a platform.
    pub fn select(&self, want: &Platform) -> Option<&Descriptor> {
        self.manifests.iter().find(|d| {
            // Attestation and signature entries carry `unknown/unknown` and
            // must never be selected as an image.
            d.platform
                .as_ref()
                .is_some_and(|p| p.satisfies(want) && p.architecture != "unknown")
        })
    }

    /// Platforms the index actually offers, for the "no match" error message.
    pub fn available(&self) -> Vec<String> {
        self.manifests
            .iter()
            .filter_map(|d| d.platform.as_ref())
            .filter(|p| p.architecture != "unknown")
            .map(|p| match &p.variant {
                Some(v) => format!("{}/{}/{v}", p.os, p.architecture),
                None => format!("{}/{}", p.os, p.architecture),
            })
            .collect()
    }
}

/// A single-platform image manifest.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Manifest {
    #[serde(rename = "schemaVersion", default)]
    pub schema_version: u32,
    #[serde(rename = "mediaType", default)]
    pub media_type: String,
    pub config: Descriptor,
    #[serde(default)]
    pub layers: Vec<Descriptor>,
}

/// The parts of the image config Zygo uses to build a default command and
/// environment, so `zygo run <image>` with no command behaves like
/// `docker run <image>`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ImageConfig {
    #[serde(default)]
    pub architecture: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub config: ConfigBody,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ConfigBody {
    #[serde(rename = "Env", default)]
    pub env: Vec<String>,
    #[serde(rename = "Cmd", default)]
    pub cmd: Option<Vec<String>>,
    #[serde(rename = "Entrypoint", default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(rename = "WorkingDir", default)]
    pub working_dir: Option<String>,
    #[serde(rename = "User", default)]
    pub user: Option<String>,
}

impl ImageConfig {
    /// Argv the image would run by default: entrypoint followed by cmd, the
    /// same composition Docker uses.
    pub fn default_argv(&self) -> Vec<String> {
        let mut argv = self.config.entrypoint.clone().unwrap_or_default();
        argv.extend(self.config.cmd.clone().unwrap_or_default());
        argv
    }

    /// Image environment as key/value pairs, skipping malformed entries.
    pub fn env_pairs(&self) -> Vec<(String, String)> {
        self.config
            .env
            .iter()
            .filter_map(|e| {
                e.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect()
    }
}

/// The widest zstd window a stream Zygo unpacks may ask for: 256 MiB.
///
/// A zstd frame names the window the decoder must hold in memory. ruzstd 0.9
/// refuses anything over 100 MiB by default, which is the right instinct for
/// an untrusted stream — the window is an allocation the sender chooses — but
/// too tight for what actually arrives here: the gVisor release archive is
/// written with a 128 MiB window, and `zstd --long` produces the same. So the
/// limit is raised, once, to a number that fits those and still bounds a
/// hostile layer at a quarter of a gigabyte rather than at "whatever the
/// header says".
pub const MAX_ZSTD_WINDOW: u64 = 256 << 20;

/// A streaming zstd decoder over `source` that accepts windows up to
/// [`MAX_ZSTD_WINDOW`]. The one place the limit is chosen, so the image
/// store and the gVisor installer cannot disagree about it.
pub fn zstd_decoder<R: std::io::Read>(
    source: R,
) -> std::result::Result<
    ruzstd::decoding::StreamingDecoder<R, ruzstd::decoding::FrameDecoder>,
    ruzstd::decoding::errors::FrameDecoderError,
> {
    ruzstd::decoding::StreamingDecoder::new_with_max_window_size(source, MAX_ZSTD_WINDOW)
}

/// Whether a layer's media type is one Zygo can unpack, and how it is
/// compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerCompression {
    None,
    Gzip,
    Zstd,
}

impl LayerCompression {
    pub fn from_media_type(media_type: &str) -> Option<Self> {
        // Strip any `+encrypted` style suffix a registry might add.
        let base = media_type.split(';').next().unwrap_or(media_type).trim();
        match base {
            "application/vnd.oci.image.layer.v1.tar"
            | "application/vnd.docker.image.rootfs.diff.tar" => Some(Self::None),
            "application/vnd.oci.image.layer.v1.tar+gzip"
            | "application/vnd.docker.image.rootfs.diff.tar.gzip" => Some(Self::Gzip),
            "application/vnd.oci.image.layer.v1.tar+zstd" => Some(Self::Zstd),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(arch: &str, os: &str, variant: Option<&str>) -> Descriptor {
        Descriptor {
            media_type: MEDIA_OCI_MANIFEST.into(),
            digest: format!("sha256:{}", "0".repeat(64)),
            size: 1,
            platform: Some(Platform {
                architecture: arch.into(),
                os: os.into(),
                variant: variant.map(str::to_string),
            }),
        }
    }

    #[test]
    fn host_platform_is_always_linux() {
        let p = Platform::host();
        assert_eq!(p.os, "linux");
        assert!(
            ["amd64", "arm64"].contains(&p.architecture.as_str()) || !p.architecture.is_empty()
        );
    }

    #[test]
    fn index_selects_the_matching_architecture() {
        let idx = Index {
            schema_version: 2,
            media_type: MEDIA_OCI_INDEX.into(),
            manifests: vec![
                desc("amd64", "linux", None),
                desc("arm64", "linux", Some("v8")),
            ],
        };
        let want = Platform {
            architecture: "arm64".into(),
            os: "linux".into(),
            variant: None,
        };
        assert_eq!(
            idx.select(&want)
                .unwrap()
                .platform
                .as_ref()
                .unwrap()
                .architecture,
            "arm64"
        );
    }

    #[test]
    fn attestation_entries_are_never_selected() {
        let idx = Index {
            schema_version: 2,
            media_type: MEDIA_OCI_INDEX.into(),
            manifests: vec![desc("unknown", "unknown", None)],
        };
        let want = Platform {
            architecture: "unknown".into(),
            os: "unknown".into(),
            variant: None,
        };
        assert!(idx.select(&want).is_none());
        assert!(idx.available().is_empty());
    }

    #[test]
    fn a_missing_variant_does_not_block_a_match() {
        let idx = Index {
            schema_version: 2,
            media_type: MEDIA_OCI_INDEX.into(),
            manifests: vec![desc("arm64", "linux", Some("v8"))],
        };
        let want = Platform {
            architecture: "arm64".into(),
            os: "linux".into(),
            variant: None,
        };
        assert!(idx.select(&want).is_some());
    }

    #[test]
    fn no_match_lists_what_was_on_offer() {
        let idx = Index {
            schema_version: 2,
            media_type: MEDIA_OCI_INDEX.into(),
            manifests: vec![
                desc("amd64", "linux", None),
                desc("arm64", "linux", Some("v8")),
            ],
        };
        assert_eq!(idx.available(), ["linux/amd64", "linux/arm64/v8"]);
    }

    #[test]
    fn a_zstd_frame_with_a_128_mib_window_is_accepted() {
        use std::io::Read;
        // A complete zstd frame: magic, a frame header whose window descriptor
        // (exponent 17, mantissa 0) declares 2^27 = 128 MiB, and one empty raw
        // block marked last. That is the window the gVisor release archive
        // uses; ruzstd's default limit (100 MiB) rejects it at `new`.
        let frame: [u8; 9] = [0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x88, 0x01, 0x00, 0x00];
        assert!(
            ruzstd::decoding::StreamingDecoder::new(&frame[..]).is_err(),
            "the default limit should reject this frame, or the helper is redundant"
        );
        let mut out = Vec::new();
        zstd_decoder(&frame[..])
            .expect("a 128 MiB window is within MAX_ZSTD_WINDOW")
            .read_to_end(&mut out)
            .expect("an empty raw block decodes to nothing");
        assert!(out.is_empty());
    }

    #[test]
    fn layer_compression_is_read_from_the_media_type() {
        use LayerCompression::*;
        assert_eq!(
            LayerCompression::from_media_type("application/vnd.oci.image.layer.v1.tar+gzip"),
            Some(Gzip)
        );
        assert_eq!(
            LayerCompression::from_media_type("application/vnd.docker.image.rootfs.diff.tar.gzip"),
            Some(Gzip)
        );
        assert_eq!(
            LayerCompression::from_media_type("application/vnd.oci.image.layer.v1.tar+zstd"),
            Some(Zstd)
        );
        assert_eq!(
            LayerCompression::from_media_type("application/vnd.oci.image.layer.v1.tar"),
            Some(None)
        );
        assert_eq!(
            LayerCompression::from_media_type("application/octet-stream"),
            Option::None
        );
    }

    #[test]
    fn image_config_composes_entrypoint_and_cmd() {
        let cfg = ImageConfig {
            config: ConfigBody {
                entrypoint: Some(vec!["/usr/bin/tini".into(), "--".into()]),
                cmd: Some(vec!["python3".into()]),
                env: vec!["PATH=/usr/bin".into(), "malformed".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(cfg.default_argv(), ["/usr/bin/tini", "--", "python3"]);
        assert_eq!(
            cfg.env_pairs(),
            [("PATH".to_string(), "/usr/bin".to_string())]
        );
    }

    #[test]
    fn manifests_parse_with_unknown_fields_present() {
        let json = r#"{
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "artifactType": "something/new",
            "config": {"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:aa","size":7},
            "layers": [{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"sha256:bb","size":9}],
            "annotations": {"org.opencontainers.image.created": "2026-01-01"}
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.layers.len(), 1);
        assert_eq!(m.config.digest, "sha256:aa");
    }
}
