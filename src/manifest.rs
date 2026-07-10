use std::collections::BTreeMap;

use anyhow::Context;
use reqwest::blocking::Client;
use serde::Deserialize;

use crate::platform::{
    LINUX_PLATFORM, WINDOWS_PLATFORM, game_archive_asset_name, launcher_asset_name,
};

pub const MANIFEST_URL: &str = "https://craftmoon-manifest.pages.dev/manifest.json";
pub type PlatformContentHashes = BTreeMap<String, String>;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub name: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub game_version: String,
    pub game_archives: BTreeMap<String, Asset>,
    pub game_content_hashes: BTreeMap<String, PlatformContentHashes>,
    pub launcher_version: String,
    pub launcher_binaries: BTreeMap<String, Asset>,
    pub patches: BTreeMap<String, String>,
    pub endpoints: Vec<String>,
}

impl Manifest {
    pub fn game_archive(&self, platform: &str) -> anyhow::Result<&Asset> {
        let asset = self
            .game_archives
            .get(platform)
            .ok_or_else(|| anyhow::anyhow!("manifest does not list a {platform} game archive"))?;
        let expected_name = game_archive_asset_name(platform, &self.game_version)?;
        anyhow::ensure!(
            asset.name == expected_name,
            "manifest has an invalid {platform} archive name {}",
            asset.name
        );
        anyhow::ensure!(
            is_sha256(&asset.sha256),
            "manifest has an invalid {platform} archive hash"
        );
        Ok(asset)
    }

    pub fn launcher_binary(&self, platform: &str) -> anyhow::Result<&Asset> {
        let asset = self.launcher_binaries.get(platform).ok_or_else(|| {
            anyhow::anyhow!("manifest does not list a {platform} launcher binary")
        })?;
        let expected_name = launcher_asset_name(platform, &self.launcher_version)?;
        anyhow::ensure!(
            asset.name == expected_name,
            "manifest has an invalid {platform} launcher binary name {}",
            asset.name
        );
        anyhow::ensure!(
            is_sha256(&asset.sha256),
            "manifest has an invalid {platform} launcher hash"
        );
        Ok(asset)
    }

    pub fn content_hash(&self, version: &str, platform: &str) -> anyhow::Result<&str> {
        let hash = self
            .game_content_hashes
            .get(version)
            .and_then(|platform_hashes| platform_hashes.get(platform))
            .map(String::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "manifest does not list a content hash for version {version}, platform {platform}"
                )
            })?;
        anyhow::ensure!(
            is_sha256(hash),
            "manifest has an invalid content hash for version {version}, platform {platform}"
        );
        Ok(hash)
    }

    fn validate(&self) -> anyhow::Result<()> {
        validate_game_tag(&self.game_version, "game_version")?;
        semver::Version::parse(&self.launcher_version).with_context(|| {
            format!(
                "manifest has a non-semver launcher_version {:?}",
                self.launcher_version
            )
        })?;
        anyhow::ensure!(
            !self.endpoints.is_empty(),
            "manifest has no download endpoints"
        );
        for endpoint in &self.endpoints {
            let url = reqwest::Url::parse(endpoint)
                .with_context(|| format!("manifest has an invalid endpoint {endpoint:?}"))?;
            anyhow::ensure!(
                url.scheme() == "https" && url.path() == "/" && url.query().is_none(),
                "manifest endpoint must be an HTTPS base URL without a path or query: {endpoint}"
            );
        }

        for platform in [WINDOWS_PLATFORM, LINUX_PLATFORM] {
            self.game_archive(platform)?;
            self.launcher_binary(platform)?;
        }

        for version in self.game_content_hashes.keys() {
            validate_game_tag(version, "game_content_hashes version")?;
            for platform in [WINDOWS_PLATFORM, LINUX_PLATFORM] {
                self.content_hash(version, platform)?;
            }
        }

        for (name, hash) in &self.patches {
            let (from, to) = patch_tags(name)?;
            validate_game_tag(from, "patch source version")?;
            validate_game_tag(to, "patch target version")?;
            anyhow::ensure!(
                self.game_content_hashes.contains_key(from)
                    && self.game_content_hashes.contains_key(to),
                "manifest patch {name} refers to a version outside game_content_hashes"
            );
            anyhow::ensure!(
                is_sha256(hash),
                "manifest has an invalid patch hash for {name}"
            );
        }

        self.content_hash(&self.game_version, crate::platform::CURRENT_PLATFORM)?;
        Ok(())
    }
}

pub fn fetch_manifest(client: &Client) -> anyhow::Result<Manifest> {
    let response = client
        .get(MANIFEST_URL)
        .send()
        .context("failed to fetch update manifest from CF Pages")?;

    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("manifest fetch failed with HTTP {status}");
    }

    let manifest: Manifest = response
        .json()
        .context("failed to parse update manifest JSON")?;
    manifest.validate()?;
    Ok(manifest)
}

fn validate_game_tag(value: &str, field_name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.as_bytes()[0].is_ascii_digit()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
        "manifest has an invalid {field_name} {value:?}"
    );
    Ok(())
}

fn patch_tags(name: &str) -> anyhow::Result<(&str, &str)> {
    let stem = name
        .strip_suffix("-linux.patch")
        .or_else(|| name.strip_suffix(".patch"))
        .ok_or_else(|| anyhow::anyhow!("manifest has an invalid patch name {name}"))?;
    stem.split_once("-to-")
        .ok_or_else(|| anyhow::anyhow!("manifest has an invalid patch name {name}"))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
