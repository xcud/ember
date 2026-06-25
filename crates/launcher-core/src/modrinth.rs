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

#[derive(Clone)]
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
    /// ISO-8601 UTC; lexical order is chronological order.
    #[serde(default)]
    pub date_published: String,
    /// "release" | "beta" | "alpha".
    #[serde(default)]
    pub version_type: String,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    pub files: Vec<VersionFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Dependency {
    #[serde(default)]
    pub project_id: Option<String>,
    /// "required" | "optional" | "incompatible" | "embedded".
    #[serde(default)]
    pub dependency_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchHit {
    pub project_id: String,
    pub slug: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub downloads: u64,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub categories: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    hits: Vec<SearchHit>,
}

impl Version {
    /// The file to install: the one flagged primary, else the first.
    pub fn primary_file(&self) -> Option<&VersionFile> {
        self.files.iter().find(|f| f.primary).or_else(|| self.files.first())
    }
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
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub downloads: u64,
}

impl Client {
    pub fn new() -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()?;
        Ok(Self { http })
    }

    /// The underlying HTTP client, for modules that need to stream downloads.
    pub fn http(&self) -> &reqwest::Client {
        &self.http
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

    /// Search for mods compatible with a loader + game version.
    pub async fn search(
        &self,
        query: &str,
        loader: &str,
        game_version: &str,
    ) -> anyhow::Result<Vec<SearchHit>> {
        let facets = serde_json::to_string(&[
            vec!["project_type:mod".to_string()],
            vec![format!("categories:{loader}")],
            vec![format!("versions:{game_version}")],
        ])?;
        let resp = self
            .http
            .get(format!("{API}/search"))
            .query(&[("query", query), ("facets", &facets), ("limit", "20")])
            .send()
            .await?
            .error_for_status()?;
        let parsed: SearchResponse = resp.json().await?;
        Ok(parsed.hits)
    }

    /// List a project's versions, filtered to one loader and game version.
    /// Returned newest-first (by publish date).
    pub async fn project_versions(
        &self,
        slug: &str,
        loader: &str,
        game_version: &str,
    ) -> anyhow::Result<Vec<Version>> {
        let loaders = serde_json::to_string(&[loader])?;
        let games = serde_json::to_string(&[game_version])?;
        let resp = self
            .http
            .get(format!("{API}/project/{slug}/version"))
            .query(&[("loaders", loaders), ("game_versions", games)])
            .send()
            .await?
            .error_for_status()?;
        let mut versions: Vec<Version> = resp.json().await?;
        // Newest first; lexical sort of ISO timestamps is chronological.
        versions.sort_by(|a, b| b.date_published.cmp(&a.date_published));
        Ok(versions)
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
