//! Microsoft / Xbox Live / Minecraft authentication.
//!
//! Implements the 5-hop relay: MS OAuth device-code -> Xbox Live -> XSTS ->
//! Minecraft services -> profile. The MS *refresh token* is persisted so the
//! user logs in once; subsequent launches silently re-mint the chain.
//!
//! Storage note: the account file holds a refresh token in plaintext with
//! 0600 perms. TODO: move to an OS keyring / encrypt at rest.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::launch::AuthSession;

/// The Azure AD application (client) ID. Overridable via `EMBER_CLIENT_ID`.
pub const DEFAULT_CLIENT_ID: &str = "273b5f29-d143-45cb-954e-56d82a13e52b";

const SCOPE: &str = "XboxLive.signin offline_access";
const DEVICE_CODE_URL: &str =
    "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const XBL_URL: &str = "https://user.auth.xboxlive.com/user/authenticate";
const XSTS_URL: &str = "https://xsts.auth.xboxlive.com/xsts/authorize";
const MC_LOGIN_URL: &str =
    "https://api.minecraftservices.com/authentication/login_with_xbox";
const MC_PROFILE_URL: &str = "https://api.minecraftservices.com/minecraft/profile";

pub fn client_id() -> String {
    std::env::var("EMBER_CLIENT_ID").unwrap_or_else(|_| DEFAULT_CLIENT_ID.to_string())
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// Persisted account. The refresh token is the long-lived credential; the
/// Minecraft token/xuid are cached and re-minted when expired.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub uuid: String,
    pub name: String,
    pub ms_refresh_token: String,
    pub mc_access_token: String,
    pub xuid: String,
    /// Unix seconds at which the Minecraft token expires.
    pub mc_expires_at: u64,
}

pub fn account_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")
        });
    base.join("ember").join("account.json")
}

impl Account {
    pub fn load() -> Option<Account> {
        let text = std::fs::read_to_string(account_path()).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = account_path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        // Best-effort tighten perms (refresh token lives here).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub fn to_session(&self) -> AuthSession {
        AuthSession {
            player_name: self.name.clone(),
            uuid: self.uuid.clone(),
            access_token: self.mc_access_token.clone(),
            xuid: self.xuid.clone(),
            client_id: client_id(),
            user_type: "msa".to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    #[serde(default = "default_interval")]
    pub interval: u64,
    pub message: Option<String>,
}

fn default_interval() -> u64 {
    5
}

struct MsTokens {
    access_token: String,
    refresh_token: String,
}

/// Step 1a: request a device code to show the user.
pub async fn start_device_code(http: &reqwest::Client) -> anyhow::Result<DeviceCode> {
    let resp = http
        .post(DEVICE_CODE_URL)
        .form(&[("client_id", client_id().as_str()), ("scope", SCOPE)])
        .send()
        .await?
        .error_for_status()?;
    Ok(resp.json().await?)
}

/// Step 1b: poll until the user approves (or the code expires).
async fn poll_for_tokens(http: &reqwest::Client, dc: &DeviceCode) -> anyhow::Result<MsTokens> {
    let cid = client_id();
    let mut interval = dc.interval.max(1);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        let resp = http
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", cid.as_str()),
                ("device_code", dc.device_code.as_str()),
            ])
            .send()
            .await?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if status.is_success() {
            return Ok(MsTokens {
                access_token: body["access_token"].as_str().unwrap_or_default().to_string(),
                refresh_token: body["refresh_token"].as_str().unwrap_or_default().to_string(),
            });
        }
        match body["error"].as_str() {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval += 5;
                continue;
            }
            Some("authorization_declined") => anyhow::bail!("you declined the sign-in request"),
            Some("expired_token") => anyhow::bail!("the sign-in code expired; run `ember login` again"),
            other => anyhow::bail!("sign-in failed: {}", other.unwrap_or("unknown error")),
        }
    }
}

/// Refresh the MS access token from a stored refresh token.
async fn refresh_ms(http: &reqwest::Client, refresh_token: &str) -> anyhow::Result<MsTokens> {
    let cid = client_id();
    let resp = http
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", cid.as_str()),
            ("refresh_token", refresh_token),
            ("scope", SCOPE),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("MS token refresh failed; re-run `ember login`");
    }
    let body: Value = resp.json().await?;
    Ok(MsTokens {
        access_token: body["access_token"].as_str().unwrap_or_default().to_string(),
        // MS rotates refresh tokens; keep the old one if none returned.
        refresh_token: body["refresh_token"]
            .as_str()
            .unwrap_or(refresh_token)
            .to_string(),
    })
}

/// Step 2: Xbox Live. Returns (token, user hash).
async fn xbl_authenticate(http: &reqwest::Client, ms_access: &str) -> anyhow::Result<(String, String)> {
    let body = serde_json::json!({
        "Properties": {
            "AuthMethod": "RPS",
            "SiteName": "user.auth.xboxlive.com",
            "RpsTicket": format!("d={ms_access}"),
        },
        "RelyingParty": "http://auth.xboxlive.com",
        "TokenType": "JWT"
    });
    let v: Value = http
        .post(XBL_URL)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let token = v["Token"].as_str().unwrap_or_default().to_string();
    let uhs = v["DisplayClaims"]["xui"][0]["uhs"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    Ok((token, uhs))
}

/// Step 3: XSTS. Returns (token, user hash, xuid). Decodes the common errors.
async fn xsts_authorize(http: &reqwest::Client, xbl_token: &str) -> anyhow::Result<(String, String, String)> {
    let body = serde_json::json!({
        "Properties": { "SandboxId": "RETAIL", "UserTokens": [xbl_token] },
        "RelyingParty": "rp://api.minecraftservices.com/",
        "TokenType": "JWT"
    });
    let resp = http.post(XSTS_URL).json(&body).send().await?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        let xerr = v["XErr"].as_i64().unwrap_or(0);
        let msg = match xerr {
            2148916233 => "this Microsoft account has no Xbox profile — sign in once at minecraft.net to create one",
            2148916235 => "Xbox Live is not available in this account's region",
            2148916236 | 2148916237 => "this account needs adult verification",
            2148916238 => "this is a child account — it must be added to a Microsoft Family",
            _ => "XSTS authorization failed",
        };
        anyhow::bail!("{msg} (XErr {xerr})");
    }
    let v: Value = resp.error_for_status()?.json().await?;
    let token = v["Token"].as_str().unwrap_or_default().to_string();
    let uhs = v["DisplayClaims"]["xui"][0]["uhs"].as_str().unwrap_or_default().to_string();
    let xuid = v["DisplayClaims"]["xui"][0]["xid"].as_str().unwrap_or_default().to_string();
    Ok((token, uhs, xuid))
}

/// Step 4: exchange for a Minecraft access token. Returns (token, expires_in).
async fn mc_login(http: &reqwest::Client, uhs: &str, xsts_token: &str) -> anyhow::Result<(String, u64)> {
    let identity = format!("XBL3.0 x={uhs};{xsts_token}");
    let v: Value = http
        .post(MC_LOGIN_URL)
        .json(&serde_json::json!({ "identityToken": identity }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let token = v["access_token"].as_str().unwrap_or_default().to_string();
    let expires_in = v["expires_in"].as_u64().unwrap_or(86_400);
    Ok((token, expires_in))
}

fn dash_uuid(raw: &str) -> String {
    if raw.len() == 32 && !raw.contains('-') {
        format!(
            "{}-{}-{}-{}-{}",
            &raw[0..8], &raw[8..12], &raw[12..16], &raw[16..20], &raw[20..32]
        )
    } else {
        raw.to_string()
    }
}

/// Step 5: fetch the player profile (UUID + name). Requires game ownership.
async fn mc_profile(http: &reqwest::Client, mc_token: &str) -> anyhow::Result<(String, String)> {
    let resp = http.get(MC_PROFILE_URL).bearer_auth(mc_token).send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("this account doesn't own Minecraft Java Edition");
    }
    let v: Value = resp.error_for_status()?.json().await?;
    let uuid = dash_uuid(v["id"].as_str().unwrap_or_default());
    let name = v["name"].as_str().unwrap_or_default().to_string();
    Ok((uuid, name))
}

/// Run the Xbox->XSTS->MC->profile portion given a fresh MS access token,
/// producing a fully populated, persisted Account.
async fn finish_chain(
    http: &reqwest::Client,
    ms: MsTokens,
) -> anyhow::Result<Account> {
    let (xbl_token, _uhs) = xbl_authenticate(http, &ms.access_token).await?;
    let (xsts_token, uhs, xuid) = xsts_authorize(http, &xbl_token).await?;
    let (mc_token, expires_in) = mc_login(http, &uhs, &xsts_token).await?;
    let (uuid, name) = mc_profile(http, &mc_token).await?;
    Ok(Account {
        uuid,
        name,
        ms_refresh_token: ms.refresh_token,
        mc_access_token: mc_token,
        xuid,
        mc_expires_at: now_secs() + expires_in.saturating_sub(60),
    })
}

/// Full interactive login. `on_code` is called with the device code so the UI
/// can tell the user where to go. Returns the persisted Account.
pub async fn login_interactive<F>(http: &reqwest::Client, on_code: F) -> anyhow::Result<Account>
where
    F: FnOnce(&DeviceCode),
{
    let dc = start_device_code(http).await?;
    on_code(&dc);
    let ms = poll_for_tokens(http, &dc).await?;
    let account = finish_chain(http, ms).await?;
    account.save()?;
    Ok(account)
}

/// Return a launch-ready session for the stored account, refreshing the
/// Minecraft token if it's expired. Persists any rotated tokens.
pub async fn ensure_session(http: &reqwest::Client, mut account: Account) -> anyhow::Result<AuthSession> {
    if now_secs() < account.mc_expires_at && !account.mc_access_token.is_empty() {
        return Ok(account.to_session());
    }
    let ms = refresh_ms(http, &account.ms_refresh_token).await?;
    let refreshed = finish_chain(http, ms).await?;
    account = refreshed;
    account.save()?;
    Ok(account.to_session())
}
