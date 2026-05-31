// Tauri v2 backend for EasyCLI
// Ports core Electron main.js logic to Rust with a simpler API surface (KISS)

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use futures_util::StreamExt;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rand::Rng;
use rfd::FileDialog;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::io::{self, BufRead, BufReader, Read, Write};
#[cfg(not(target_os = "windows"))]
use std::os::unix::process::CommandExt;
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tauri::tray::TrayIcon;
use tauri::WindowEvent;
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use thiserror::Error;
use tokio::time::sleep;

static PROCESS_PID: Lazy<Arc<Mutex<Option<u32>>>> = Lazy::new(|| Arc::new(Mutex::new(None)));
static TRAY_ICON: Lazy<Arc<Mutex<Option<TrayIcon>>>> = Lazy::new(|| Arc::new(Mutex::new(None)));
static CALLBACK_SERVERS: Lazy<Arc<Mutex<HashMap<u16, (Arc<AtomicBool>, thread::JoinHandle<()>)>>>> =
    Lazy::new(|| Arc::new(Mutex::new(HashMap::new())));
// Keep-alive mechanism for Local mode
static KEEP_ALIVE_HANDLE: Lazy<Arc<Mutex<Option<(Arc<AtomicBool>, thread::JoinHandle<()>)>>>> =
    Lazy::new(|| Arc::new(Mutex::new(None)));
// Store the password used to start CLIProxyAPI for keep-alive authentication
static CLI_PROXY_PASSWORD: Lazy<Arc<Mutex<Option<String>>>> =
    Lazy::new(|| Arc::new(Mutex::new(None)));
static KEEP_ALIVE_RESTARTING: AtomicBool = AtomicBool::new(false);
const RUNTIME_LOG_RETENTION_DAYS: usize = 14;

#[derive(Error, Debug)]
enum AppError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("Zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("Other: {0}")]
    Other(String),
}

fn home_dir() -> Result<PathBuf, AppError> {
    home::home_dir().ok_or_else(|| AppError::Other("Failed to resolve home directory".into()))
}

fn app_dir() -> Result<PathBuf, AppError> {
    Ok(home_dir()?.join("cliproxyapi"))
}

fn resolve_path(input: &str, base: Option<&Path>) -> PathBuf {
    if input.is_empty() {
        return PathBuf::new();
    }
    if input.starts_with('~') {
        if let Some(h) = home::home_dir() {
            if input == "~" {
                return h;
            }
            if input.starts_with("~/") {
                return h.join(&input[2..]);
            }
            return h.join(&input[1..]);
        }
    }
    let p = PathBuf::from(input);
    if p.is_absolute() {
        return p;
    }
    if let Some(base) = base {
        return base.join(p);
    }
    p
}

#[derive(Serialize, Deserialize, Debug)]
struct VersionInfo {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "camelCase")]
struct RuntimeHealthSummary {
    last_auto_restart_at: Option<String>,
    last_auto_restart_reason: Option<String>,
    last_auto_restart_result: Option<String>,
    last_auto_restart_details: Option<String>,
    last_auto_restart_port: Option<u16>,
    last_auto_restart_trigger_failures: Option<u32>,
    retention_days: usize,
    available_log_files: usize,
    latest_log_file: Option<String>,
    log_directory: String,
    status_file: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Asset {
    name: String,
    browser_download_url: String,
}

#[derive(Serialize)]
#[allow(non_snake_case)]
struct OpResult {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    needsUpdate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    isLatest: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latestVersion: Option<String>,
}

fn compare_versions(a: &str, b: &str) -> i32 {
    let pa: Vec<i32> = a.split('.').filter_map(|s| s.parse().ok()).collect();
    let pb: Vec<i32> = b.split('.').filter_map(|s| s.parse().ok()).collect();
    let len = pa.len().max(pb.len());
    for i in 0..len {
        let va = *pa.get(i).unwrap_or(&0);
        let vb = *pb.get(i).unwrap_or(&0);
        if va > vb {
            return 1;
        }
        if va < vb {
            return -1;
        }
    }
    0
}

fn current_local_info() -> Result<Option<(String, PathBuf)>, AppError> {
    let dir = app_dir()?;
    let version_file = dir.join("version.txt");
    if !version_file.exists() {
        return Ok(None);
    }
    let ver = fs::read_to_string(&version_file)?.trim().to_string();
    let path = dir.join(&ver);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some((ver, path)))
}

fn ensure_config(version_path: &Path) -> Result<(), AppError> {
    let dir = app_dir()?;
    let config = dir.join("config.yaml");
    if config.exists() {
        return Ok(());
    }
    let example = version_path.join("config.example.yaml");
    if example.exists() {
        fs::copy(example, &config)?;
    }
    Ok(())
}

fn parse_proxy(proxy_url: &str, builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    if proxy_url.is_empty() {
        return builder;
    }

    // Parse proxy URL to extract protocol, host, port, and optional auth
    match parse_proxy_url(proxy_url) {
        Ok(proxy_config) => {
            let proxy_builder = match proxy_config.protocol.as_str() {
                "http" | "https" => {
                    let url = if proxy_config.username.is_some() && proxy_config.password.is_some()
                    {
                        format!(
                            "{}://{}:{}@{}:{}",
                            proxy_config.protocol,
                            proxy_config.username.unwrap(),
                            proxy_config.password.unwrap(),
                            proxy_config.host,
                            proxy_config.port
                        )
                    } else {
                        format!(
                            "{}://{}:{}",
                            proxy_config.protocol, proxy_config.host, proxy_config.port
                        )
                    };
                    reqwest::Proxy::all(&url)
                }
                "socks5" => {
                    let url = if proxy_config.username.is_some() && proxy_config.password.is_some()
                    {
                        format!(
                            "socks5://{}:{}@{}:{}",
                            proxy_config.username.unwrap(),
                            proxy_config.password.unwrap(),
                            proxy_config.host,
                            proxy_config.port
                        )
                    } else {
                        format!("socks5://{}:{}", proxy_config.host, proxy_config.port)
                    };
                    reqwest::Proxy::all(&url)
                }
                _ => {
                    // Fallback to original behavior for unsupported protocols
                    return match reqwest::Proxy::all(proxy_url) {
                        Ok(p) => builder.proxy(p),
                        Err(_) => builder,
                    };
                }
            };

            match proxy_builder {
                Ok(proxy) => builder.proxy(proxy),
                Err(_) => builder,
            }
        }
        Err(_) => {
            // Fallback to original behavior if parsing fails
            match reqwest::Proxy::all(proxy_url) {
                Ok(p) => builder.proxy(p),
                Err(_) => builder,
            }
        }
    }
}

#[derive(Debug)]
struct ProxyConfig {
    protocol: String,
    host: String,
    port: u16,
    username: Option<String>,
    password: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_proxy_url() {
        // Test HTTP proxy without auth
        let result = parse_proxy_url("http://proxy.example.com:8080");
        assert!(result.is_ok());
        let config = result.unwrap();
        assert_eq!(config.protocol, "http");
        assert_eq!(config.host, "proxy.example.com");
        assert_eq!(config.port, 8080);
        assert!(config.username.is_none());
        assert!(config.password.is_none());

        // Test HTTPS proxy with auth
        let result = parse_proxy_url("https://user:pass@proxy.example.com:3128");
        assert!(result.is_ok());
        let config = result.unwrap();
        assert_eq!(config.protocol, "https");
        assert_eq!(config.host, "proxy.example.com");
        assert_eq!(config.port, 3128);
        assert_eq!(config.username, Some("user".to_string()));
        assert_eq!(config.password, Some("pass".to_string()));

        // Test SOCKS5 proxy without auth
        let result = parse_proxy_url("socks5://127.0.0.1:1080");
        assert!(result.is_ok());
        let config = result.unwrap();
        assert_eq!(config.protocol, "socks5");
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 1080);
        assert!(config.username.is_none());
        assert!(config.password.is_none());

        // Test SOCKS5 proxy with auth
        let result = parse_proxy_url("socks5://myuser:mypass@192.168.1.1:1080");
        assert!(result.is_ok());
        let config = result.unwrap();
        assert_eq!(config.protocol, "socks5");
        assert_eq!(config.host, "192.168.1.1");
        assert_eq!(config.port, 1080);
        assert_eq!(config.username, Some("myuser".to_string()));
        assert_eq!(config.password, Some("mypass".to_string()));

        // Test invalid formats
        assert!(parse_proxy_url("invalid").is_err());
        assert!(parse_proxy_url("ftp://proxy:8080").is_err());
        assert!(parse_proxy_url("http://proxy").is_err());
        assert!(parse_proxy_url("http://user@proxy:8080").is_err());
    }
}

fn parse_proxy_url(proxy_url: &str) -> Result<ProxyConfig, String> {
    // Remove any whitespace
    let url = proxy_url.trim();

    // Parse URL format: protocol://[user:pass@]host:port
    if let Some(colon_pos) = url.find("://") {
        let protocol = &url[..colon_pos].to_lowercase();
        let rest = &url[colon_pos + 3..];

        // Check if protocol is supported
        if !["http", "https", "socks5"].contains(&protocol.as_str()) {
            return Err(format!("Unsupported proxy protocol: {}", protocol));
        }

        // Parse host:port and optional auth
        let (host_port, username, password) = if let Some(at_pos) = rest.find('@') {
            // Has authentication: user:pass@host:port
            let auth_part = &rest[..at_pos];
            let host_port_part = &rest[at_pos + 1..];

            if let Some(colon_pos) = auth_part.find(':') {
                let user = &auth_part[..colon_pos];
                let pass = &auth_part[colon_pos + 1..];
                (
                    host_port_part,
                    Some(user.to_string()),
                    Some(pass.to_string()),
                )
            } else {
                return Err(
                    "Invalid proxy authentication format. Expected user:pass@host:port".to_string(),
                );
            }
        } else {
            // No authentication: host:port
            (rest, None, None)
        };

        // Parse host:port
        if let Some(colon_pos) = host_port.rfind(':') {
            let host = &host_port[..colon_pos];
            let port_str = &host_port[colon_pos + 1..];

            if let Ok(port) = port_str.parse::<u16>() {
                Ok(ProxyConfig {
                    protocol: protocol.to_string(),
                    host: host.to_string(),
                    port,
                    username,
                    password,
                })
            } else {
                Err(format!("Invalid port number: {}", port_str))
            }
        } else {
            Err("Invalid proxy format. Expected protocol://host:port or protocol://user:pass@host:port".to_string())
        }
    } else {
        Err("Invalid proxy URL format. Expected protocol://host:port".to_string())
    }
}

async fn fetch_latest_release(proxy_url: String) -> Result<VersionInfo, AppError> {
    let client_builder = reqwest::Client::builder()
        .user_agent("EasyCLI-Updater/1.0")
        .timeout(Duration::from_secs(30)) // Increased from 15s for slower connections
        .connect_timeout(Duration::from_secs(15));

    let client = parse_proxy(&proxy_url, client_builder)
        .build()
        .map_err(|e| AppError::Other(format!("Failed to create HTTP client: {}", e)))?;

    let mut last_err = None;
    for attempt in 0..3 {
        if attempt > 0 {
            println!("[FETCH] Retry attempt {}/3 after 2s delay", attempt + 1);
            sleep(Duration::from_secs(2)).await;
        }
        match client
            .get("https://api.github.com/repos/router-for-me/CLIProxyAPI/releases/latest")
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .await
        {
            Ok(resp) => match resp.error_for_status() {
                Ok(r) => return Ok(r.json::<VersionInfo>().await?),
                Err(e) => {
                    last_err = Some(AppError::Http(e));
                    println!("[FETCH] HTTP error: {:?}", last_err);
                }
            },
            Err(e) => {
                last_err = Some(AppError::Http(e));
                println!("[FETCH] Connection error: {:?}", last_err);
            }
        }
    }
    let err_msg = if !proxy_url.is_empty() {
        format!(
            "Failed to fetch latest release info after 3 attempts.\n\
             Proxy: {}\n\
             Error: {:?}\n\
             Please check your proxy settings and try again.",
            proxy_url, last_err
        )
    } else {
        format!(
            "Failed to fetch latest release info after 3 attempts.\n\
             Error: {:?}\n\
             Please check your network connection and try again.",
            last_err
        )
    };
    Err(last_err.unwrap_or_else(|| AppError::Other(err_msg)))
}

#[tauri::command]
async fn check_version_and_download(
    window: tauri::Window,
    proxy_url: Option<String>,
) -> Result<serde_json::Value, String> {
    let proxy = proxy_url.unwrap_or_default();
    let dir = app_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let local = current_local_info().map_err(|e| e.to_string())?;
    window
        .emit("download-status", json!({"status": "checking"}))
        .ok();
    let release = match fetch_latest_release(proxy.clone()).await {
        Ok(r) => r,
        Err(e) => {
            let msg = e.to_string();
            window
                .emit("download-status", json!({"status": "failed", "error": msg}))
                .ok();
            return Ok(json!({"success": false, "error": msg}));
        }
    };
    let latest = release.tag_name.trim_start_matches('v').to_string();

    if let Some((ver, path)) = local {
        let cmp = compare_versions(&ver, &latest);
        ensure_config(&path).map_err(|e| e.to_string())?;
        if cmp >= 0 {
            window
                .emit(
                    "download-status",
                    json!({"status": "latest", "version": ver}),
                )
                .ok();
            return Ok(json!(OpResult {
                success: true,
                error: None,
                path: Some(path.to_string_lossy().to_string()),
                version: Some(ver),
                needsUpdate: Some(false),
                isLatest: Some(true),
                latestVersion: None
            }));
        } else {
            window
                .emit(
                    "download-status",
                    json!({"status": "update-available", "version": ver, "latest": latest}),
                )
                .ok();
            return Ok(json!(OpResult {
                success: true,
                error: None,
                path: Some(path.to_string_lossy().to_string()),
                version: Some(ver),
                needsUpdate: Some(true),
                isLatest: Some(false),
                latestVersion: Some(latest)
            }));
        }
    }
    // No local found
    Ok(json!(OpResult {
        success: true,
        error: None,
        path: None,
        version: None,
        needsUpdate: Some(true),
        isLatest: Some(false),
        latestVersion: Some(latest)
    }))
}

#[tauri::command]
async fn download_cliproxyapi(
    window: tauri::Window,
    proxy_url: Option<String>,
) -> Result<serde_json::Value, String> {
    let proxy = proxy_url.unwrap_or_default();
    let dir = app_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let release = fetch_latest_release(proxy.clone())
        .await
        .map_err(|e| e.to_string())?;
    let latest = release.tag_name.trim_start_matches('v').to_string();

    let platform = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let filename = match (platform, arch) {
        ("macos", "aarch64") => format!("CLIProxyAPI_{}_darwin_arm64.tar.gz", latest),
        ("macos", "x86_64") => format!("CLIProxyAPI_{}_darwin_amd64.tar.gz", latest),
        ("linux", "x86_64") => format!("CLIProxyAPI_{}_linux_amd64.tar.gz", latest),
        ("linux", "aarch64") => format!("CLIProxyAPI_{}_linux_arm64.tar.gz", latest),
        ("windows", "x86_64") => format!("CLIProxyAPI_{}_windows_amd64.zip", latest),
        ("windows", "aarch64") => format!("CLIProxyAPI_{}_windows_arm64.zip", latest),
        _ => return Err(format!("Unsupported platform: {} {}", platform, arch)),
    };
    let asset = release
        .assets
        .into_iter()
        .find(|a| a.name == filename)
        .ok_or_else(|| format!("No suitable download file found: {}", filename))?;

    let download_path = dir.join(&filename);
    window
        .emit("download-status", json!({"status": "starting"}))
        .ok();

    // Download with progress
    // Build client with proper configuration for GitHub downloads
    let client_builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(600)) // 10 minutes total timeout for large files
        .connect_timeout(Duration::from_secs(60)) // 60 seconds to establish connection
        .user_agent("EasyCLI-Updater/1.0") // GitHub requires User-Agent
        .tcp_keepalive(Duration::from_secs(30)) // Keep connection alive during slow downloads
        .danger_accept_invalid_certs(false); // Secure by default

    let client = parse_proxy(&proxy, client_builder)
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    println!("[DOWNLOAD] Starting download from: {}", asset.browser_download_url);
    println!("[DOWNLOAD] Proxy: {}", if proxy.is_empty() { "none" } else { &proxy });

    let resp = client
        .get(&asset.browser_download_url)
        .send()
        .await
        .map_err(|e| {
            let error_msg = format!(
                "Failed to connect to GitHub. Error: {}\n\
                 URL: {}\n\
                 Proxy: {}\n\
                 Please check your network connection and proxy settings.",
                e,
                asset.browser_download_url,
                if proxy.is_empty() { "none".to_string() } else { proxy.clone() }
            );
            println!("[DOWNLOAD ERROR] {}", error_msg);
            error_msg
        })?;
    if !resp.status().is_success() {
        return Err(format!("Download failed, status: {}", resp.status()));
    }
    let total = resp.content_length().unwrap_or(0);
    let mut file = fs::File::create(&download_path).map_err(|e| e.to_string())?;
    let mut downloaded: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| e.to_string())?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        downloaded += bytes.len() as u64;
        let progress = if total > 0 {
            (downloaded as f64 / total as f64) * 100.0
        } else {
            0.0
        };
        window
            .emit(
                "download-progress",
                json!({"progress": progress, "downloaded": downloaded, "total": total}),
            )
            .ok();
    }

    // Extract
    let extract_path = dir.join(&latest);
    if download_path.extension().and_then(|e| e.to_str()) == Some("zip") {
        extract_zip(&download_path, &extract_path).map_err(|e| e.to_string())?;
    } else {
        extract_targz(&download_path, &extract_path).map_err(|e| e.to_string())?;
    }
    // Save version.txt
    fs::write(dir.join("version.txt"), &latest).map_err(|e| e.to_string())?;
    // Cleanup old versions - remove version directories that don't match the latest
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.metadata() {
                if metadata.is_dir() {
                    let dir_name = entry.file_name();
                    let dir_name_str = dir_name.to_string_lossy();
                    // Check if it's a version directory (starts with digit) and not the latest
                    if dir_name_str
                        .chars()
                        .next()
                        .map(|c| c.is_ascii_digit())
                        .unwrap_or(false)
                        && dir_name_str != latest
                    {
                        println!("[CLEANUP] Removing old version: {}", dir_name_str);
                        let _ = fs::remove_dir_all(entry.path());
                    }
                }
            }
        }
    }
    // Cleanup downloaded archive
    let _ = fs::remove_file(&download_path);

    // Ensure config exists
    ensure_config(&extract_path).map_err(|e| e.to_string())?;

    window
        .emit(
            "download-status",
            json!({"status": "completed", "version": latest}),
        )
        .ok();
    Ok(json!(OpResult {
        success: true,
        error: None,
        path: Some(extract_path.to_string_lossy().to_string()),
        version: Some(latest),
        needsUpdate: None,
        isLatest: None,
        latestVersion: None
    }))
}

fn extract_zip(zip_path: &Path, dest: &Path) -> Result<(), AppError> {
    fs::create_dir_all(dest)?;
    let file = fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    for i in 0..archive.len() {
        let mut f = archive.by_index(i)?;
        let outpath = dest.join(f.mangled_name());
        if f.name().ends_with('/') {
            fs::create_dir_all(&outpath)?;
        } else {
            if let Some(p) = outpath.parent() {
                fs::create_dir_all(p)?;
            }
            let mut outfile = fs::File::create(&outpath)?;
            io::copy(&mut f, &mut outfile)?;
        }
    }
    Ok(())
}

fn extract_targz(tar_gz_path: &Path, dest: &Path) -> Result<(), AppError> {
    fs::create_dir_all(dest)?;
    let tar_gz = fs::File::open(tar_gz_path)?;
    let dec = flate2::read::GzDecoder::new(tar_gz);
    let mut archive = tar::Archive::new(dec);
    archive.unpack(dest)?;
    Ok(())
}

#[tauri::command]
fn check_secret_key() -> Result<serde_json::Value, String> {
    let dir = app_dir().map_err(|e| e.to_string())?;
    let config_path = dir.join("config.yaml");
    if !config_path.exists() {
        return Ok(json!({"needsPassword": true, "reason": "Config file missing"}));
    }
    let content = fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
    let value: serde_yaml::Value = serde_yaml::from_str(&content).map_err(|e| e.to_string())?;
    let rm = value
        .get("remote-management")
        .and_then(|v| v.as_mapping())
        .cloned();
    if let Some(map) = rm {
        if let Some(sk) = map.get(&serde_yaml::Value::from("secret-key")) {
            if sk.as_str().map(|s| !s.trim().is_empty()).unwrap_or(false) {
                return Ok(json!({"needsPassword": false}));
            }
        }
    }
    Ok(json!({"needsPassword": true, "reason": "Missing secret-key"}))
}

#[derive(Deserialize)]
struct UpdateSecretKeyArgs {
    secret_key: String,
}

#[tauri::command]
fn update_secret_key(args: UpdateSecretKeyArgs) -> Result<serde_json::Value, String> {
    let secret_key = args.secret_key;
    let dir = app_dir().map_err(|e| e.to_string())?;
    let p = dir.join("config.yaml");

    // Create directory if it doesn't exist
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let mut v: serde_yaml::Value = if p.exists() {
        let content = fs::read_to_string(&p).map_err(|e| e.to_string())?;
        serde_yaml::from_str(&content).map_err(|e| e.to_string())?
    } else {
        // Create a new empty config if file doesn't exist
        serde_yaml::Value::Mapping(Default::default())
    };

    // Ensure the value is a mapping
    if !v.is_mapping() {
        v = serde_yaml::Value::Mapping(Default::default());
    }

    let m = v
        .as_mapping_mut()
        .ok_or("Failed to create config mapping")?;
    let entry = m
        .entry(serde_yaml::Value::from("remote-management"))
        .or_insert_with(|| serde_yaml::Value::Mapping(Default::default()));

    // Ensure remote-management is a mapping
    if !entry.is_mapping() {
        *entry = serde_yaml::Value::Mapping(Default::default());
    }

    let map = entry
        .as_mapping_mut()
        .ok_or("Failed to create remote-management mapping")?;
    map.insert(
        serde_yaml::Value::from("secret-key"),
        serde_yaml::Value::from(secret_key),
    );

    let out = serde_yaml::to_string(&v).map_err(|e| e.to_string())?;
    fs::write(&p, out).map_err(|e| e.to_string())?;
    Ok(json!({"success": true}))
}

#[tauri::command]
fn read_config_yaml() -> Result<serde_json::Value, String> {
    let dir = app_dir().map_err(|e| e.to_string())?;
    let p = dir.join("config.yaml");
    if !p.exists() {
        return Ok(json!({}));
    }
    let content = fs::read_to_string(&p).map_err(|e| e.to_string())?;
    let v: serde_yaml::Value = serde_yaml::from_str(&content).map_err(|e| e.to_string())?;
    let json_v = serde_json::to_value(v).map_err(|e| e.to_string())?;
    Ok(json_v)
}

#[tauri::command]
fn update_config_yaml(
    endpoint: String,
    value: serde_json::Value,
    is_delete: Option<bool>,
) -> Result<serde_json::Value, String> {
    let dir = app_dir().map_err(|e| e.to_string())?;
    let p = dir.join("config.yaml");
    if !p.exists() {
        return Err("Configuration file does not exist".into());
    }
    let content = fs::read_to_string(&p).map_err(|e| e.to_string())?;
    let mut conf: serde_yaml::Value = serde_yaml::from_str(&content).map_err(|e| e.to_string())?;
    let parts: Vec<&str> = endpoint.split('.').collect();
    // Descend mapping
    let mut current = conf.as_mapping_mut().ok_or("Invalid config structure")?;
    for (i, part) in parts.iter().enumerate() {
        let key = serde_yaml::Value::from(*part);
        if i == parts.len() - 1 {
            if is_delete.unwrap_or(false) {
                current.remove(&key);
            } else {
                current.insert(
                    key,
                    serde_yaml::to_value(&value).map_err(|e| e.to_string())?,
                );
            }
        } else {
            let entry = current
                .entry(key)
                .or_insert_with(|| serde_yaml::Value::Mapping(Default::default()));
            if let Some(map) = entry.as_mapping_mut() {
                current = map;
            } else {
                return Err("Invalid nested config path".into());
            }
        }
    }
    let out = serde_yaml::to_string(&conf).map_err(|e| e.to_string())?;
    fs::write(&p, out).map_err(|e| e.to_string())?;
    Ok(json!({"success": true}))
}

#[tauri::command]
fn read_local_auth_files() -> Result<serde_json::Value, String> {
    let dir = app_dir().map_err(|e| e.to_string())?;
    let p = dir.join("config.yaml");
    if !p.exists() {
        return Ok(json!([]));
    }
    let content = fs::read_to_string(&p).map_err(|e| e.to_string())?;
    let conf: serde_yaml::Value = serde_yaml::from_str(&content).map_err(|e| e.to_string())?;
    let auth_dir = conf.get("auth-dir").and_then(|v| v.as_str()).unwrap_or("");
    if auth_dir.is_empty() {
        return Ok(json!([]));
    }
    let base = p.parent().unwrap();
    let ad = resolve_path(auth_dir, Some(base));
    if !ad.exists() {
        return Ok(json!([]));
    }
    let mut result = vec![];
    for entry in fs::read_dir(ad).map_err(|e| e.to_string())? {
        let e = entry.map_err(|e| e.to_string())?;
        let path = e.path();
        if path.is_file() {
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                if name.to_lowercase().ends_with(".json") {
                    let meta = e.metadata().map_err(|e| e.to_string())?;
                    let mut file_type = "unknown".to_string();
                    if let Ok(mut f) = fs::File::open(&path) {
                        let mut s = String::new();
                        let _ = f.read_to_string(&mut s);
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                            if let Some(t) = v.get("type").and_then(|x| x.as_str()) {
                                file_type = t.to_string();
                            }
                        }
                    }
                    let mod_ms = meta
                        .modified()
                        .ok()
                        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    result.push(json!({
                        "name": name,
                        "size": meta.len(),
                        "modtime": mod_ms,
                        "type": file_type
                    }));
                }
            }
        }
    }
    Ok(json!(result))
}

#[derive(Deserialize)]
struct UploadFile {
    name: String,
    content: String,
}

#[tauri::command]
fn upload_local_auth_files(files: Vec<UploadFile>) -> Result<serde_json::Value, String> {
    let dir = app_dir().map_err(|e| e.to_string())?;
    let p = dir.join("config.yaml");
    if !p.exists() {
        return Err("Configuration file does not exist".into());
    }
    let content = fs::read_to_string(&p).map_err(|e| e.to_string())?;
    let conf: serde_yaml::Value = serde_yaml::from_str(&content).map_err(|e| e.to_string())?;
    let auth_dir = conf
        .get("auth-dir")
        .and_then(|v| v.as_str())
        .ok_or("auth-dir not configured in config.yaml")?;
    let base = p.parent().unwrap();
    let ad = resolve_path(auth_dir, Some(base));
    fs::create_dir_all(&ad).map_err(|e| e.to_string())?;
    let mut success = 0usize;
    let mut errors = vec![];
    let mut error_count = 0usize;
    for f in files {
        let path = ad.join(&f.name);
        if path.exists() {
            errors.push(format!("{}: File already exists", f.name));
            error_count += 1;
            continue;
        }
        if let Err(e) = fs::write(&path, f.content.as_bytes()) {
            errors.push(format!("{}: {}", f.name, e));
            error_count += 1;
        } else {
            success += 1;
        }
    }
    Ok(
        json!({"success": success>0, "successCount": success, "errorCount": error_count, "errors": if errors.is_empty(){serde_json::Value::Null}else{json!(errors)} }),
    )
}

#[tauri::command]
fn delete_local_auth_files(filenames: Vec<String>) -> Result<serde_json::Value, String> {
    let dir = app_dir().map_err(|e| e.to_string())?;
    let p = dir.join("config.yaml");
    if !p.exists() {
        return Err("Configuration file does not exist".into());
    }
    let content = fs::read_to_string(&p).map_err(|e| e.to_string())?;
    let conf: serde_yaml::Value = serde_yaml::from_str(&content).map_err(|e| e.to_string())?;
    let auth_dir = conf
        .get("auth-dir")
        .and_then(|v| v.as_str())
        .ok_or("auth-dir not configured in config.yaml")?;
    let base = p.parent().unwrap();
    let ad = resolve_path(auth_dir, Some(base));
    if !ad.exists() {
        return Err("Authentication file directory does not exist".into());
    }
    let mut success = 0usize;
    let mut error_count = 0usize;
    for name in filenames {
        let path = ad.join(&name);
        match fs::remove_file(&path) {
            Ok(_) => success += 1,
            Err(_) => error_count += 1,
        }
    }
    Ok(json!({"success": success>0, "successCount": success, "errorCount": error_count}))
}

#[tauri::command]
fn download_local_auth_files(filenames: Vec<String>) -> Result<serde_json::Value, String> {
    let dir = app_dir().map_err(|e| e.to_string())?;
    let p = dir.join("config.yaml");
    if !p.exists() {
        return Err("Configuration file does not exist".into());
    }
    let content = fs::read_to_string(&p).map_err(|e| e.to_string())?;
    let conf: serde_yaml::Value = serde_yaml::from_str(&content).map_err(|e| e.to_string())?;
    let auth_dir = conf
        .get("auth-dir")
        .and_then(|v| v.as_str())
        .ok_or("auth-dir not configured in config.yaml")?;
    let base = p.parent().unwrap();
    let ad = resolve_path(auth_dir, Some(base));
    if !ad.exists() {
        return Err("Authentication file directory does not exist".into());
    }
    let mut files = vec![];
    let mut error_count = 0usize;
    for name in filenames {
        let path = ad.join(&name);
        match fs::read_to_string(&path) {
            Ok(c) => files.push(json!({"name": name, "content": c})),
            Err(_) => error_count += 1,
        }
    }
    Ok(json!({"success": !files.is_empty(), "files": files, "errorCount": error_count}))
}

fn find_executable(version_path: &Path) -> Option<PathBuf> {
    let mut exe = PathBuf::from("cli-proxy-api");
    if cfg!(target_os = "windows") {
        exe.set_extension("exe");
    }
    let path = version_path.join(exe);
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

fn generate_random_password() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

fn ensure_local_management_secret(conf: &mut serde_yaml::Value) -> Result<String, String> {
    let mapping = conf
        .as_mapping_mut()
        .ok_or("Invalid config structure")?;
    let remote_management_key = serde_yaml::Value::from("remote-management");
    let secret_key = serde_yaml::Value::from("secret-key");

    let remote_management = mapping
        .entry(remote_management_key)
        .or_insert_with(|| serde_yaml::Value::Mapping(Default::default()));

    if !remote_management.is_mapping() {
        *remote_management = serde_yaml::Value::Mapping(Default::default());
    }

    let remote_management_map = remote_management
        .as_mapping_mut()
        .ok_or("Invalid remote-management config structure")?;

    if let Some(existing) = remote_management_map.get(&secret_key).and_then(|v| v.as_str()) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    let generated = generate_random_password();
    remote_management_map.insert(secret_key, serde_yaml::Value::from(generated.as_str()));
    Ok(generated)
}

fn read_config_yaml_value() -> Result<serde_yaml::Value, String> {
    let dir = app_dir().map_err(|e| e.to_string())?;
    let path = dir.join("config.yaml");
    if !path.exists() {
        return Err("Configuration file does not exist".into());
    }
    let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    serde_yaml::from_str(&content).map_err(|e| e.to_string())
}

fn write_config_yaml_value(conf: &serde_yaml::Value) -> Result<(), String> {
    let dir = app_dir().map_err(|e| e.to_string())?;
    let path = dir.join("config.yaml");
    let updated_content = serde_yaml::to_string(conf).map_err(|e| e.to_string())?;
    fs::write(&path, updated_content).map_err(|e| e.to_string())
}

fn read_local_management_secret_from_config(conf: &serde_yaml::Value) -> Option<String> {
    conf.get("remote-management")
        .and_then(|v| v.as_mapping())
        .and_then(|map| map.get(&serde_yaml::Value::from("secret-key")))
        .and_then(|v| v.as_str())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn cache_local_management_secret(password: Option<&str>) {
    *CLI_PROXY_PASSWORD.lock() = password.map(|value| value.to_string());
}

#[cfg(target_os = "windows")]
fn runtime_log_date_and_timestamp() -> (String, String) {
    #[repr(C)]
    struct SystemTimeParts {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }

    unsafe extern "system" {
        fn GetLocalTime(system_time: *mut SystemTimeParts);
    }

    let mut system_time = SystemTimeParts {
        year: 0,
        month: 0,
        day_of_week: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        milliseconds: 0,
    };

    unsafe {
        GetLocalTime(&mut system_time);
    }

    let date = format!(
        "{:04}-{:02}-{:02}",
        system_time.year, system_time.month, system_time.day
    );
    let timestamp = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        system_time.year,
        system_time.month,
        system_time.day,
        system_time.hour,
        system_time.minute,
        system_time.second,
        system_time.milliseconds
    );

    (date, timestamp)
}

#[cfg(all(unix, not(target_os = "windows")))]
fn runtime_log_date_and_timestamp() -> (String, String) {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut now = seconds;
    let mut local_tm = libc::tm {
        tm_sec: 0,
        tm_min: 0,
        tm_hour: 0,
        tm_mday: 0,
        tm_mon: 0,
        tm_year: 0,
        tm_wday: 0,
        tm_yday: 0,
        tm_isdst: 0,
        #[cfg(any(
            target_os = "android",
            target_os = "emscripten",
            target_os = "freebsd",
            target_os = "linux",
            target_os = "macos"
        ))]
        tm_gmtoff: 0,
        #[cfg(any(
            target_os = "android",
            target_os = "emscripten",
            target_os = "freebsd",
            target_os = "linux",
            target_os = "macos"
        ))]
        tm_zone: std::ptr::null_mut(),
    };

    unsafe {
        libc::localtime_r(&mut now, &mut local_tm);
    }

    let date = format!(
        "{:04}-{:02}-{:02}",
        local_tm.tm_year + 1900,
        local_tm.tm_mon + 1,
        local_tm.tm_mday
    );
    let timestamp = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        local_tm.tm_year + 1900,
        local_tm.tm_mon + 1,
        local_tm.tm_mday,
        local_tm.tm_hour,
        local_tm.tm_min,
        local_tm.tm_sec
    );

    (date, timestamp)
}

fn append_runtime_log(level: &str, message: &str) {
    let (date, ts) = runtime_log_date_and_timestamp();

    let dir = match app_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("[RUNTIME_LOG][ERROR] failed to resolve app dir: {}", e);
            return;
        }
    };

    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("[RUNTIME_LOG][ERROR] failed to create app dir: {}", e);
        return;
    }

    let path = dir.join(format!("easycli-runtime-{}.log", date));
    let line = format!("[{}][{}] {}\n", ts, level, message);
    match fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut file) => {
            if let Err(e) = file.write_all(line.as_bytes()) {
                eprintln!("[RUNTIME_LOG][ERROR] failed to write log file: {}", e);
            }
        }
        Err(e) => {
            eprintln!("[RUNTIME_LOG][ERROR] failed to open log file: {}", e);
        }
    }

    cleanup_old_runtime_logs(&dir);
}

fn runtime_status_path(dir: &Path) -> PathBuf {
    dir.join("easycli-runtime-status.json")
}

fn list_runtime_log_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut runtime_logs: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("easycli-runtime-") && name.ends_with(".log"))
                .unwrap_or(false)
        })
        .collect();

    runtime_logs.sort_by(|a, b| {
        let a_name = a.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        let b_name = b.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        a_name.cmp(b_name)
    });

    Ok(runtime_logs)
}

fn default_runtime_health_summary(dir: &Path) -> RuntimeHealthSummary {
    RuntimeHealthSummary {
        retention_days: RUNTIME_LOG_RETENTION_DAYS,
        log_directory: dir.to_string_lossy().to_string(),
        status_file: runtime_status_path(dir).to_string_lossy().to_string(),
        ..RuntimeHealthSummary::default()
    }
}

fn refresh_runtime_health_summary_metadata(summary: &mut RuntimeHealthSummary, dir: &Path) {
    summary.retention_days = RUNTIME_LOG_RETENTION_DAYS;
    summary.log_directory = dir.to_string_lossy().to_string();
    summary.status_file = runtime_status_path(dir).to_string_lossy().to_string();

    match list_runtime_log_files(dir) {
        Ok(logs) => {
            summary.available_log_files = logs.len();
            summary.latest_log_file = logs
                .last()
                .map(|path| path.to_string_lossy().to_string());
        }
        Err(_) => {
            summary.available_log_files = 0;
            summary.latest_log_file = None;
        }
    }
}

fn load_runtime_health_summary(dir: &Path) -> RuntimeHealthSummary {
    let path = runtime_status_path(dir);
    let mut summary = fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str::<RuntimeHealthSummary>(&content).ok())
        .unwrap_or_else(|| default_runtime_health_summary(dir));
    refresh_runtime_health_summary_metadata(&mut summary, dir);
    summary
}

fn persist_runtime_health_summary(dir: &Path, summary: &RuntimeHealthSummary) {
    let path = runtime_status_path(dir);
    match serde_json::to_string_pretty(summary) {
        Ok(content) => {
            if let Err(e) = fs::write(&path, content) {
                eprintln!(
                    "[RUNTIME_STATUS][ERROR] failed to write runtime status file {}: {}",
                    path.to_string_lossy(),
                    e
                );
            }
        }
        Err(e) => {
            eprintln!("[RUNTIME_STATUS][ERROR] failed to serialize runtime status: {}", e);
        }
    }
}

fn update_runtime_health_summary<F>(mutator: F)
where
    F: FnOnce(&mut RuntimeHealthSummary),
{
    let dir = match app_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("[RUNTIME_STATUS][ERROR] failed to resolve app dir: {}", e);
            return;
        }
    };

    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!("[RUNTIME_STATUS][ERROR] failed to create app dir: {}", e);
        return;
    }

    let mut summary = load_runtime_health_summary(&dir);
    mutator(&mut summary);
    refresh_runtime_health_summary_metadata(&mut summary, &dir);
    persist_runtime_health_summary(&dir, &summary);
}

fn cleanup_old_runtime_logs(dir: &Path) {
    let mut runtime_logs: Vec<PathBuf> = match list_runtime_log_files(dir) {
        Ok(logs) => logs,
        Err(e) => {
            eprintln!("[RUNTIME_LOG][ERROR] failed to read log directory: {}", e);
            return;
        }
    };

    if runtime_logs.len() <= RUNTIME_LOG_RETENTION_DAYS {
        return;
    }

    runtime_logs.sort_by(|a, b| {
        let a_name = a.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        let b_name = b.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        b_name.cmp(a_name)
    });

    for old_log in runtime_logs.into_iter().skip(RUNTIME_LOG_RETENTION_DAYS) {
        if let Err(e) = fs::remove_file(&old_log) {
            eprintln!(
                "[RUNTIME_LOG][ERROR] failed to remove old log file {}: {}",
                old_log.to_string_lossy(),
                e
            );
        }
    }
}

#[tauri::command]
fn read_runtime_health_summary() -> Result<serde_json::Value, String> {
    let dir = app_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(serde_json::to_value(load_runtime_health_summary(&dir)).map_err(|e| e.to_string())?)
}

#[tauri::command]
fn export_runtime_logs_zip() -> Result<serde_json::Value, String> {
    let dir = app_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let log_files = list_runtime_log_files(&dir).map_err(|e| e.to_string())?;
    let status_file = runtime_status_path(&dir);

    if log_files.is_empty() && !status_file.exists() {
        return Err("No runtime logs available to export".into());
    }

    let (date, _) = runtime_log_date_and_timestamp();
    let default_name = format!("easycli-runtime-logs-{}.zip", date);
    let archive_path = FileDialog::new()
        .set_title("Export runtime logs")
        .add_filter("ZIP archive", &["zip"])
        .set_file_name(&default_name)
        .save_file()
        .ok_or_else(|| "User cancelled log export".to_string())?;

    let archive_file = fs::File::create(&archive_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipWriter::new(archive_file);
    let options = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);

    let mut exported_files = 0usize;

    for log_file in log_files {
        let name = log_file
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "Invalid runtime log filename".to_string())?;
        archive
            .start_file(name, options)
            .map_err(|e| e.to_string())?;
        let content = fs::read(&log_file).map_err(|e| e.to_string())?;
        archive.write_all(&content).map_err(|e| e.to_string())?;
        exported_files += 1;
    }

    if status_file.exists() {
        let name = status_file
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "Invalid runtime status filename".to_string())?;
        archive
            .start_file(name, options)
            .map_err(|e| e.to_string())?;
        let content = fs::read(&status_file).map_err(|e| e.to_string())?;
        archive.write_all(&content).map_err(|e| e.to_string())?;
        exported_files += 1;
    }

    archive.finish().map_err(|e| e.to_string())?;

    Ok(json!({
        "success": true,
        "path": archive_path.to_string_lossy().to_string(),
        "exportedFiles": exported_files
    }))
}

#[tauri::command]
fn open_runtime_log_directory() -> Result<serde_json::Value, String> {
    let dir = app_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg(dir.as_os_str())
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .spawn()
            .map_err(|e| e.to_string())?;
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(dir.as_os_str())
            .spawn()
            .map_err(|e| e.to_string())?;
    }

    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(dir.as_os_str())
            .spawn()
            .map_err(|e| e.to_string())?;
    }

    Ok(json!({
        "success": true,
        "path": dir.to_string_lossy().to_string()
    }))
}

fn is_process_running(pid: u32) -> bool {
    #[cfg(target_os = "windows")]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid)])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .output();
        if let Ok(output) = output {
            return String::from_utf8_lossy(&output.stdout).contains(&pid.to_string());
        }
        false
    }

    #[cfg(not(target_os = "windows"))]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
}

// Kill any process using the specified port
fn kill_process_on_port(port: u16) -> Result<(), String> {
    println!("[PORT_CLEANUP] Checking port {}", port);

    #[cfg(target_os = "macos")]
    {
        // Use lsof to find the process
        let output = std::process::Command::new("lsof")
            .args(["-ti", &format!(":{}", port)])
            .output()
            .map_err(|e| format!("Failed to run lsof: {}", e))?;

        if output.status.success() {
            let pids = String::from_utf8_lossy(&output.stdout);
            for pid_str in pids.lines() {
                if let Ok(pid) = pid_str.trim().parse::<i32>() {
                    println!("[PORT_CLEANUP] Killing PID {} on port {}", pid, port);
                    if let Err(e) = std::process::Command::new("kill")
                        .args(["-9", &pid.to_string()])
                        .output()
                    {
                        eprintln!("[PORT_CLEANUP] Failed to run kill for PID {}: {}", pid, e);
                    }
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        // Use fuser to kill the process
        let output = std::process::Command::new("fuser")
            .args(["-k", "-9", &format!("{}/tcp", port)])
            .output()
            .map_err(|e| format!("Failed to run fuser: {}", e))?;

        if output.status.success() {
            println!("[PORT_CLEANUP] Killed processes on port {}", port);
        }
    }

    #[cfg(target_os = "windows")]
    {
        // Use netstat to find the PID, then taskkill to kill it
        let output = std::process::Command::new("netstat")
            .args(["-ano"])
            .output()
            .map_err(|e| format!("Failed to run netstat: {}", e))?;

        if output.status.success() {
            let netstat_output = String::from_utf8_lossy(&output.stdout);
            let port_pattern = format!(":{}", port);

            for line in netstat_output.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() > 2
                    && parts[1].ends_with(&port_pattern)
                    && line.contains("LISTENING")
                {
                    // Extract PID from the last column
                    if let Some(pid_str) = parts.last() {
                        if let Ok(pid) = pid_str.parse::<i32>() {
                            println!("[PORT_CLEANUP] Killing PID {} on port {}", pid, port);
                            if let Err(e) = std::process::Command::new("taskkill")
                                .args(["/F", "/PID", &pid.to_string()])
                                .output()
                            {
                                eprintln!(
                                    "[PORT_CLEANUP] Failed to run taskkill for PID {}: {}",
                                    pid, e
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

#[tauri::command]
fn start_cliproxyapi(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let config = app_dir().map_err(|e| e.to_string())?.join("config.yaml");
    let persisted_password = if config.exists() {
        read_config_yaml_value()
            .ok()
            .and_then(|conf| read_local_management_secret_from_config(&conf))
    } else {
        None
    };

    // Check if already running by testing PID
    if let Some(pid) = *PROCESS_PID.lock() {
        if is_process_running(pid) {
            cache_local_management_secret(persisted_password.as_deref());
            return Ok(
                json!({"success": true, "message": "already running", "password": persisted_password}),
            );
        }
    }

    let info = current_local_info().map_err(|e| e.to_string())?;
    let (_ver, path) = info.ok_or("Version file does not exist")?;
    let exec = find_executable(&path).ok_or("Executable file does not exist")?;
    if !config.exists() {
        return Err("Configuration file does not exist".into());
    }

    // Read config, clean port, and prepare for update
    let content = fs::read_to_string(&config).map_err(|e| e.to_string())?;
    let mut conf: serde_yaml::Value = serde_yaml::from_str(&content).map_err(|e| e.to_string())?;

    let port = conf.get("port").and_then(|v| v.as_u64()).unwrap_or(8317) as u16;

    // Automatic port cleanup
    if let Err(e) = kill_process_on_port(port) {
        eprintln!("[PORT_CLEANUP] Warning: {}", e);
    }

    // Reuse persisted secret key, generate only if missing
    let password = ensure_local_management_secret(&mut conf)?;

    // Store the password for keep-alive authentication
    cache_local_management_secret(Some(password.as_str()));

    // Persist config so a newly generated secret key is saved for future restarts
    write_config_yaml_value(&conf)?;

    println!("[CLIProxyAPI][START] exec: {}", exec.to_string_lossy());
    println!(
        "[CLIProxyAPI][START] args: -config {} --password {}",
        config.to_string_lossy(),
        password
    );
    let mut cmd = std::process::Command::new(&exec);
    cmd.args([
        "-config",
        config.to_string_lossy().as_ref(),
        "--password",
        &password,
    ]);
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(0x08000000 | 0x00000008); // CREATE_NO_WINDOW | DETACHED_PROCESS
    }
    #[cfg(not(target_os = "windows"))]
    {
        // On Unix systems, use process_group to detach from parent
        unsafe {
            cmd.pre_exec(|| {
                // Create new process group (session leader)
                libc::setsid();
                Ok(())
            });
        }
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd.spawn().map_err(|e| {
        eprintln!("[CLIProxyAPI][ERROR] failed to start process: {}", e);
        append_runtime_log("ERROR", &format!("CLIProxyAPI start failed: {}", e));
        e.to_string()
    })?;
    // Don't track the child process - let it run independently
    // Store PID for restart functionality
    let pid = child.id();
    *PROCESS_PID.lock() = Some(pid);
    println!("[CLIProxyAPI][START] Detached process with PID: {}", pid);
    append_runtime_log("INFO", &format!("CLIProxyAPI started with PID {} on port {}", pid, port));
    // Drop child handle to fully detach
    std::mem::drop(child);
    // Don't monitor - process is fully detached
    // Create tray icon when local process starts
    let _ = create_tray(&app);

    // Start keep-alive mechanism for Local mode
    let config = read_config_yaml().unwrap_or(json!({}));
    let port = config.get("port").and_then(|v| v.as_u64()).unwrap_or(8317) as u16;
    let _ = start_keep_alive(app.clone(), port);

    Ok(json!({"success": true, "password": password}))
}

#[tauri::command]
fn restart_cliproxyapi(app: tauri::AppHandle) -> Result<(), String> {
    // Kill existing detached process if PID is stored
    if let Some(pid) = *PROCESS_PID.lock() {
        println!("[CLIProxyAPI][RESTART] Killing old process PID: {}", pid);
        append_runtime_log("WARN", &format!("Restart requested for CLIProxyAPI PID {}", pid));
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            let _ = std::process::Command::new("taskkill")
                .args(["/F", "/PID", &pid.to_string()])
                .creation_flags(0x08000000) // CREATE_NO_WINDOW
                .output();
        }
        #[cfg(not(target_os = "windows"))]
        {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    // Start new using current version
    let info = current_local_info().map_err(|e| e.to_string())?;
    let (ver, path) = info.ok_or("Version file does not exist")?;
    let exec = find_executable(&path).ok_or("Executable file does not exist")?;
    let config = app_dir().map_err(|e| e.to_string())?.join("config.yaml");
    if !config.exists() {
        return Err("Configuration file does not exist".into());
    }

    // Read config, clean port, and prepare for update
    let content = fs::read_to_string(&config).map_err(|e| e.to_string())?;
    let mut conf: serde_yaml::Value = serde_yaml::from_str(&content).map_err(|e| e.to_string())?;

    let port = conf.get("port").and_then(|v| v.as_u64()).unwrap_or(8317) as u16;

    // Automatic port cleanup
    if let Err(e) = kill_process_on_port(port) {
        eprintln!("[PORT_CLEANUP] Warning: {}", e);
    }

    // Reuse persisted secret key, generate only if missing
    let password = ensure_local_management_secret(&mut conf)?;

    // Store the password for keep-alive authentication
    *CLI_PROXY_PASSWORD.lock() = Some(password.clone());

    // Persist config so a newly generated secret key is saved for future restarts
    write_config_yaml_value(&conf)?;

    println!("[CLIProxyAPI][RESTART] exec: {}", exec.to_string_lossy());
    println!(
        "[CLIProxyAPI][RESTART] args: -config {} --password {}",
        config.to_string_lossy(),
        password
    );
    let mut cmd = std::process::Command::new(&exec);
    cmd.args([
        "-config",
        config.to_string_lossy().as_ref(),
        "--password",
        &password,
    ]);
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(0x08000000 | 0x00000008); // CREATE_NO_WINDOW | DETACHED_PROCESS
    }
    #[cfg(not(target_os = "windows"))]
    {
        // On Unix systems, use process_group to detach from parent
        unsafe {
            cmd.pre_exec(|| {
                // Create new process group (session leader)
                libc::setsid();
                Ok(())
            });
        }
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = cmd.spawn().map_err(|e| {
        eprintln!("[CLIProxyAPI][ERROR] failed to restart process: {}", e);
        append_runtime_log("ERROR", &format!("CLIProxyAPI restart failed: {}", e));
        e.to_string()
    })?;
    // Store PID and drop child handle to fully detach
    let pid = child.id();
    *PROCESS_PID.lock() = Some(pid);
    println!("[CLIProxyAPI][RESTART] Detached process with PID: {}", pid);
    append_runtime_log(
        "INFO",
        &format!("CLIProxyAPI restarted with PID {} on port {}", pid, port),
    );
    std::mem::drop(child);

    // Start keep-alive mechanism for Local mode
    let config = read_config_yaml().unwrap_or(json!({}));
    let port = config.get("port").and_then(|v| v.as_u64()).unwrap_or(8317) as u16;
    let _ = start_keep_alive(app.clone(), port);

    if let Some(w) = app.get_webview_window("main") {
        let _ = w.emit("cliproxyapi-restarted", json!({"version": ver}));
    }
    Ok(())
}

fn create_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    use tauri::{
        menu::{MenuBuilder, MenuItemBuilder},
        tray::TrayIconBuilder,
    };
    let mut guard = TRAY_ICON.lock();
    if guard.is_some() {
        return Ok(());
    }

    let open_settings = MenuItemBuilder::with_id("open_settings", "打开设置").build(app)?;
    let restart_service = MenuItemBuilder::with_id("restart_service", "重启服务").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "退出").build(app)?;
    let menu = MenuBuilder::new(app)
        .items(&[&open_settings, &restart_service, &quit])
        .build()?;
    let mut builder = TrayIconBuilder::new()
        .menu(&menu)
        .show_menu_on_left_click(true)
        .tooltip("EasyCLI")
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open_settings" => {
                let _ = open_settings_window(app.clone());
            }
            "restart_service" => {
                if let Err(error) = restart_cliproxyapi(app.clone()) {
                    eprintln!("[CLIProxyAPI][RESTART] Tray restart failed: {}", error);
                }
            }
            "quit" => {
                // Just exit app - CLIProxyAPI continues running
                let _ = TRAY_ICON.lock().take();
                println!("[CLIProxyAPI][INFO] Quitting app - CLIProxyAPI continues in background");
                let _ = app.exit(0);
            }
            _ => {}
        });
    // Platform-specific tray icon
    #[cfg(target_os = "linux")]
    {
        const ICON_PNG: &[u8] = include_bytes!("../../images/icon.png");
        if let Ok(img) = image::load_from_memory(ICON_PNG) {
            let rgba = img.into_rgba8();
            let (w, h) = rgba.dimensions();
            let icon = tauri::image::Image::new_owned(rgba.into_raw(), w, h);
            builder = builder.icon(icon);
        }
    }
    #[cfg(target_os = "windows")]
    {
        const ICON_ICO: &[u8] = include_bytes!("../../images/icon.ico");
        if let Ok(dir) = ico::IconDir::read(Cursor::new(ICON_ICO)) {
            if let Some(entry) = dir.entries().iter().max_by_key(|e| e.width()) {
                if let Ok(img) = entry.decode() {
                    let w = img.width();
                    let h = img.height();
                    let rgba = img.rgba_data().to_vec();
                    let icon = tauri::image::Image::new_owned(rgba, w, h);
                    builder = builder.icon(icon);
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        // Try decode ICNS and convert to PNG buffer; fallback to PNG if needed.
        const ICON_ICNS: &[u8] = include_bytes!("../../images/icon.icns");
        let mut set = false;
        if let Ok(fam) = icns::IconFamily::read(Cursor::new(ICON_ICNS)) {
            use icns::IconType;
            let prefs = [
                IconType::RGBA32_512x512,
                IconType::RGBA32_256x256,
                IconType::RGBA32_128x128,
                IconType::RGBA32_64x64,
                IconType::RGBA32_32x32,
                IconType::RGBA32_16x16,
            ];
            for ty in prefs.iter() {
                if let Ok(icon_img) = fam.get_icon_with_type(*ty) {
                    let mut png_buf: Vec<u8> = Vec::new();
                    if icon_img.write_png(&mut png_buf).is_ok() {
                        if let Ok(img) = image::load_from_memory(&png_buf) {
                            let rgba = img.into_rgba8();
                            let (w, h) = rgba.dimensions();
                            let icon = tauri::image::Image::new_owned(rgba.into_raw(), w, h);
                            builder = builder.icon(icon);
                            set = true;
                            break;
                        }
                    }
                }
            }
        }
        if !set {
            const ICON_PNG: &[u8] = include_bytes!("../../images/icon.png");
            if let Ok(img) = image::load_from_memory(ICON_PNG) {
                let rgba = img.into_rgba8();
                let (w, h) = rgba.dimensions();
                let icon = tauri::image::Image::new_owned(rgba.into_raw(), w, h);
                builder = builder.icon(icon);
            }
        }
    }
    let tray = builder.build(app)?;
    *guard = Some(tray);
    Ok(())
}

fn callback_path_for(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "/anthropic/callback",
        "codex" => "/codex/callback",
        "google" => "/google/callback",
        "iflow" => "/iflow/callback",
        "antigravity" => "/antigravity/callback",
        _ => "/callback",
    }
}

fn build_redirect_url(
    mode: &str,
    provider: &str,
    base_url: Option<String>,
    local_port: Option<u16>,
    query: &str,
) -> String {
    let cb = callback_path_for(provider);
    let base = if mode == "local" {
        let port = local_port.unwrap_or(8317);
        format!("http://127.0.0.1:{}{}", port, cb)
    } else {
        let bu = base_url.unwrap_or_else(|| "http://127.0.0.1:8317".to_string());
        // ensure single slash
        if bu.ends_with('/') {
            format!("{}{}", bu, cb.trim_start_matches('/'))
        } else {
            format!("{}/{}", bu, cb.trim_start_matches('/'))
        }
    };
    if query.is_empty() {
        base
    } else {
        format!("{}?{}", base, query)
    }
}

fn run_callback_server(
    stop: Arc<AtomicBool>,
    listen_port: u16,
    mode: String,
    provider: String,
    base_url: Option<String>,
    local_port: Option<u16>,
) {
    let addr = format!("127.0.0.1:{}", listen_port);
    let listener = match std::net::TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[CALLBACK] failed to bind {}: {}", addr, e);
            return;
        }
    };
    if let Err(e) = listener.set_nonblocking(false) {
        eprintln!("[CALLBACK] set_nonblocking failed: {}", e);
    }
    println!("[CALLBACK] listening on {} for provider {}", addr, provider);
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // read request line
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut req_line = String::new();
                if reader.read_line(&mut req_line).is_ok() {
                    let pathq = req_line.split_whitespace().nth(1).unwrap_or("/");
                    let query = pathq.splitn(2, '?').nth(1).unwrap_or("");
                    let loc =
                        build_redirect_url(&mode, &provider, base_url.clone(), local_port, query);
                    let resp = format!(
                        "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        loc
                    );
                    let _ = stream.write_all(resp.as_bytes());
                }
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            Err(e) => {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                eprintln!("[CALLBACK] accept error: {}", e);
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
    println!("[CALLBACK] server on {} stopped", addr);
}

#[tauri::command]
fn start_callback_server(
    provider: String,
    listen_port: u16,
    mode: String,
    base_url: Option<String>,
    local_port: Option<u16>,
) -> Result<serde_json::Value, String> {
    let mut map = CALLBACK_SERVERS.lock();
    if let Some((flag, handle)) = map.remove(&listen_port) {
        flag.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", listen_port));
        let _ = handle.join();
    }
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    let handle = thread::spawn(move || {
        run_callback_server(
            stop_clone,
            listen_port,
            mode,
            provider,
            base_url,
            local_port,
        )
    });
    map.insert(listen_port, (stop, handle));
    Ok(json!({"success": true}))
}

#[tauri::command]
fn stop_callback_server(listen_port: u16) -> Result<serde_json::Value, String> {
    // Take the server handle out of the map so it won't be stopped twice
    let opt = CALLBACK_SERVERS.lock().remove(&listen_port);
    if let Some((flag, handle)) = opt {
        // Signal stop and nudge the listener, then detach-join in background
        flag.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", listen_port));
        std::thread::spawn(move || {
            let _ = handle.join();
        });
        Ok(json!({"success": true}))
    } else {
        Ok(json!({"success": false, "error": "not running"}))
    }
}

#[tauri::command]
fn open_settings_window(app: tauri::AppHandle) -> Result<(), String> {
    // If settings window already exists (predefined in config), just show and focus it
    if let Some(win) = app.get_webview_window("settings") {
        let _ = win.show();
        let _ = win.set_focus();
        // Ensure Dock icon is visible while settings is open (macOS only)
        #[cfg(target_os = "macos")]
        {
            let _ = app.show();
            let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
            let _ = app.set_dock_visibility(true);
        }
        // Also close login window shortly after (do not exit app)
        let app_cloned = app.clone();
        tauri::async_runtime::spawn(async move {
            sleep(Duration::from_millis(50)).await;
            if let Some(main) = app_cloned.get_webview_window("main") {
                let _ = main.hide();
            }
        });
        return Ok(());
    }

    // Otherwise create it and show
    let url = WebviewUrl::App("settings.html".into());
    let win = WebviewWindowBuilder::new(&app, "settings", url)
        .title("EasyCLI Control Panel")
        .inner_size(930.0, 600.0)
        .resizable(false)
        .build()
        .map_err(|e| e.to_string())?;
    let _ = win.show();
    let _ = win.set_focus();
    // Ensure Dock icon is visible while settings is open (macOS only)
    #[cfg(target_os = "macos")]
    {
        let _ = app.show();
        let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
        let _ = app.set_dock_visibility(true);
    }
    // Close the main (login) window shortly after to avoid hanging the invoke (do not exit app)
    let app_cloned = app.clone();
    tauri::async_runtime::spawn(async move {
        sleep(Duration::from_millis(50)).await;
        if let Some(main) = app_cloned.get_webview_window("main") {
            let _ = main.hide();
        }
    });
    Ok(())
}

#[tauri::command]
fn open_login_window(app: tauri::AppHandle) -> Result<(), String> {
    // If login window already exists (predefined in config), show and focus it
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.set_focus();
        // Close settings window shortly after to ensure clean state
        let app_cloned = app.clone();
        tauri::async_runtime::spawn(async move {
            sleep(Duration::from_millis(50)).await;
            if let Some(settings) = app_cloned.get_webview_window("settings") {
                let _ = settings.close();
            }
        });
        return Ok(());
    }

    // Otherwise create the login window and close settings
    let url = WebviewUrl::App("login.html".into());
    let win = WebviewWindowBuilder::new(&app, "main", url)
        .title("EasyCLI")
        .inner_size(530.0, 380.0)
        .resizable(false)
        .build()
        .map_err(|e| e.to_string())?;
    let _ = win.show();
    let _ = win.set_focus();

    let app_cloned = app.clone();
    tauri::async_runtime::spawn(async move {
        sleep(Duration::from_millis(50)).await;
        if let Some(settings) = app_cloned.get_webview_window("settings") {
            let _ = settings.close();
        }
    });
    Ok(())
}

// Auto-start functionality

#[cfg(target_os = "macos")]
fn get_launch_agent_path() -> Result<PathBuf, AppError> {
    let home = home_dir()?;
    Ok(home.join("Library/LaunchAgents/com.easycli.app.plist"))
}

#[cfg(target_os = "linux")]
fn get_autostart_path() -> Result<PathBuf, AppError> {
    let home = home_dir()?;
    Ok(home.join(".config/autostart/easycli.desktop"))
}

#[cfg(target_os = "macos")]
fn get_app_path() -> Result<String, AppError> {
    // Get the path to the current executable
    let exe = std::env::current_exe()?;

    // Navigate up from the executable to find the .app bundle
    // Typical path: /Applications/EasyCLI.app/Contents/MacOS/EasyCLI
    let mut path = exe.as_path();

    // Go up directories until we find the .app bundle
    while let Some(parent) = path.parent() {
        if let Some(file_name) = parent.file_name() {
            if file_name.to_string_lossy().ends_with(".app") {
                return Ok(parent.to_string_lossy().to_string());
            }
        }
        path = parent;
    }

    // Fallback: return the executable path
    Ok(exe.to_string_lossy().to_string())
}

#[cfg(target_os = "linux")]
fn get_app_path() -> Result<String, AppError> {
    let exe = std::env::current_exe()?;
    Ok(exe.to_string_lossy().to_string())
}

#[cfg(target_os = "windows")]
fn get_app_path() -> Result<String, AppError> {
    let exe = std::env::current_exe()?;
    Ok(exe.to_string_lossy().to_string())
}

#[tauri::command]
fn check_auto_start_enabled() -> Result<serde_json::Value, String> {
    #[cfg(target_os = "macos")]
    {
        let plist_path = get_launch_agent_path().map_err(|e| e.to_string())?;
        Ok(json!({"enabled": plist_path.exists()}))
    }

    #[cfg(target_os = "linux")]
    {
        let desktop_path = get_autostart_path().map_err(|e| e.to_string())?;
        Ok(json!({"enabled": desktop_path.exists()}))
    }

    #[cfg(target_os = "windows")]
    {
        use winreg::enums::*;
        use winreg::RegKey;

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let run_key = hkcu.open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run");

        match run_key {
            Ok(key) => match key.get_value::<String, _>("EasyCLI") {
                Ok(_) => Ok(json!({"enabled": true})),
                Err(_) => Ok(json!({"enabled": false})),
            },
            Err(_) => Ok(json!({"enabled": false})),
        }
    }
}

#[tauri::command]
fn enable_auto_start() -> Result<serde_json::Value, String> {
    #[cfg(target_os = "macos")]
    {
        let plist_path = get_launch_agent_path().map_err(|e| e.to_string())?;
        let app_path = get_app_path().map_err(|e| e.to_string())?;

        // Create LaunchAgents directory if it doesn't exist
        if let Some(parent) = plist_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }

        // Create plist content
        let plist_content = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.easycli.app</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/bin/open</string>
        <string>{}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
</dict>
</plist>"#,
            app_path
        );

        fs::write(&plist_path, plist_content).map_err(|e| e.to_string())?;
        Ok(json!({"success": true}))
    }

    #[cfg(target_os = "linux")]
    {
        let desktop_path = get_autostart_path().map_err(|e| e.to_string())?;
        let app_path = get_app_path().map_err(|e| e.to_string())?;

        // Create autostart directory if it doesn't exist
        if let Some(parent) = desktop_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }

        // Create .desktop file content
        let desktop_content = format!(
            r#"[Desktop Entry]
Type=Application
Name=EasyCLI
Exec={}
Hidden=false
NoDisplay=false
X-GNOME-Autostart-enabled=true
Comment=EasyCLI - API Proxy Management Tool"#,
            app_path
        );

        fs::write(&desktop_path, desktop_content).map_err(|e| e.to_string())?;
        Ok(json!({"success": true}))
    }

    #[cfg(target_os = "windows")]
    {
        use winreg::enums::*;
        use winreg::RegKey;

        let app_path = get_app_path().map_err(|e| e.to_string())?;
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let run_key = hkcu
            .open_subkey_with_flags(
                "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                KEY_WRITE,
            )
            .map_err(|e| e.to_string())?;

        run_key
            .set_value("EasyCLI", &app_path)
            .map_err(|e| e.to_string())?;
        Ok(json!({"success": true}))
    }
}

#[tauri::command]
fn disable_auto_start() -> Result<serde_json::Value, String> {
    #[cfg(target_os = "macos")]
    {
        let plist_path = get_launch_agent_path().map_err(|e| e.to_string())?;
        if plist_path.exists() {
            fs::remove_file(&plist_path).map_err(|e| e.to_string())?;
        }
        Ok(json!({"success": true}))
    }

    #[cfg(target_os = "linux")]
    {
        let desktop_path = get_autostart_path().map_err(|e| e.to_string())?;
        if desktop_path.exists() {
            fs::remove_file(&desktop_path).map_err(|e| e.to_string())?;
        }
        Ok(json!({"success": true}))
    }

    #[cfg(target_os = "windows")]
    {
        use winreg::enums::*;
        use winreg::RegKey;

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let run_key = hkcu.open_subkey_with_flags(
            "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
            KEY_WRITE,
        );

        if let Ok(key) = run_key {
            let _ = key.delete_value("EasyCLI");
        }
        Ok(json!({"success": true}))
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let has_tray = TRAY_ICON.lock().is_some();
                if has_tray {
                    api.prevent_close();
                    let _ = window.hide();
                    if window.label() == "settings" {
                        #[cfg(target_os = "macos")]
                        {
                            let _ = window
                                .app_handle()
                                .set_activation_policy(tauri::ActivationPolicy::Accessory);
                            let _ = window.app_handle().set_dock_visibility(false);
                        }
                    }
                    println!(
                        "[CLIProxyAPI][INFO] {} window hidden - app remains in tray",
                        window.label()
                    );
                    return;
                }
                // No tray icon yet (e.g., app closed before starting CLIProxyAPI) - allow default shutdown.
                println!(
                    "[CLIProxyAPI][INFO] {} window closed before tray initialization - exiting app",
                    window.label()
                );
            }
        })
        // Handle application exit request to prevent exit when tray is present
        .invoke_handler(tauri::generate_handler![
            check_version_and_download,
            download_cliproxyapi,
            check_secret_key,
            update_secret_key,
            read_config_yaml,
            update_config_yaml,
            read_local_auth_files,
            upload_local_auth_files,
            delete_local_auth_files,
            download_local_auth_files,
            restart_cliproxyapi,
            start_cliproxyapi,
            open_runtime_log_directory,
            read_runtime_health_summary,
            export_runtime_logs_zip,
            open_settings_window,
            open_login_window,
            start_callback_server,
            stop_callback_server,
            save_files_to_directory,
            start_keep_alive,
            stop_keep_alive,
            check_auto_start_enabled,
            enable_auto_start,
            disable_auto_start
        ])
        .build(tauri::generate_context!())
        .unwrap()
        .run(|_app_handle, event| {
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
                // Only prevent exit if tray icon exists (app is in "running in background" state)
                if TRAY_ICON.lock().is_some() {
                    // Log exit attempt for debugging
                    println!(
                        "[CLIProxyAPI][INFO] Exit requested (code={:?}) - preventing, app remains in tray",
                        code
                    );
                    api.prevent_exit();
                } else {
                    // No tray icon, allow normal exit (cleanup will happen)
                    println!(
                        "[CLIProxyAPI][INFO] Exit requested (code={:?}) - allowing exit, no tray icon",
                        code
                    );
                }
            }
        });
}

#[derive(Deserialize)]
struct SaveFile {
    name: String,
    content: String,
}

#[tauri::command]
fn save_files_to_directory(files: Vec<SaveFile>) -> Result<serde_json::Value, String> {
    if files.is_empty() {
        return Ok(json!({"success": false, "error": "No files to save"}));
    }
    // Show a system directory picker to choose the destination folder
    let folder = FileDialog::new()
        .set_title("Choose save directory")
        .pick_folder()
        .ok_or_else(|| "User cancelled directory selection".to_string())?;

    // Write each file into the chosen directory
    let mut success: usize = 0;
    let mut error_count: usize = 0;
    let mut errors: Vec<String> = Vec::new();
    for f in files {
        let path = folder.join(&f.name);
        match fs::write(&path, f.content.as_bytes()) {
            Ok(_) => success += 1,
            Err(e) => {
                error_count += 1;
                errors.push(format!("{}: {}", f.name, e));
            }
        }
    }

    Ok(json!({
        "success": success > 0,
        "successCount": success,
        "errorCount": error_count,
        "errors": if errors.is_empty() { serde_json::Value::Null } else { json!(errors) }
    }))
}

// Keep-alive mechanism functions

fn run_keep_alive_loop(
    stop: Arc<AtomicBool>,
    app: tauri::AppHandle,
    port: u16,
    initial_password: String,
) {
    println!("[KEEP-ALIVE] Starting keep-alive loop for port {}", port);
    append_runtime_log("INFO", &format!("Keep-alive loop started on port {}", port));

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            println!("[KEEP-ALIVE] Failed to create tokio runtime: {}", e);
            append_runtime_log("ERROR", &format!("Keep-alive runtime creation failed: {}", e));
            return;
        }
    };

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            println!("[KEEP-ALIVE] Failed to build HTTP client: {}", e);
            append_runtime_log("ERROR", &format!("Keep-alive HTTP client creation failed: {}", e));
            return;
        }
    };

    let mut consecutive_failures = 0u32;

    while !stop.load(Ordering::SeqCst) {
        let keep_alive_url = format!("http://127.0.0.1:{}/keep-alive", port);
        let password = CLI_PROXY_PASSWORD
            .lock()
            .clone()
            .unwrap_or_else(|| initial_password.clone());
        let password_for_log: String = password.chars().take(8).collect();
        let result = rt.block_on(async {
            println!("[KEEP-ALIVE] Sending request to: {}", keep_alive_url);
            println!("[KEEP-ALIVE] Using password: {}...", password_for_log);
            client
                .get(&keep_alive_url)
                .header("Authorization", format!("Bearer {}", &password))
                .header("Content-Type", "application/json")
                .send()
                .await
        });

        let request_ok = match result {
            Ok(response) if response.status().is_success() => {
                if consecutive_failures > 0 {
                    append_runtime_log(
                        "INFO",
                        &format!(
                            "Keep-alive recovered on port {} after {} consecutive failures",
                            port, consecutive_failures
                        ),
                    );
                }
                println!("[KEEP-ALIVE] Request successful");
                true
            }
            Ok(response) => {
                println!("[KEEP-ALIVE] Request failed: {}", response.status());
                append_runtime_log(
                    "WARN",
                    &format!(
                        "Keep-alive request failed on port {} with status {}",
                        port,
                        response.status()
                    ),
                );
                false
            }
            Err(e) => {
                println!("[KEEP-ALIVE] Request error: {}", e);
                append_runtime_log(
                    "WARN",
                    &format!("Keep-alive request error on port {}: {}", port, e),
                );
                false
            }
        };

        if request_ok {
            consecutive_failures = 0;
        } else {
            consecutive_failures += 1;
            println!(
                "[KEEP-ALIVE] Health check failed {} consecutive time(s)",
                consecutive_failures
            );
            append_runtime_log(
                "WARN",
                &format!(
                    "Keep-alive health check failed {} consecutive time(s) on port {}",
                    consecutive_failures, port
                ),
            );

            if consecutive_failures >= 3
                && !KEEP_ALIVE_RESTARTING.swap(true, Ordering::SeqCst)
            {
                println!("[KEEP-ALIVE] Health check failed repeatedly, restarting CLIProxyAPI");
                let (restart_date, restart_ts) = runtime_log_date_and_timestamp();
                append_runtime_log(
                    "ERROR",
                    &format!(
                        "Keep-alive exceeded failure threshold on port {}, triggering auto-restart",
                        port
                    ),
                );
                update_runtime_health_summary(|summary| {
                    summary.last_auto_restart_at = Some(restart_ts.clone());
                    summary.last_auto_restart_reason =
                        Some("keep_alive_failure_threshold".to_string());
                    summary.last_auto_restart_result = Some("in_progress".to_string());
                    summary.last_auto_restart_details = Some(format!(
                        "Keep-alive failed {} consecutive times on port {}",
                        consecutive_failures, port
                    ));
                    summary.last_auto_restart_port = Some(port);
                    summary.last_auto_restart_trigger_failures = Some(consecutive_failures);
                    summary.latest_log_file =
                        Some(format!("easycli-runtime-{}.log", restart_date.clone()));
                });
                match restart_cliproxyapi(app.clone()) {
                    Ok(_) => {
                        consecutive_failures = 0;
                        append_runtime_log(
                            "INFO",
                            &format!("CLIProxyAPI auto-restart succeeded on port {}", port),
                        );
                        update_runtime_health_summary(|summary| {
                            summary.last_auto_restart_result = Some("succeeded".to_string());
                            summary.last_auto_restart_details = Some(format!(
                                "CLIProxyAPI auto-restart succeeded on port {}",
                                port
                            ));
                        });
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.emit(
                                "cliproxyapi-auto-restarted",
                                json!({"reason": "keep-alive-health-check-failed"}),
                            );
                        }
                    }
                    Err(e) => {
                        println!("[KEEP-ALIVE] Auto-restart failed: {}", e);
                        append_runtime_log(
                            "ERROR",
                            &format!("CLIProxyAPI auto-restart failed on port {}: {}", port, e),
                        );
                        update_runtime_health_summary(|summary| {
                            summary.last_auto_restart_result = Some("failed".to_string());
                            summary.last_auto_restart_details = Some(format!(
                                "CLIProxyAPI auto-restart failed on port {}: {}",
                                port, e
                            ));
                        });
                    }
                }
                KEEP_ALIVE_RESTARTING.store(false, Ordering::SeqCst);
            }
        }

        for _ in 0..50 {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    println!("[KEEP-ALIVE] Keep-alive loop stopped");
    append_runtime_log("INFO", &format!("Keep-alive loop stopped on port {}", port));
}

#[tauri::command]
fn start_keep_alive(app: tauri::AppHandle, port: u16) -> Result<serde_json::Value, String> {
    // Stop existing keep-alive if running
    stop_keep_alive_internal();

    // Get the stored password, fall back to persisted config if needed
    let password = if let Some(password) = CLI_PROXY_PASSWORD.lock().clone() {
        password
    } else {
        let conf = read_config_yaml_value()?;
        let password = read_local_management_secret_from_config(&conf)
            .ok_or("No CLIProxyAPI password available")?;
        *CLI_PROXY_PASSWORD.lock() = Some(password.clone());
        password
    };

    let stop = Arc::new(AtomicBool::new(false));
    let handle = {
        let stop_clone = stop.clone();
        let app_clone = app.clone();
        thread::spawn(move || {
            run_keep_alive_loop(stop_clone, app_clone, port, password);
        })
    };

    *KEEP_ALIVE_HANDLE.lock() = Some((stop, handle));

    println!("[KEEP-ALIVE] Started keep-alive for port {}", port);
    append_runtime_log("INFO", &format!("Keep-alive enabled on port {}", port));
    Ok(json!({"success": true}))
}

#[tauri::command]
fn stop_keep_alive() -> Result<serde_json::Value, String> {
    stop_keep_alive_internal();
    Ok(json!({"success": true}))
}

fn stop_keep_alive_internal() {
    if let Some((stop, handle)) = KEEP_ALIVE_HANDLE.lock().take() {
        println!("[KEEP-ALIVE] Stopping keep-alive mechanism");
        append_runtime_log("INFO", "Keep-alive stop requested");
        stop.store(true, Ordering::SeqCst);

        // Detach the handle to avoid blocking
        std::thread::spawn(move || {
            let _ = handle.join();
        });
    }
}
