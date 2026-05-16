use anyhow::Context;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

pub const USER_AGENT_VALUE: &str = "CraftMoon-Launcher/1.0";
pub const CRAFTMOON_REPO: &str = "gb2dev/CraftMoon";
pub const LAUNCHER_REPO: &str = "gb2dev/CraftMoonLauncher";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubRelease {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<GitHubAsset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubAsset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

pub fn github_client() -> anyhow::Result<Client> {
    Client::builder()
        .user_agent(USER_AGENT_VALUE)
        .https_only(true)
        .build()
        .context("failed to create GitHub HTTP client")
}

pub fn fetch_latest_release(client: &Client) -> anyhow::Result<GitHubRelease> {
    fetch_latest_release_for_repo(client, CRAFTMOON_REPO)
}

pub fn fetch_latest_release_for_repo(client: &Client, repo: &str) -> anyhow::Result<GitHubRelease> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    get_release_json(client, &url)
}

pub fn fetch_releases(client: &Client) -> anyhow::Result<Vec<GitHubRelease>> {
    fetch_releases_for_repo(client, CRAFTMOON_REPO)
}

pub fn fetch_releases_for_repo(client: &Client, repo: &str) -> anyhow::Result<Vec<GitHubRelease>> {
    let mut releases = Vec::new();
    let mut page = 1;

    loop {
        let url = format!("https://api.github.com/repos/{repo}/releases?per_page=100&page={page}");
        let response = client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .with_context(|| format!("failed to fetch {url}"))?;

        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("GitHub API request failed with status {status} for {url}");
        }

        let mut page_releases: Vec<GitHubRelease> = response
            .json()
            .with_context(|| format!("failed to parse GitHub release list from {url}"))?;
        let page_count = page_releases.len();
        releases.append(&mut page_releases);

        if page_count < 100 {
            break;
        }
        page += 1;
    }

    Ok(releases)
}

pub fn fetch_release_by_tag(client: &Client, tag: &str) -> anyhow::Result<GitHubRelease> {
    let url = format!("https://api.github.com/repos/{CRAFTMOON_REPO}/releases/tags/{tag}");
    get_release_json(client, &url)
}

fn get_release_json(client: &Client, url: &str) -> anyhow::Result<GitHubRelease> {
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .with_context(|| format!("failed to fetch {url}"))?;

    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("GitHub API request failed with status {status} for {url}");
    }

    response
        .json()
        .with_context(|| format!("failed to parse GitHub release JSON from {url}"))
}
