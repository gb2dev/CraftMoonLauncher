use anyhow::Context;
use reqwest::blocking::Client;

pub const USER_AGENT_VALUE: &str = "CraftMoon-Launcher/1.0";

pub fn http_client() -> anyhow::Result<Client> {
    Client::builder()
        .user_agent(USER_AGENT_VALUE)
        .https_only(true)
        .build()
        .context("failed to create HTTP client")
}
