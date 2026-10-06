use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::files::{home_dir, rfc3339};

pub const OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Codex,
    Opencode,
}

#[derive(Debug, Clone)]
pub struct Credential {
    pub kind: Kind,
    pub path: PathBuf,
    pub access: String,
    pub account_id: Option<String>,
    pub refresh: Option<String>,
    /// Unix seconds; decoded from the JWT when possible.
    pub expires_at: Option<i64>,
}

impl Credential {
    pub fn is_expired(&self) -> bool {
        match self.expires_at {
            Some(at) => at <= crate::files::now_unix() + 30,
            None => false,
        }
    }

    pub fn valid(&self) -> bool {
        !self.is_expired()
    }

    pub fn masked_account(&self) -> String {
        match &self.account_id {
            Some(id) if id.len() > 6 => format!("...{}", &id[id.len() - 6..]),
            Some(id) => id.clone(),
            None => "unknown".to_string(),
        }
    }
}

pub fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Codex => "codex",
        Kind::Opencode => "opencode",
    }
}

fn decode_b64url(input: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(input)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(input))
        .ok()
}

pub fn jwt_exp(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = decode_b64url(payload)?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.get("exp").and_then(|v| v.as_i64())
}

pub fn parse_codex(raw: &str, path: &Path) -> Option<Credential> {
    let value: Value = serde_json::from_str(raw).ok()?;
    if value.get("auth_mode")?.as_str()? != "chatgpt" {
        return None;
    }
    let tokens = value.get("tokens")?;
    let access = tokens.get("access_token")?.as_str()?.to_string();
    let refresh = tokens
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let account_id = tokens
        .get("account_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let expires_at = jwt_exp(&access);
    Some(Credential {
        kind: Kind::Codex,
        path: path.to_path_buf(),
        access,
        account_id,
        refresh,
        expires_at,
    })
}

pub fn parse_opencode(raw: &str, path: &Path) -> Option<Credential> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let entry = value.get("openai")?;
    if entry.get("type")?.as_str()? != "oauth" {
        return None;
    }
    let access = entry.get("access")?.as_str()?.to_string();
    let refresh = entry
        .get("refresh")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let account_id = entry
        .get("accountId")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let expires_at = entry
        .get("expires")
        .and_then(|v| v.as_i64())
        .map(|ms| ms / 1000)
        .or_else(|| jwt_exp(&access));
    Some(Credential {
        kind: Kind::Opencode,
        path: path.to_path_buf(),
        access,
        account_id,
        refresh,
        expires_at,
    })
}

/// Candidate credential files, most preferred first.
pub fn candidates() -> Vec<(Kind, PathBuf)> {
    let mut out = Vec::new();
    let codex_base = std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".codex")));
    if let Some(base) = codex_base {
        out.push((Kind::Codex, base.join("auth.json")));
    }
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".local").join("share")));
    if let Some(base) = data_home {
        out.push((Kind::Opencode, base.join("opencode").join("auth.json")));
    }
    out
}

pub fn load_from(path: &Path) -> Result<Credential> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_codex(&raw, path)
        .or_else(|| parse_opencode(&raw, path))
        .ok_or_else(|| anyhow!("{} has no usable ChatGPT OAuth credentials", path.display()))
}

/// Pick the best available credential: an unexpired token first, otherwise the
/// first store that can refresh itself.
pub fn load(explicit: Option<&Path>) -> Result<Credential> {
    if let Some(path) = explicit {
        return load_from(path);
    }
    if let Some(path) = std::env::var_os("FUCKINGGEN_AUTH_FILE").filter(|value| !value.is_empty()) {
        return load_from(Path::new(&path));
    }
    let mut found = Vec::new();
    for (kind, path) in candidates() {
        if let Ok(raw) = std::fs::read_to_string(&path) {
            let parsed = match kind {
                Kind::Codex => parse_codex(&raw, &path),
                Kind::Opencode => parse_opencode(&raw, &path),
            };
            if let Some(cred) = parsed {
                found.push(cred);
            }
        }
    }
    if found.is_empty() {
        let looked = candidates()
            .iter()
            .map(|(_, p)| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "no ChatGPT subscription credentials found. log in first with `codex login`.\nlooked in: {looked}"
        );
    }
    if let Some(cred) = found.iter().find(|c| c.valid()) {
        return Ok(cred.clone());
    }
    if let Some(cred) = found.iter().find(|c| c.refresh.is_some()) {
        return Ok(cred.clone());
    }
    Ok(found.remove(0))
}

/// Exchange the refresh token and persist the rotated credentials back into the
/// file the login tool owns.
pub fn refresh(agent: &ureq::Agent, cred: &Credential) -> Result<Credential> {
    let refresh_token = cred.refresh.clone().ok_or_else(|| {
        anyhow!(
            "{} has no refresh token; log in again with `codex login`",
            cred.path.display()
        )
    })?;
    let form = format!(
        "grant_type=refresh_token&client_id={}&refresh_token={}&scope={}",
        urlencode(CODEX_CLIENT_ID),
        urlencode(&refresh_token),
        urlencode("openid profile email")
    );
    let response = agent
        .post(OAUTH_TOKEN_URL)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&form)
        .map_err(|err| anyhow!(crate::http::describe_error(err)))?;
    let text = response.into_string().unwrap_or_default();
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing token refresh response: {}", truncate(&text, 200)))?;
    let access = value
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            anyhow!(
                "refresh response had no access_token: {}",
                truncate(&text, 200)
            )
        })?
        .to_string();
    let mut updated = cred.clone();
    updated.expires_at = jwt_exp(&access).or_else(|| {
        value
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .map(|secs| crate::files::now_unix() + secs)
    });
    updated.access = access;
    if let Some(new_refresh) = value.get("refresh_token").and_then(|v| v.as_str()) {
        updated.refresh = Some(new_refresh.to_string());
    }
    write_back(&updated, &value).with_context(|| format!("updating {}", cred.path.display()))?;
    Ok(updated)
}

fn write_back(cred: &Credential, response: &Value) -> Result<()> {
    let raw = std::fs::read_to_string(&cred.path)?;
    let mut root: Value = serde_json::from_str(&raw)?;
    match cred.kind {
        Kind::Codex => {
            let tokens = root
                .get_mut("tokens")
                .ok_or_else(|| anyhow!("auth file lost its tokens object"))?;
            tokens["access_token"] = Value::String(cred.access.clone());
            if let Some(refresh) = &cred.refresh {
                tokens["refresh_token"] = Value::String(refresh.clone());
            }
            if let Some(id_token) = response.get("id_token").and_then(|v| v.as_str()) {
                tokens["id_token"] = Value::String(id_token.to_string());
            }
            root["last_refresh"] = Value::String(rfc3339(crate::files::now_unix()));
        }
        Kind::Opencode => {
            let entry = root
                .get_mut("openai")
                .ok_or_else(|| anyhow!("auth file lost its openai entry"))?;
            entry["access"] = Value::String(cred.access.clone());
            if let Some(refresh) = &cred.refresh {
                entry["refresh"] = Value::String(refresh.clone());
            }
            if let Some(expires_at) = cred.expires_at {
                entry["expires"] = Value::Number((expires_at * 1000).into());
            }
        }
    }
    write_private_json(&cred.path, &root)
}

pub fn write_private_json(path: &Path, value: &Value) -> Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "auth.json".to_string());
    let tmp = path.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("moving into place: {}", path.display()))?;
    Ok(())
}

pub fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub fn truncate(input: &str, max: usize) -> String {
    let mut out: String = input.chars().take(max).collect();
    if input.chars().count() > max {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fake_jwt(exp: i64) -> String {
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{{\"exp\":{exp}}}"));
        format!("eyJhbGciOiJub25lIn0.{payload}.sig")
    }

    #[test]
    fn reads_jwt_expiry() {
        assert_eq!(jwt_exp(&fake_jwt(1_893_456_000)), Some(1_893_456_000));
        assert_eq!(jwt_exp("garbage"), None);
    }

    #[test]
    fn parses_codex_credentials() {
        let raw = json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "access_token": fake_jwt(1_893_456_000),
                "refresh_token": "r1",
                "id_token": "i1",
                "account_id": "acct-123456789"
            },
            "last_refresh": "2026-09-29T19:04:45Z"
        })
        .to_string();
        let cred = parse_codex(&raw, Path::new("/tmp/auth.json")).unwrap();
        assert_eq!(cred.kind, Kind::Codex);
        assert_eq!(cred.account_id.as_deref(), Some("acct-123456789"));
        assert_eq!(cred.expires_at, Some(1_893_456_000));
        assert_eq!(cred.masked_account(), "...456789");
    }

    #[test]
    fn parses_opencode_credentials() {
        let raw = json!({
            "openai": {
                "type": "oauth",
                "access": fake_jwt(1_800_000_000),
                "refresh": "r",
                "expires": 1_800_000_000_000i64,
                "accountId": "acct-abcdef"
            }
        })
        .to_string();
        let cred = parse_opencode(&raw, Path::new("/tmp/auth.json")).unwrap();
        assert_eq!(cred.kind, Kind::Opencode);
        assert_eq!(cred.expires_at, Some(1_800_000_000));
    }

    #[test]
    fn rejects_wrong_auth_mode() {
        let raw =
            json!({"auth_mode": "api_key", "tokens": {"access_token": fake_jwt(1)}}).to_string();
        assert!(parse_codex(&raw, Path::new("/tmp/x")).is_none());
    }

    #[test]
    fn urlencode_escapes() {
        assert_eq!(
            urlencode("openid profile email"),
            "openid%20profile%20email"
        );
        assert_eq!(urlencode("a-b._~"), "a-b._~");
        assert_eq!(urlencode("a/b"), "a%2Fb");
    }

    #[test]
    fn write_back_preserves_unrelated_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("auth.json");
        std::fs::write(
            &path,
            json!({
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {"access_token": "old", "refresh_token": "old-r", "id_token": "old-i", "account_id": "acct"},
                "last_refresh": "2026-01-01T00:00:00Z"
            })
            .to_string(),
        )
        .unwrap();
        let cred = Credential {
            kind: Kind::Codex,
            path: path.clone(),
            access: "new".into(),
            account_id: Some("acct".into()),
            refresh: Some("new-r".into()),
            expires_at: Some(42),
        };
        write_back(&cred, &json!({"id_token": "new-i"})).unwrap();
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["tokens"]["access_token"], "new");
        assert_eq!(after["tokens"]["refresh_token"], "new-r");
        assert_eq!(after["tokens"]["id_token"], "new-i");
        assert_eq!(after["tokens"]["account_id"], "acct");
        assert!(after["OPENAI_API_KEY"].is_null());
        assert_ne!(after["last_refresh"], "2026-01-01T00:00:00Z");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
