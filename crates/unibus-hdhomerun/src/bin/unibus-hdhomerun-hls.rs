//! One explicitly owned GPU projection; channel choices come from the tuner.
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{ConnectInfo, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    process::{Child, Command},
    sync::Mutex,
};
use unibus_hdhomerun::{is_protected, valid_channel};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    listen: SocketAddr,
    allowed_clients: Vec<std::net::IpAddr>,
    catalog_url: String,
    stream_base: String,
    public_base: String,
    directory: PathBuf,
}
struct App {
    config: Config,
    client: reqwest::Client,
    session: Mutex<Session>,
}
#[derive(Default)]
struct Session {
    child: Option<Child>,
    generation: u64,
    channel: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tune {
    channel: String,
}
#[derive(Serialize)]
struct Choice {
    channel: String,
    name: String,
}
type Reply = Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)>;
fn fail(error: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_GATEWAY,
        Json(serde_json::json!({"error":error.to_string()})),
    )
}
fn admit(app: &App, peer: SocketAddr) -> Result<()> {
    ensure!(
        app.config.allowed_clients.contains(&peer.ip()),
        "Client not authorized"
    );
    Ok(())
}
async fn lineup(app: &App) -> Result<Vec<Choice>> {
    let response = app
        .client
        .get(&app.config.catalog_url)
        .send()
        .await?
        .error_for_status()?;
    ensure!(
        response.content_length().is_none_or(|n| n <= 1024 * 1024),
        "Catalog too large"
    );
    let mut bytes = Vec::new();
    let mut response = response;
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= 1024 * 1024,
            "Catalog too large"
        );
        bytes.extend(chunk);
    }
    let channels: Vec<serde_json::Map<String, serde_json::Value>> = serde_json::from_slice(&bytes)?;
    ensure!(channels.len() <= 1024, "Too many channels");
    // Reuse the owner's protection policy and channel validation, never accept a URL from a caller.
    channels
        .iter()
        .filter(|c| !is_protected(c))
        .map(|c| {
            let channel = c["GuideNumber"].as_str().context("Missing channel")?;
            ensure!(valid_channel(channel), "Invalid catalog channel");
            Ok(Choice {
                channel: channel.into(),
                name: c["GuideName"].as_str().context("Missing name")?.into(),
            })
        })
        .collect()
}
async fn channels(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Reply {
    admit(&app, peer).map_err(|e| {
        (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error":e.to_string()})),
        )
    })?;
    Ok(Json(
        serde_json::json!({"schema":"unibus.hdhomerun.hls.v1","channels":lineup(&app).await.map_err(fail)?}),
    ))
}
async fn tune(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(request): Json<Tune>,
) -> Reply {
    admit(&app, peer).map_err(|e| {
        (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error":e.to_string()})),
        )
    })?;
    ensure_choice(&request.channel).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":e.to_string()})),
        )
    })?;
    let choices = lineup(&app).await.map_err(fail)?;
    let choice = choices
        .iter()
        .find(|c| c.channel == request.channel)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error":"Channel unavailable"})),
            )
        })?;
    let mut session = app.session.try_lock().map_err(|_| {
        (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error":"Channel change already in progress"})),
        )
    })?;
    let alive = session
        .child
        .as_mut()
        .is_some_and(|c| matches!(c.try_wait(), Ok(None)));
    if session.channel.as_deref() != Some(&request.channel) || !alive {
        let previous = session.child.is_some().then(|| {
            app.config
                .directory
                .join(format!("g{}", session.generation))
        });
        if let Some(mut child) = session.child.take() {
            if child.try_wait().map_err(fail)?.is_none() {
                child.kill().await.map_err(fail)?;
            }
            child.wait().await.map_err(fail)?;
        }
        if let Some(old) = previous {
            tokio::fs::remove_dir_all(old).await.map_err(fail)?;
        }
        session.channel = None;
        session.generation += 1;
        let directory = app
            .config
            .directory
            .join(format!("g{}", session.generation));
        tokio::fs::create_dir(&directory).await.map_err(fail)?;
        let input = format!("{}/auto/v{}", app.config.stream_base, request.channel);
        let segments = directory.join(format!("segment-g{}-%09d.ts", session.generation));
        let playlist = directory.join("live.m3u8");
        let mut child = Command::new("/usr/bin/ffmpeg")
            .kill_on_drop(true)
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "warning",
                "-rw_timeout",
                "15000000",
                "-threads",
                "2",
                "-i",
                &input,
                "-map",
                "0:v:0",
                "-map",
                "0:a:0",
                "-vf",
                "scale=1280:720,fps=30,format=nv12",
                "-c:v",
                "h264_nvenc",
                "-preset",
                "p1",
                "-tune",
                "ull",
                "-profile:v",
                "main",
                "-b:v",
                "5M",
                "-maxrate",
                "5M",
                "-bufsize",
                "1M",
                "-g",
                "30",
                "-bf",
                "0",
                "-c:a",
                "aac",
                "-b:a",
                "160k",
                "-ac",
                "2",
                "-ar",
                "48000",
                "-f",
                "hls",
                "-hls_time",
                "1",
                "-hls_list_size",
                "8",
                "-hls_delete_threshold",
                "12",
                "-hls_flags",
                "delete_segments+independent_segments+temp_file",
                "-hls_segment_filename",
            ])
            .arg(segments)
            .arg(&playlist)
            .spawn()
            .map_err(fail)?;
        let ready = async {
            loop {
                ensure!(
                    child.try_wait()?.is_none(),
                    "Encoder exited before playback was ready"
                );
                if tokio::fs::read(&playlist)
                    .await
                    .is_ok_and(|b| b.windows(7).any(|w| w == b"#EXTINF"))
                {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        if let Err(error) = tokio::time::timeout(Duration::from_secs(18), ready)
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r)
        {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = tokio::fs::remove_dir_all(&directory).await;
            return Err(fail(error));
        }
        session.child = Some(child);
        session.channel = Some(request.channel.clone());
        // Keep a stable media endpoint across workspaces, but never reuse segment
        // names across channels: old decoder caches must not cross generations.
        let pending = app
            .config
            .directory
            .join(format!("current-g{}", session.generation));
        std::os::unix::fs::symlink(&directory, &pending).map_err(fail)?;
        std::fs::rename(&pending, app.config.directory.join("current")).map_err(fail)?;
    }
    Ok(Json(
        serde_json::json!({"schema":"unibus.hdhomerun.hls.v1","channel":choice.channel,"name":choice.name,
        "url":format!("{}/current/live.m3u8?generation={}",app.config.public_base,session.generation)}),
    ))
}
fn ensure_choice(channel: &str) -> Result<()> {
    ensure!(valid_channel(channel), "Invalid channel");
    Ok(())
}

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: unibus-hdhomerun-hls CONFIG.json")?;
    let config: Config = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(
        !config.allowed_clients.is_empty() && config.listen.ip().is_ipv4(),
        "Explicit client allowlist required"
    );
    ensure!(
        config.directory.is_absolute() && config.directory.components().count() > 3,
        "Dedicated absolute projection directory required"
    );
    for endpoint in [
        &config.catalog_url,
        &config.stream_base,
        &config.public_base,
    ] {
        let url = reqwest::Url::parse(endpoint)?;
        ensure!(
            url.scheme() == "http"
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none(),
            "Invalid private endpoint"
        );
        ensure!(
            url.host_str()
                .and_then(|h| h.parse::<std::net::Ipv4Addr>().ok())
                .is_some_and(|ip| ip.is_private()),
            "Private literal endpoint required"
        );
    }
    let listen = config.listen;
    let generation = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as u64;
    let app = Arc::new(App {
        config,
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        session: Mutex::new(Session {
            generation,
            ..Default::default()
        }),
    });
    let router = Router::new()
        .route("/v1/channels", get(channels))
        .route("/v1/tune", post(tune))
        .with_state(app);
    axum::serve(
        tokio::net::TcpListener::bind(listen).await?,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tuning_is_a_channel_not_a_url_or_shell() {
        for bad in ["http://evil/", "4.1;reboot", "../4.1", ""] {
            assert!(ensure_choice(bad).is_err());
        }
        assert!(ensure_choice("4.1").is_ok());
        assert!(serde_json::from_str::<Tune>(r#"{"channel":"4.1","url":"http://evil"}"#).is_err());
    }
    #[tokio::test]
    async fn catalog_uses_owner_choices_and_filters_protected_channels() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let owner = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            socket.read(&mut request).await.unwrap();
            let body = r#"[{"GuideNumber":"4.1","GuideName":"KOMO"},{"GuideNumber":"9.9","GuideName":"Protected","DRM":1}]"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let app = App {
            config: Config {
                listen: address,
                allowed_clients: vec!["192.168.2.97".parse().unwrap()],
                catalog_url: format!("http://{address}/lineup.json"),
                stream_base: "http://192.168.2.74:5004".into(),
                public_base: "http://192.168.2.67:18089/tuner".into(),
                directory: PathBuf::from("/var/lib/private-tuner"),
            },
            client: reqwest::Client::new(),
            session: Mutex::new(Session::default()),
        };
        let channels = lineup(&app).await.unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].channel, "4.1");
        assert!(admit(&app, "192.168.2.97:1234".parse().unwrap()).is_ok());
        assert!(admit(&app, "192.168.2.98:1234".parse().unwrap()).is_err());
        owner.await.unwrap();
    }
}
