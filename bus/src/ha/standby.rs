//! Standby hub follower, bundle sync, HTTP 503 gate, and promotion handoff (section 13.5).

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message as WsMsg;

use crate::client::{self, Opts};
use crate::hub::{self, HubConfig};
use crate::msg::Message;
use crate::proto::ConnectParams;
use crate::store::Store;

use super::{HaRoleState, HubHaRole};

/// Configuration for `sommelier standby`.
#[derive(Clone, Debug)]
pub struct StandbyConfig {
    pub of_url: String,
    pub token: String,
    pub data_dir: PathBuf,
    pub listen_addr: String,
    pub initial_epoch: u64,
    pub bundle_source_dir: Option<PathBuf>,
    pub bundle_sync_every: Duration,
    pub verbose: bool,
}

impl StandbyConfig {
    pub fn new(
        of_url: impl Into<String>,
        token: impl Into<String>,
        data_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            of_url: of_url.into(),
            token: token.into(),
            data_dir: data_dir.into(),
            listen_addr: "127.0.0.1:7301".into(),
            initial_epoch: 1,
            bundle_source_dir: None,
            bundle_sync_every: Duration::from_secs(300),
            verbose: false,
        }
    }
}

/// Statistics from syncing non-log state (`tokens.json`, `policy/`, `briefs/`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleStats {
    pub tokens_copied: bool,
    pub policy_files: usize,
    pub brief_files: usize,
}

/// Handle to a running standby server (for drills and tests).
pub struct StandbyHandle {
    pub addr: SocketAddr,
    pub ws_url: String,
    pub http_url: String,
    pub last_id: Arc<AtomicU64>,
    pub epoch: Arc<AtomicU64>,
    stop_tx: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<Result<(), String>>>,
}

impl StandbyHandle {
    pub fn last_id(&self) -> u64 {
        self.last_id.load(Ordering::Relaxed)
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
    }

    pub fn stop(&self) {
        let _ = self.stop_tx.send(true);
    }

    pub async fn join(mut self) -> Result<(), String> {
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        task.await
            .map_err(|e| format!("standby task panicked: {e}"))?
    }
}

impl Drop for StandbyHandle {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Reads the highest message id from `data_dir/bus.jsonl` (0 if missing or empty).
pub fn read_last_log_id(data_dir: &Path) -> Result<u64, String> {
    let log_path = data_dir.join("bus.jsonl");
    let f = match File::open(&log_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("cannot open {}: {e}", log_path.display())),
    };
    let mut last_id = 0u64;
    for line in BufReader::new(f).lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(m) = serde_json::from_str::<Message>(&line) {
            last_id = last_id.max(m.id);
        }
    }
    Ok(last_id)
}

/// Copies the non-log bundle (`tokens.json` with salted hashes only, `policy/`, `briefs/`)
/// from `src_dir` into `dst_dir` using atomic temp-write + rename.
pub fn sync_bundle(src_dir: &Path, dst_dir: &Path) -> Result<BundleStats, String> {
    fs::create_dir_all(dst_dir).map_err(|e| e.to_string())?;
    let mut stats = BundleStats::default();

    let src_tokens = src_dir.join("tokens.json");
    if src_tokens.exists() {
        let dst_tokens = dst_dir.join("tokens.json");
        copy_atomic(&src_tokens, &dst_tokens)?;
        stats.tokens_copied = true;
    }

    let src_policy = src_dir.join("policy");
    if src_policy.is_dir() {
        stats.policy_files = copy_dir_recursive(&src_policy, &dst_dir.join("policy"))?;
    }

    let src_briefs = src_dir.join("briefs");
    if src_briefs.is_dir() {
        stats.brief_files = copy_dir_recursive(&src_briefs, &dst_dir.join("briefs"))?;
    }

    Ok(stats)
}

fn copy_atomic(src: &Path, dst: &Path) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = dst.with_extension("tmp");
    fs::copy(src, &tmp).map_err(|e| format!("copy {}: {e}", src.display()))?;
    fs::rename(&tmp, dst).map_err(|e| format!("rename {}: {e}", dst.display()))?;
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<usize, String> {
    fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    let mut count = 0usize;
    for entry in fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let name = entry.file_name();
        if name.to_string_lossy().ends_with(".lock") || name.to_string_lossy().ends_with(".tmp") {
            continue;
        }
        let target = dst.join(name);
        if path.is_dir() {
            count += copy_dir_recursive(&path, &target)?;
        } else if path.is_file() {
            copy_atomic(&path, &target)?;
            count += 1;
        }
    }
    Ok(count)
}

/// Starts the standby server on `listener` (or binds `cfg.listen_addr` if `None`).
///
/// While in standby role, it:
/// - Follows the primary's log (`bus.jsonl`) byte-for-byte and applies to `state.db`.
/// - Periodically syncs non-log bundle if `bundle_source_dir` is set.
/// - Refuses all HTTP/WS clients on `listener` with HTTP 503 `"not primary, epoch N"`.
/// - When `ha_state.json` transitions to `role = "primary"`, closes the follower and
///   starts serving as primary `Hub` on the same `listener`.
pub async fn run_standby(
    cfg: StandbyConfig,
    listener: Option<TcpListener>,
) -> Result<StandbyHandle, String> {
    fs::create_dir_all(&cfg.data_dir)
        .map_err(|e| format!("cannot create {}: {e}", cfg.data_dir.display()))?;

    let initial_state = HaRoleState {
        role: HubHaRole::Standby,
        epoch: cfg.initial_epoch,
        holder: None,
        primary_url: Some(cfg.of_url.clone()),
        updated_at: Some(crate::time::now_ms() / 1000),
    };
    initial_state.save(&cfg.data_dir)?;

    if let Some(src) = &cfg.bundle_source_dir {
        sync_bundle(src, &cfg.data_dir)?;
    }

    let listener = match listener {
        Some(l) => l,
        None => TcpListener::bind(&cfg.listen_addr)
            .await
            .map_err(|e| format!("cannot bind {}: {e}", cfg.listen_addr))?,
    };
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let last_id = Arc::new(AtomicU64::new(read_last_log_id(&cfg.data_dir)?));
    let epoch = Arc::new(AtomicU64::new(cfg.initial_epoch));
    let (stop_tx, stop_rx) = watch::channel(false);

    let last_id_task = last_id.clone();
    let epoch_task = epoch.clone();

    let task = tokio::spawn(async move {
        standby_loop(cfg, listener, last_id_task, epoch_task, stop_rx).await
    });

    Ok(StandbyHandle {
        addr,
        ws_url: format!("ws://{addr}"),
        http_url: format!("http://{addr}"),
        last_id,
        epoch,
        stop_tx,
        task: Some(task),
    })
}

async fn standby_loop(
    cfg: StandbyConfig,
    listener: TcpListener,
    last_id: Arc<AtomicU64>,
    epoch: Arc<AtomicU64>,
    mut stop_rx: watch::Receiver<bool>,
) -> Result<(), String> {
    let mut store = Store::open(&cfg.data_dir)?;
    let log_path = cfg.data_dir.join("bus.jsonl");
    if log_path.exists() {
        store.catch_up_from_log(&log_path)?;
    }

    let (promote_tx, promote_rx) = watch::channel(false);
    let data_dir_follow = cfg.data_dir.clone();
    let of_url = cfg.of_url.clone();
    let token = cfg.token.clone();
    let last_id_follow = last_id.clone();
    let mut stop_follow = stop_rx.clone();
    let mut promote_follow = promote_rx.clone();

    let follower = tokio::spawn(async move {
        follow_log_loop(
            &of_url,
            &token,
            &data_dir_follow,
            store,
            last_id_follow,
            &mut stop_follow,
            &mut promote_follow,
        )
        .await;
    });

    let mut poll_state = tokio::time::interval(Duration::from_millis(100));
    let mut bundle_tick = tokio::time::interval(cfg.bundle_sync_every);
    bundle_tick.tick().await;

    let mut promoted_to_primary = false;
    loop {
        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    break;
                }
            }
            accept_res = listener.accept() => {
                if let Ok((stream, _)) = accept_res {
                    let ep = epoch.load(Ordering::Relaxed);
                    tokio::spawn(async move {
                        let _ = write_503(stream, ep).await;
                    });
                }
            }
            _ = poll_state.tick() => {
                if let Some(st) = HaRoleState::load(&cfg.data_dir) {
                    epoch.store(st.epoch, Ordering::Relaxed);
                    if st.role == HubHaRole::Primary {
                        promoted_to_primary = true;
                        let _ = promote_tx.send(true);
                        break;
                    }
                }
            }
            _ = bundle_tick.tick() => {
                if let Some(src) = &cfg.bundle_source_dir {
                    let _ = sync_bundle(src, &cfg.data_dir);
                }
            }
        }
    }

    let _ = follower.await;

    if promoted_to_primary {
        let mut hub_cfg = HubConfig::new(&cfg.data_dir);
        hub_cfg.verbose = cfg.verbose;
        hub::run(hub_cfg, listener)
            .await
            .map_err(|e| format!("promoted hub error: {e}"))?;
    }

    Ok(())
}

async fn write_503(mut stream: TcpStream, epoch: u64) -> std::io::Result<()> {
    let body = format!("not primary, epoch {epoch}\n");
    let resp = format!(
        "HTTP/1.1 503 Service Unavailable\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.shutdown().await
}

async fn follow_log_loop(
    of_url: &str,
    token: &str,
    data_dir: &Path,
    mut store: Store,
    last_id: Arc<AtomicU64>,
    stop_rx: &mut watch::Receiver<bool>,
    promote_rx: &mut watch::Receiver<bool>,
) {
    let opts = Opts {
        url: of_url.to_owned(),
        token: token.to_owned(),
    };
    let log_path = data_dir.join("bus.jsonl");
    let mut backoff = Duration::from_millis(100);

    loop {
        if *stop_rx.borrow() || *promote_rx.borrow() {
            return;
        }
        let cur_last = last_id.load(Ordering::Relaxed);
        let params = ConnectParams {
            since: Some(cur_last + 1),
            live: true,
            ..Default::default()
        };
        match client::connect(&opts, &params).await {
            Ok(ws) => {
                backoff = Duration::from_millis(100);
                let (mut sink, mut stream) = ws.split();
                let _ = sink
                    .send(WsMsg::text(
                        json!({"op": "whoami", "rid": "standby"}).to_string(),
                    ))
                    .await;
                let mut ping = tokio::time::interval(client::PING_EVERY);
                ping.tick().await;
                loop {
                    tokio::select! {
                        _ = stop_rx.changed() => {
                            if *stop_rx.borrow() {
                                let _ = sink.close().await;
                                return;
                            }
                        }
                        _ = promote_rx.changed() => {
                            if *promote_rx.borrow() {
                                let _ = sink.close().await;
                                return;
                            }
                        }
                        frame = stream.next() => match frame {
                            Some(Ok(WsMsg::Text(t))) => {
                                let line = t.as_str();
                                let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
                                if v.get("reply").is_some() {
                                    continue;
                                }
                                let Ok(msg) = serde_json::from_value::<Message>(v) else { continue };
                                let cur = last_id.load(Ordering::Relaxed);
                                if msg.id <= cur {
                                    continue;
                                }
                                if append_exact_line(&log_path, line).is_ok() {
                                    let _ = store.apply(&msg);
                                    last_id.store(msg.id, Ordering::Relaxed);
                                }
                            }
                            Some(Ok(WsMsg::Close(_))) | None | Some(Err(_)) => break,
                            Some(Ok(_)) => {}
                        },
                        _ = ping.tick() => {
                            if sink.send(WsMsg::Ping(Default::default())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
            Err(e) if e.fatal => {
                return;
            }
            Err(_) => {}
        }

        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    return;
                }
            }
            _ = promote_rx.changed() => {
                if *promote_rx.borrow() {
                    return;
                }
            }
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

fn append_exact_line(log_path: &Path, line: &str) -> std::io::Result<()> {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    writeln!(f, "{line}")?;
    f.flush()
}
