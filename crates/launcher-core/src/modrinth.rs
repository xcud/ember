//! A thin async client for the parts of the Modrinth API we need right now:
//! batch content-hash lookup and batch project metadata.
//!
//! Docs: <https://docs.modrinth.com/api/>

use std::collections::HashMap;

use serde::Deserialize;

const API: &str = "https://api.modrinth.com/v2";

/// Modrinth asks API consumers to send a descriptive User-Agent identifying the
/// app and a contact. See <https://docs.modrinth.com/api/#user-agents>.
const USER_AGENT: &str =
    "positronic-ai/ember/0.1.0 (ben@positronic.ai)";

pub struct Client {
    http: reqwest::Client,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Version {
    pub id: String,
    pub project_id: String,
    pub version_number: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub loaders: Vec<String>,
    #[serde(default)]
    pub game_versions: Vec<String>,
    pub files: Vec<VersionFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VersionFile {
    pub hashes: FileHashes,
    pub url: String,
    pub filename: String,
    pub size: u64,
    #[serde(default)]
    pub primary: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileHashes {
    #[serde(default)]
    pub sha1: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Project {
    pub id: String,
    pub slug: String,
    #[serde(default)]
    pub title: String,
}

impl Client {
    pub fn new() -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()?;
        Ok(Self { http })
    }

    /// Look up many files at once by SHA-1. Returns a map of `sha1 -> Version`
    /// for the hashes Modrinth recognizes; unknown hashes are simply absent.
    pub async fn versions_by_sha1(
        &self,
        sha1s: &[String],
    ) -> anyhow::Result<HashMap<String, Version>> {
        if sha1s.is_empty() {
            return Ok(HashMap::new());
        }
        let body = serde_json::json!({ "hashes": sha1s, "algorithm": "sha1" });
        let resp = self
            .http
            .post(format!("{API}/version_files"))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }

    /// Fetch project metadata (we mainly want the human-readable `slug`).
    pub async fn projects(&self, ids: &[String]) -> anyhow::Result<Vec<Project>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // The `ids` query param is a JSON-encoded array of strings.
        let ids_json = serde_json::to_string(ids)?;
        let resp = self
            .http
            .get(format!("{API}/projects"))
            .query(&[("ids", ids_json)])
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }
}
