//! Credentials handling, compatible with the Python toolbox:
//! same credentials file (base64 encoded INI in `~/.copernicusmarine`),
//! same environment variables, `.netrc` support and same authentication server.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::Engine;
use log::{debug, error, info, warn};

const AUTH_DOMAIN: &str = "auth.marine.copernicus.eu";
const TOKEN_ENDPOINT: &str =
    "https://auth.marine.copernicus.eu/realms/MIS/protocol/openid-connect/token";
const USERINFO_ENDPOINT: &str =
    "https://auth.marine.copernicus.eu/realms/MIS/protocol/openid-connect/userinfo";
const CREDENTIALS_FILENAME: &str = ".copernicusmarine-credentials";
const NETRC_HOSTS: [&str; 4] = [
    "nrt.cmems-du.eu",
    "my.cmems-du.eu",
    AUTH_DOMAIN,
    "default_host",
];
pub const RECOVER_CREDENTIALS_MESSAGE: &str = "Learn how to recover your credentials at: \
    https://help.marine.copernicus.eu/en/articles/\
    4444552-i-forgot-my-username-or-my-password-what-should-i-do";

#[derive(Debug, Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

pub fn default_credentials_directory() -> PathBuf {
    match std::env::var("COPERNICUSMARINE_CREDENTIALS_DIRECTORY") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home().join(".copernicusmarine"),
    }
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn env_credential(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn parse_ini(content: &str, section: &str) -> std::collections::HashMap<String, String> {
    let mut values = std::collections::HashMap::new();
    let mut in_section = false;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_section = &line[1..line.len() - 1] == section;
        } else if in_section {
            if let Some((key, value)) = line.split_once('=').or_else(|| line.split_once(':')) {
                values.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
    }
    values
}

fn read_copernicusmarine_file(path: &Path) -> Result<Option<Credentials>> {
    let encoded = std::fs::read_to_string(path)?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .with_context(|| format!("Invalid credentials file {}", path.display()))?;
    let values = parse_ini(&String::from_utf8(decoded)?, "credentials");
    Ok(match (values.get("username"), values.get("password")) {
        (Some(u), Some(p)) if !u.is_empty() && !p.is_empty() => Some(Credentials {
            username: u.clone(),
            password: p.clone(),
        }),
        _ => None,
    })
}

fn read_netrc_file(path: &Path) -> Result<Option<Credentials>> {
    let content = std::fs::read_to_string(path)?;
    let tokens: Vec<&str> = content.split_whitespace().collect();
    // machine -> (login, password)
    let mut entries: Vec<(String, Option<String>, Option<String>)> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i] {
            "machine" if i + 1 < tokens.len() => {
                entries.push((tokens[i + 1].to_string(), None, None));
                i += 2;
            }
            "default" => {
                entries.push(("default_host".to_string(), None, None));
                i += 1;
            }
            "login" if i + 1 < tokens.len() => {
                if let Some(entry) = entries.last_mut() {
                    entry.1 = Some(tokens[i + 1].to_string());
                }
                i += 2;
            }
            "password" if i + 1 < tokens.len() => {
                if let Some(entry) = entries.last_mut() {
                    entry.2 = Some(tokens[i + 1].to_string());
                }
                i += 2;
            }
            _ => i += 1,
        }
    }
    for host in NETRC_HOSTS {
        if let Some((_, Some(login), Some(password))) = entries.iter().find(|e| e.0 == host) {
            if host != AUTH_DOMAIN && host != "default_host" {
                warn!(
                    "netrc host {host} is deprecated, please use {AUTH_DOMAIN} in {}",
                    path.display()
                );
            }
            return Ok(Some(Credentials {
                username: login.clone(),
                password: password.clone(),
            }));
        }
    }
    Ok(None)
}

fn read_motu_file(path: &Path) -> Result<Option<Credentials>> {
    let values = parse_ini(&std::fs::read_to_string(path)?, "Main");
    Ok(match (values.get("user"), values.get("pwd")) {
        (Some(u), Some(p)) => Some(Credentials {
            username: u.clone(),
            password: p.clone(),
        }),
        _ => None,
    })
}

fn read_any_credentials_file(path: &Path) -> Result<Option<Credentials>> {
    let name = path.to_string_lossy();
    if name.contains("netrc") {
        read_netrc_file(path)
    } else if name.contains("motuclient") {
        warn!("The motuclient configuration file is deprecated, please use the login command.");
        read_motu_file(path)
    } else {
        read_copernicusmarine_file(path)
    }
}

/// Look for credentials in a configuration file: the user specified one,
/// or the default ones (toolbox credentials file, motuclient file, netrc).
pub fn credentials_from_files(
    credentials_file: Option<&Path>,
    configuration_directory: Option<&Path>,
) -> Result<Option<(Credentials, PathBuf)>> {
    if let Some(file) = credentials_file {
        if file.exists() {
            return Ok(read_any_credentials_file(file)?.map(|c| (c, file.to_path_buf())));
        }
    }
    let toolbox_file = configuration_directory
        .map(Path::to_path_buf)
        .unwrap_or_else(default_credentials_directory)
        .join(CREDENTIALS_FILENAME);
    let motu_file = home().join("motuclient").join("motuclient-python.ini");
    let netrc_file = home().join(if cfg!(windows) { "_netrc" } else { ".netrc" });
    let found = if toolbox_file.exists() {
        read_copernicusmarine_file(&toolbox_file)?.map(|c| (c, toolbox_file))
    } else if motu_file.exists() {
        read_motu_file(&motu_file)?.map(|c| (c, motu_file))
    } else if netrc_file.exists() {
        read_netrc_file(&netrc_file)?.map(|c| (c, netrc_file))
    } else {
        None
    };
    Ok(found)
}

fn prompt(label: &str, hide_input: bool) -> Result<String> {
    if hide_input {
        return Ok(rpassword::prompt_password(format!(
            "Copernicus Marine {label}: "
        ))?);
    }
    eprint!("Copernicus Marine {label}: ");
    std::io::stderr().flush()?;
    let mut value = String::new();
    std::io::stdin().read_line(&mut value)?;
    Ok(value.trim().to_string())
}

fn confirm(question: &str) -> Result<bool> {
    eprint!("{question} [y/N]: ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// Resolve credentials: function arguments, then environment variables,
/// then configuration files, then interactive prompt.
pub fn resolve_credentials(
    username: Option<String>,
    password: Option<String>,
    credentials_file: Option<&Path>,
) -> Result<Credentials> {
    let from_files = || {
        credentials_from_files(credentials_file, None)
            .ok()
            .flatten()
    };
    let mut file_credentials = None;
    let username = match username.or_else(|| env_credential("COPERNICUSMARINE_SERVICE_USERNAME")) {
        Some(u) => u,
        None => {
            file_credentials = from_files();
            match &file_credentials {
                Some((c, path)) => {
                    debug!("username loaded from {}", path.display());
                    c.username.clone()
                }
                None => {
                    info!(
                        "Downloading Copernicus Marine data requires a Copernicus Marine username \
                         and password, sign up for free at: https://data.marine.copernicus.eu/register"
                    );
                    prompt("username", false)?
                }
            }
        }
    };
    let password = match password.or_else(|| env_credential("COPERNICUSMARINE_SERVICE_PASSWORD")) {
        Some(p) => p,
        None => match file_credentials.or_else(from_files) {
            Some((c, _)) => c.password,
            None => prompt("password", true)?,
        },
    };
    if username.is_empty() || password.is_empty() {
        bail!("Credentials cannot be empty.");
    }
    Ok(Credentials { username, password })
}

/// Check credentials against the Copernicus Marine authentication system.
/// Returns `Some(username)` if valid, `None` if invalid, error if unreachable.
pub async fn validate(credentials: &Credentials) -> Result<Option<String>> {
    let client = reqwest::Client::builder()
        .user_agent(format!("copernicusmarine-rs/{}", crate::VERSION))
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let mut last_error = None;
    for _ in 0..3 {
        match check_with_auth_server(&client, credentials).await {
            Ok(user) => return Ok(user),
            Err(e) => {
                debug!("Could not connect with the authentication system: {e:#}");
                last_error = Some(e);
            }
        }
    }
    Err(last_error.unwrap()).context(
        "Could not connect to the Copernicus Marine authentication system \
         (auth.marine.copernicus.eu). Check your internet connection.",
    )
}

async fn check_with_auth_server(
    client: &reqwest::Client,
    credentials: &Credentials,
) -> Result<Option<String>> {
    debug!("POSTing credentials to {TOKEN_ENDPOINT}...");
    let response = client
        .post(TOKEN_ENDPOINT)
        .form(&[
            ("client_id", "toolbox"),
            ("grant_type", "password"),
            ("username", credentials.username.as_str()),
            ("password", credentials.password.as_str()),
            ("scope", "openid profile email"),
        ])
        .send()
        .await?;
    match response.status().as_u16() {
        200 => {
            let token: serde_json::Value = response.json().await?;
            let access_token = token["access_token"].as_str().context("No access token")?;
            let userinfo: serde_json::Value = client
                .get(USERINFO_ENDPOINT)
                .bearer_auth(access_token)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            Ok(userinfo["preferred_username"].as_str().map(str::to_string))
        }
        401 => Ok(None),
        // Keycloak answers 400 "invalid_grant" for a wrong username/password
        400 => {
            let body: serde_json::Value = response.json().await.unwrap_or_default();
            if body["error"].as_str() == Some("invalid_grant") {
                Ok(None)
            } else {
                bail!("Authentication server answered with status 400: {body}")
            }
        }
        status => bail!("Authentication server answered with status {status}"),
    }
}

/// Used by `get` and `subset`: resolve and validate the credentials.
pub async fn get_and_check_user(
    username: Option<String>,
    password: Option<String>,
    credentials_file: Option<&Path>,
) -> Result<String> {
    let credentials = resolve_credentials(username, password, credentials_file)?;
    match validate(&credentials).await? {
        Some(user) => Ok(user),
        None => bail!("Invalid username or password. {RECOVER_CREDENTIALS_MESSAGE}"),
    }
}

fn write_credentials_file(credentials: &Credentials, directory: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(directory)?;
    let path = directory.join(CREDENTIALS_FILENAME);
    let content = format!(
        "[credentials]\nusername={}\npassword={}\n",
        credentials.username, credentials.password
    );
    let encoded = base64::engine::general_purpose::STANDARD.encode(content);
    std::fs::write(&path, encoded)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

pub struct LoginOptions {
    pub username: Option<String>,
    pub password: Option<String>,
    pub configuration_file_directory: Option<PathBuf>,
    pub force_overwrite: bool,
    pub check_credentials_valid: bool,
}

/// `login` command. Returns whether the login succeeded.
pub async fn login(options: LoginOptions) -> Result<bool> {
    let directory = options
        .configuration_file_directory
        .clone()
        .unwrap_or_else(default_credentials_directory);

    if options.check_credentials_valid {
        let (credentials, source) = if let (Some(u), Some(p)) =
            (&options.username, &options.password)
        {
            (
                Credentials {
                    username: u.clone(),
                    password: p.clone(),
                },
                "input username and password".to_string(),
            )
        } else if let (Some(u), Some(p)) = (
            env_credential("COPERNICUSMARINE_SERVICE_USERNAME"),
            env_credential("COPERNICUSMARINE_SERVICE_PASSWORD"),
        ) {
            (
                Credentials {
                    username: u,
                    password: p,
                },
                "environment variables: COPERNICUSMARINE_SERVICE_USERNAME and \
                 COPERNICUSMARINE_SERVICE_PASSWORD"
                    .to_string(),
            )
        } else if let Some((credentials, path)) = credentials_from_files(None, Some(&directory))? {
            (
                credentials,
                format!("configuration file {}", path.display()),
            )
        } else {
            info!(
                "No credentials found. Please provide credentials as arguments or environment \
                 variables, or use the 'login' command to create a credentials file."
            );
            return Ok(false);
        };
        return Ok(if validate(&credentials).await?.is_some() {
            info!("Valid credentials from {source}.");
            true
        } else {
            error!("Invalid credentials from {source}.");
            info!("{RECOVER_CREDENTIALS_MESSAGE}");
            false
        });
    }

    let credentials_path = directory.join(CREDENTIALS_FILENAME);
    let mut overwrite_confirmed = options.force_overwrite;
    if !options.force_overwrite && credentials_path.exists() {
        if let Ok(Some(saved)) = read_copernicusmarine_file(&credentials_path) {
            if validate(&saved).await?.is_some() {
                info!(
                    "Using existing credentials from {}. Use --force-overwrite combined with \
                     credentials to always overwrite.",
                    credentials_path.display()
                );
                return Ok(true);
            }
        }
        if !confirm(&format!(
            "File {} already exists but contains invalid credentials. Do you want to overwrite?",
            credentials_path.display()
        ))? {
            info!(
                "Continuing with the existing configuration in {}.",
                credentials_path.display()
            );
            return Ok(false);
        }
        overwrite_confirmed = true;
    }

    let username = match options
        .username
        .or_else(|| env_credential("COPERNICUSMARINE_SERVICE_USERNAME"))
    {
        Some(u) => u,
        None => prompt("username", false)?,
    };
    let password = match options
        .password
        .or_else(|| env_credential("COPERNICUSMARINE_SERVICE_PASSWORD"))
    {
        Some(p) => p,
        None => prompt("password", true)?,
    };
    let credentials = Credentials { username, password };
    if validate(&credentials).await?.is_none() {
        error!("Invalid credentials. {RECOVER_CREDENTIALS_MESSAGE}");
        return Ok(false);
    }
    if credentials_path.exists()
        && !overwrite_confirmed
        && !confirm(&format!(
            "File {} already exists, overwrite it?",
            credentials_path.display()
        ))?
    {
        error!("Abort");
        return Ok(false);
    }
    let path = write_credentials_file(&credentials, &directory)?;
    info!("Credentials file stored in {}.", path.display());
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_credentials_file() {
        let dir = std::env::temp_dir().join(format!("cmt-rs-test-{}", std::process::id()));
        let credentials = Credentials {
            username: "user".into(),
            password: "p=ss".into(),
        };
        let path = write_credentials_file(&credentials, &dir).unwrap();
        let read = read_copernicusmarine_file(&path).unwrap().unwrap();
        assert_eq!(read.username, "user");
        assert_eq!(read.password, "p=ss");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn parses_netrc() {
        let path = std::env::temp_dir().join(format!("cmt-rs-netrc-{}", std::process::id()));
        std::fs::write(
            &path,
            "machine auth.marine.copernicus.eu login bob password secret\n",
        )
        .unwrap();
        let read = read_netrc_file(&path).unwrap().unwrap();
        assert_eq!(
            (read.username.as_str(), read.password.as_str()),
            ("bob", "secret")
        );
        std::fs::remove_file(path).unwrap();
    }
}
