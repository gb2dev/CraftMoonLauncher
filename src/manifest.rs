use std::collections::BTreeMap;

use anyhow::Context;
use reqwest::blocking::Client;
use serde::Deserialize;

pub const MANIFEST_URL: &str = "https://craftmoon-manifest.pages.dev/manifest.json";

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub game_version: String,
    pub game_archives: BTreeMap<String, String>,
    pub launcher_version: String,
    pub launcher_binaries: BTreeMap<String, String>,
    #[serde(default)]
    pub patches: BTreeMap<String, String>,
    pub endpoints: Vec<String>,
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

    response
        .json()
        .context("failed to parse update manifest JSON")
}
