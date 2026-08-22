use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

const SERVER_START_TIMEOUT: Duration = Duration::from_secs(20);
const MODEL_READY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct AutoStartConfig {
    pub base_url: String,
    pub api_model: String,
    pub local_model: String,
    pub context_length: Option<u32>,
}

#[derive(Deserialize)]
struct ServerStatus {
    running: bool,
    port: Option<u16>,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ApiModel>,
}

#[derive(Deserialize)]
struct ApiModel {
    id: String,
}

/// Ensure the configured local LM Studio server and model are ready.
///
/// This function only manages loopback HTTP endpoints. Remote or HTTPS URLs are
/// rejected so enabling auto-start can never accidentally alter another host.
pub fn ensure_ready(cfg: &AutoStartConfig) -> Result<()> {
    let port = local_server_port(&cfg.base_url).ok_or_else(|| {
        anyhow!(
            "LM Studio auto-start only supports a loopback HTTP URL without a path: {}",
            cfg.base_url
        )
    })?;

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(3))
        .build()
        .context("Failed to create LM Studio health-check client")?;

    if model_is_ready(&client, &cfg.base_url, &cfg.api_model) {
        tracing::info!("LM Studio model already ready: {}", cfg.api_model);
        return Ok(());
    }

    let lms = find_lms_executable();
    run_lms(lms.as_os_str(), ["daemon", "up", "--json"])
        .context("Failed to start the LM Studio headless daemon")?;

    let status_output = run_lms(lms.as_os_str(), ["server", "status", "--json"])
        .context("Failed to query the LM Studio server")?;
    let status: ServerStatus = serde_json::from_slice(&status_output.stdout)
        .context("LM Studio returned an invalid server status")?;

    if status.running {
        if status.port != Some(port) {
            bail!(
                "LM Studio server is already running on port {}, but lm_url uses port {}",
                status
                    .port
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                port
            );
        }
    } else {
        let port_arg = port.to_string();
        run_lms(
            lms.as_os_str(),
            ["server", "start", "--port", port_arg.as_str()],
        )
        .with_context(|| format!("Failed to start the LM Studio server on port {port}"))?;
    }

    wait_until(SERVER_START_TIMEOUT, || {
        server_is_reachable(&client, &cfg.base_url)
    })
    .context("LM Studio server did not become reachable")?;

    if !model_is_ready(&client, &cfg.base_url, &cfg.api_model) {
        tracing::info!(
            "Loading LM Studio model {} as {}...",
            cfg.local_model,
            cfg.api_model
        );
        let mut args = vec![
            "load".to_string(),
            cfg.local_model.clone(),
            "--identifier".to_string(),
            cfg.api_model.clone(),
            "--yes".to_string(),
        ];
        if let Some(context_length) = cfg.context_length {
            args.push("--context-length".to_string());
            args.push(context_length.to_string());
        }
        run_lms(lms.as_os_str(), args.iter().map(String::as_str))
            .with_context(|| format!("Failed to load LM Studio model {}", cfg.local_model))?;
    }

    wait_until(MODEL_READY_TIMEOUT, || {
        model_is_ready(&client, &cfg.base_url, &cfg.api_model)
    })
    .with_context(|| format!("LM Studio model {} did not become ready", cfg.api_model))?;

    tracing::info!("LM Studio model ready: {}", cfg.api_model);
    Ok(())
}

fn local_server_port(base_url: &str) -> Option<u16> {
    let url = reqwest::Url::parse(base_url).ok()?;
    if url.scheme() != "http"
        || !matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
        )
        || (url.path() != "/" && !url.path().is_empty())
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    url.port_or_known_default()
}

fn server_is_reachable(client: &reqwest::blocking::Client, base_url: &str) -> bool {
    client
        .get(format!("{}/v1/models", base_url.trim_end_matches('/')))
        .send()
        .is_ok_and(|response| response.status().is_success())
}

fn model_is_ready(client: &reqwest::blocking::Client, base_url: &str, identifier: &str) -> bool {
    client
        .get(format!("{}/v1/models", base_url.trim_end_matches('/')))
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .and_then(reqwest::blocking::Response::json::<ModelsResponse>)
        .is_ok_and(|models| models.data.iter().any(|model| model.id == identifier))
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> Result<()> {
    let started = Instant::now();
    loop {
        if predicate() {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            bail!("timed out after {} seconds", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn find_lms_executable() -> PathBuf {
    #[cfg(windows)]
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        let candidate = PathBuf::from(profile)
            .join(".lmstudio")
            .join("bin")
            .join("lms.exe");
        if candidate.is_file() {
            return candidate;
        }
    }

    PathBuf::from(if cfg!(windows) { "lms.exe" } else { "lms" })
}

fn run_lms<I, S>(executable: &OsStr, args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(executable);
    command.args(args);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let output = command.output().with_context(|| {
        format!(
            "Could not run {}; install LM Studio and launch it once so the lms CLI is available",
            executable.to_string_lossy()
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        bail!(
            "{} failed (exit {}): {}",
            executable.to_string_lossy(),
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "terminated".to_string()),
            if stderr.is_empty() { stdout } else { stderr }
        );
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::local_server_port;

    #[test]
    fn accepts_loopback_http_urls() {
        assert_eq!(local_server_port("http://localhost:1234"), Some(1234));
        assert_eq!(local_server_port("http://127.0.0.1:4321/"), Some(4321));
        assert_eq!(local_server_port("http://[::1]:1234"), Some(1234));
    }

    #[test]
    fn rejects_remote_or_unsafe_urls() {
        assert_eq!(local_server_port("http://192.168.1.10:1234"), None);
        assert_eq!(local_server_port("https://localhost:1234"), None);
        assert_eq!(local_server_port("http://localhost:1234/proxy"), None);
        assert_eq!(local_server_port("not-a-url"), None);
    }
}
