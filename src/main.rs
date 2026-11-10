// ldk-server-gui: a web dashboard and (optionally) control panel for an LDK
// Server node.
//
// The backend polls the node's gRPC API on an interval, subscribes to its
// event stream for immediate refreshes, and pushes updates to the browser
// over Server-Sent Events. With `--enable-actions`, it also exposes mutating
// node commands (see actions.rs), guarded by a random per-start action token.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use ldk_server_client::client::LdkServerClient;
use ldk_server_client::config::{
	get_default_config_path, load_config, read_tls_certificate, resolve_base_url,
	resolve_cert_path, resolve_macaroon, Config,
};
use ldk_server_client::ldk_server_grpc::api::{
	GetBalancesRequest, GetNodeInfoRequest, ListChannelsRequest, ListPaymentsRequest,
	ListPeersRequest,
};
use serde_json::{json, Value};
use tokio::sync::{broadcast, Notify, RwLock};

mod actions;

const INDEX_HTML: &str = include_str!("../static/index.html");
const KYOTO_HTML: &str = include_str!("../static/kyoto.html");
const RESOURCES_HTML: &str = include_str!("../static/resources.html");
const CONTROL_HTML: &str = include_str!("../static/control.html");
const COMMON_JS: &str = include_str!("../static/common.js");
const EVENT_BUFFER_CAP: usize = 200;

// What the dashboard itself was built from, as `version|branch|commit|dirty`. Written by build.rs.
const GUI_BUILD_INFO: &str = env!("BUILD_INFO_GUI");

// Put at the end of every page, in place of `<!--VERSIONS-->`: the version, branch and commit of
// the dashboard and of the node it talks to (the node, ldk-node and kyoto), from `/api/versions`.
const VERSIONS_FOOTER: &str = r##"<footer id="build-versions" style="margin-top:24px;padding-top:10px;border-top:1px solid var(--border,#232b3a);color:var(--muted,#8a93a5);font-size:12px;line-height:1.9"></footer>
<script>
(function () {
  var box = document.getElementById("build-versions");
  var escapes = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" };
  function esc(s) { return String(s).replace(/[&<>"]/g, function (c) { return escapes[c]; }); }
  function show(data) {
    var parts = (data.components || []).map(function (c) {
      var dirty = c.dirty === "true";
      var title = dirty ? "built with uncommitted changes" : "built from a clean checkout";
      return '<span class="mono" title="' + title + '">' + esc(c.component) + " " + esc(c.version) +
        " · " + esc(c.branch) + " @ " + esc(c.commit) +
        (dirty ? ' <span style="color:var(--accent2,#ffd166)">+uncommitted</span>' : "") + "</span>";
    });
    if (data.error) parts.push("<span>node build info: " + esc(data.error) + "</span>");
    box.innerHTML = parts.join(" &nbsp;|&nbsp; ");
  }
  function load() {
    fetch("/api/versions").then(function (r) { return r.json(); }).then(show).catch(function () {});
  }
  load();
  setInterval(load, 60000);
})();
</script>"##;

const DEFAULT_DIR: &str = if cfg!(target_os = "macos") {
	"~/Library/Application Support/ldk-server"
} else if cfg!(target_os = "windows") {
	"%APPDATA%\\ldk-server"
} else {
	"~/.ldk-server"
};

#[derive(Parser, Debug)]
#[command(
	name = "ldk-server-gui",
	version,
	about = "Read-only web dashboard for an LDK Server node"
)]
struct Args {
	#[arg(short, long, help = "Base URL of the ldk-server gRPC endpoint (default: from config file)")]
	base_url: Option<String>,

	#[arg(short, long, help = format!("Hex macaroon (default: admin.macaroon from {DEFAULT_DIR})"))]
	macaroon: Option<String>,

	#[arg(short, long, help = "Path to the server's TLS certificate (PEM)")]
	tls_cert: Option<PathBuf>,

	#[arg(short, long, help = "Path to the ldk-server config file")]
	config: Option<PathBuf>,

	#[arg(short, long, default_value = "127.0.0.1:3540", help = "Address the dashboard listens on")]
	listen: SocketAddr,

	#[arg(long, default_value_t = 5, help = "Snapshot poll interval in seconds")]
	poll_interval_secs: u64,

	#[arg(
		long,
		default_value = "http://127.0.0.1:9332",
		help = "Base URL of kyoto's stats endpoint (the node's CBF chain source)"
	)]
	kyoto_url: String,

	#[arg(long, requires = "metrics_password", help = "Basic Auth user of the node's /metrics, if set")]
	metrics_user: Option<String>,

	#[arg(long, requires = "metrics_user", help = "Basic Auth password of the node's /metrics, if set")]
	metrics_password: Option<String>,

	#[arg(
		long,
		help = "Enable the control panel (mutating node commands over HTTP, guarded by a random per-start action token)"
	)]
	enable_actions: bool,

	#[arg(
		long,
		requires = "enable_actions",
		help = "Read the action token from this file instead of generating a new one at each start"
	)]
	action_token_file: Option<PathBuf>,

	#[arg(
		long,
		help = "The node's log file. The Kyoto page lists the chain source outages the node wrote there"
	)]
	node_log: Option<PathBuf>,

	#[arg(
		long,
		help = "Keep the node's memory and CPU history in this file, so the graphs survive a restart"
	)]
	usage_history_file: Option<PathBuf>,
}

struct AppState {
	snapshot: RwLock<Arc<str>>,
	activity: RwLock<VecDeque<Arc<str>>>,
	tx: broadcast::Sender<Arc<str>>,
	kyoto_stats_url: String,
	kyoto_history_url: String,
	http: reqwest::Client,
	// For the node's `/metrics`, where the CBF chain source activity is reported.
	client: LdkServerClient,
	metrics_auth: Option<(String, String)>,
	// The node's log file, if known: where the chain source events are read from.
	node_log: Option<PathBuf>,
	// Readings of the node's process, oldest first, for the resource graphs. Saved to
	// `usage_file`, if there is one, so they survive a restart.
	usage: std::sync::Mutex<VecDeque<UsageSample>>,
	usage_file: Option<PathBuf>,
	// Set when --enable-actions is passed; required as `X-Action-Token` on action POSTs.
	action_token: Option<Arc<str>>,
	// Notified by actions to trigger an immediate snapshot refresh.
	refresh: Arc<Notify>,
	// The pages with the Control link in their navigation bar, when actions are enabled.
	index_html: Arc<str>,
	kyoto_html: Arc<str>,
	resources_html: Arc<str>,
}

impl AppState {
	// Records a control-panel action in the activity feed (audit trail) and
	// broadcasts it to connected browsers.
	async fn log_action(&self, summary: String) {
		let msg = Arc::<str>::from(
			json!({ "kind": "action", "received_at": now_unix(), "data": { "summary": summary } })
				.to_string(),
		);
		{
			let mut activity = self.activity.write().await;
			if activity.len() >= EVENT_BUFFER_CAP {
				activity.pop_back();
			}
			activity.push_front(Arc::clone(&msg));
		}
		let _ = self.tx.send(wrap("event", &msg));
	}
}

// The shortest action token that is read from a file. The control panel can move funds.
const MIN_ACTION_TOKEN_LEN: usize = 24;

// Reads the action token from a file, so it stays the same across restarts and does not show up in
// the process list or the log. Whitespace around it, such as the final newline, is ignored.
fn read_action_token(path: &std::path::Path) -> Result<Arc<str>, String> {
	let contents = std::fs::read_to_string(path)
		.map_err(|e| format!("failed to read the action token file '{}': {e}", path.display()))?;
	let token = contents.trim();
	if token.len() < MIN_ACTION_TOKEN_LEN {
		return Err(format!(
			"the action token in '{}' is too short, use at least {MIN_ACTION_TOKEN_LEN} characters (for example `openssl rand -hex 32`)",
			path.display()
		));
	}
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		if let Ok(metadata) = std::fs::metadata(path) {
			if metadata.permissions().mode() & 0o077 != 0 {
				eprintln!(
					"warning: '{}' can be read by other users, run `chmod 600` on it",
					path.display()
				);
			}
		}
	}
	Ok(Arc::from(token))
}

// Hex-encodes 16 fresh random bytes for the action token.
fn generate_action_token() -> Result<Arc<str>, String> {
	let mut bytes = [0u8; 16];
	getrandom::getrandom(&mut bytes).map_err(|e| format!("failed to generate action token: {e}"))?;
	let mut hex = String::with_capacity(32);
	for b in bytes {
		hex.push_str(&format!("{b:02x}"));
	}
	Ok(Arc::from(hex))
}

#[tokio::main]
async fn main() {
	let args = Args::parse();
	let client = build_client(&args).unwrap_or_else(|e| {
		eprintln!("error: {e}");
		std::process::exit(1);
	});

	let action_token = if args.enable_actions {
		let token = match &args.action_token_file {
			Some(path) => read_action_token(path),
			None => generate_action_token(),
		}
		.unwrap_or_else(|e| {
			eprintln!("error: {e}");
			std::process::exit(1);
		});
		println!("actions enabled — control panel: http://{}/control", args.listen);
		match &args.action_token_file {
			// A token from a file is a secret that is kept: it is never printed.
			Some(path) => println!("action token read from {}", path.display()),
			None => println!("action token (enter it on the control page): {token}"),
		}
		Some(token)
	} else {
		None
	};

	let refresh = Arc::new(Notify::new());
	let actions_enabled = action_token.is_some();
	let index_html = with_control_link(INDEX_HTML, actions_enabled);
	let kyoto_html = with_control_link(KYOTO_HTML, actions_enabled);
	let resources_html = with_control_link(RESOURCES_HTML, actions_enabled);

	// What was recorded before this start, so the graphs do not begin empty.
	let usage_history = match &args.usage_history_file {
		Some(path) => {
			let history = load_usage_history(path);
			// Rewriting it now drops what is older than the window.
			if let Err(e) = write_usage_file(path, &history) {
				eprintln!("warning: cannot write the usage history file '{}': {e}", path.display());
			}
			history
		},
		None => VecDeque::new(),
	};

	let (tx, _rx) = broadcast::channel::<Arc<str>>(256);
	let state = Arc::new(AppState {
		snapshot: RwLock::new(Arc::from("{\"ok\":false,\"error\":\"no data yet\"}")),
		activity: RwLock::new(VecDeque::new()),
		tx,
		kyoto_stats_url: format!("{}/api/stats", args.kyoto_url.trim_end_matches('/')),
		kyoto_history_url: format!("{}/api/peers/history", args.kyoto_url.trim_end_matches('/')),
		http: reqwest::Client::builder()
			.timeout(Duration::from_secs(3))
			.build()
			.expect("failed to build the HTTP client"),
		client: client.clone(),
		metrics_auth: args.metrics_user.clone().zip(args.metrics_password.clone()),
		node_log: args.node_log.clone(),
		usage: std::sync::Mutex::new(usage_history),
		usage_file: args.usage_history_file.clone(),
		action_token,
		refresh: Arc::clone(&refresh),
		index_html,
		kyoto_html,
		resources_html,
	});

	// Poller: refresh the snapshot on an interval, or immediately when an
	// event arrives.
	{
		let state = Arc::clone(&state);
		let client = client.clone();
		let refresh = Arc::clone(&refresh);
		let interval = Duration::from_secs(args.poll_interval_secs.max(1));
		tokio::spawn(async move {
			loop {
				let snap = collect_snapshot(&client).await;
				let msg = Arc::<str>::from(snap.to_string());
				*state.snapshot.write().await = Arc::clone(&msg);
				let _ = state.tx.send(wrap("snapshot", &msg));

				tokio::select! {
					_ = tokio::time::sleep(interval) => {},
					_ = refresh.notified() => {},
				}
			}
		});
	}

	// Event subscriber: record events in the activity feed and trigger an
	// immediate snapshot refresh.
	{
		let state = Arc::clone(&state);
		let refresh = Arc::clone(&refresh);
		tokio::spawn(async move {
			// Log stream failures only on the connected → disconnected transition,
			// to avoid spamming while the node is (re)starting.
			let mut logged_failure = false;
			loop {
				match client.subscribe_events().await {
					Ok(mut stream) => {
						if logged_failure {
							eprintln!("event stream connected");
							logged_failure = false;
						}
						loop {
							match stream.next_message().await {
								Some(Ok(envelope)) => {
									let msg = event_message(&envelope);
									{
										let mut activity = state.activity.write().await;
										if activity.len() >= EVENT_BUFFER_CAP {
											activity.pop_back();
										}
										activity.push_front(Arc::clone(&msg));
									}
									let _ = state.tx.send(wrap("event", &msg));
									refresh.notify_one();
								},
								Some(Err(e)) => {
									eprintln!("event stream error: {e}");
									break;
								},
								None => break,
							}
						}
					},
					Err(e) => {
						if !logged_failure {
							eprintln!("subscribe_events failed: {e} (retrying every 2s, logged once)");
							logged_failure = true;
						}
					},
				}
				tokio::time::sleep(Duration::from_secs(2)).await;
			}
		});
	}

	// Sampler: reads the node's memory and CPU time from its `/metrics` at a fixed pace, for the
	// resource graphs. A reading that fails is skipped, which shows as a gap in the graph.
	{
		let state = Arc::clone(&state);
		tokio::spawn(async move {
			let mut appended = 0usize;
			loop {
				tokio::time::sleep(USAGE_INTERVAL).await;
				let Ok(text) = fetch_metrics_text(&state).await else { continue };
				let Some(stats) = parse_node_stats(&text) else { continue };
				let (Some(rss), Some(cpu)) = (stats["rss_bytes"].as_f64(), stats["cpu_seconds"].as_f64())
				else {
					continue;
				};
				let sample = UsageSample { t: now_unix(), rss, cpu };
				let mut history = state.usage.lock().expect("lock");
				history.push_back(sample);
				let cutoff = sample.t.saturating_sub(USAGE_WINDOW_SECS);
				while history.front().is_some_and(|oldest| oldest.t < cutoff) {
					history.pop_front();
				}
				if let Some(path) = &state.usage_file {
					appended += 1;
					// Now and then the file is rewritten from memory, so it does not grow forever.
					let written = if appended >= USAGE_FILE_REWRITE_EVERY {
						appended = 0;
						write_usage_file(path, &history)
					} else {
						append_usage_line(path, &sample)
					};
					if let Err(e) = written {
						eprintln!("cannot write the usage history file '{}': {e}", path.display());
					}
				}
			}
		});
	}

	let app = Router::new()
		.route("/", get(index))
		.route("/common.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], COMMON_JS) }))
		.route("/api/versions", get(api_versions))
		.route("/api/snapshot", get(api_snapshot))
		.route("/api/activity", get(api_activity))
		.route("/api/stream", get(api_stream))
		.route("/kyoto", get(kyoto_page))
		.route("/resources", get(resources_page))
		.route("/api/kyoto", get(api_kyoto))
		.route("/api/chain-events", get(api_chain_events))
		.route("/api/usage-history", get(api_usage_history))
		.route("/control", get(actions::control_page))
		.route("/api/actions/check", post(actions::check_token))
		.route("/api/actions/open-channel", post(actions::open_channel))
		.route("/api/actions/close-channel", post(actions::close_channel))
		.route("/api/actions/force-close-channel", post(actions::force_close_channel))
		.route("/api/actions/splice-in", post(actions::splice_in))
		.route("/api/actions/splice-out", post(actions::splice_out))
		.route("/api/actions/update-channel-config", post(actions::update_channel_config))
		.route("/api/actions/bolt11-send", post(actions::bolt11_send))
		.route("/api/actions/bolt12-send", post(actions::bolt12_send))
		.route("/api/actions/spontaneous-send", post(actions::spontaneous_send))
		.route("/api/actions/bolt11-receive", post(actions::bolt11_receive))
		.route("/api/actions/onchain-send", post(actions::onchain_send))
		.route("/api/actions/onchain-receive", post(actions::onchain_receive))
		.route("/api/actions/connect-peer", post(actions::connect_peer))
		.route("/api/actions/disconnect-peer", post(actions::disconnect_peer))
		.route("/api/decode-invoice", post(actions::decode_invoice))
		.route("/api/decode-offer", post(actions::decode_offer))
		.route("/api/stats/tracking-mode", get(actions::tracking_mode))
		.route("/api/stats/forwarded-payments", get(actions::forwarded_payments))
		.route("/api/stats/channel/{channel_id}", get(actions::channel_stats))
		.with_state(state);

	let listener = tokio::net::TcpListener::bind(args.listen)
		.await
		.unwrap_or_else(|e| {
			eprintln!("failed to bind {}: {e}", args.listen);
			std::process::exit(1);
		});
	println!("dashboard listening on http://{}", args.listen);
	axum::serve(listener, app).await.unwrap();
}

fn build_client(args: &Args) -> Result<LdkServerClient, String> {
	let config_path = args.config.clone().or_else(get_default_config_path);
	let config: Option<Config> = match config_path {
		Some(path) if path.is_file() => Some(load_config(&path)?),
		Some(path) if args.config.is_some() => {
			return Err(format!("config file '{}' does not exist", path.display()))
		},
		_ => None,
	};

	let macaroon = resolve_macaroon(args.macaroon.clone(), config.as_ref())?
		.ok_or_else(|| "no macaroon provided or found; use --macaroon".to_string())?;
	let base_url = resolve_base_url(args.base_url.clone(), config.as_ref());
	let cert_path = resolve_cert_path(args.tls_cert.clone(), config.as_ref())
		.ok_or_else(|| "no TLS certificate path found; use --tls-cert".to_string())?;
	let cert_pem = read_tls_certificate(&cert_path)?;

	LdkServerClient::new(base_url, macaroon, &cert_pem)
}

async fn collect_snapshot(client: &LdkServerClient) -> Value {
	let (node, balances, channels, payments, peers) = tokio::join!(
		client.get_node_info(GetNodeInfoRequest {}),
		client.get_balances(GetBalancesRequest {}),
		client.list_channels(ListChannelsRequest {}),
		client.list_payments(ListPaymentsRequest { page_token: None }),
		client.list_peers(ListPeersRequest {}),
	);

	let section = |result: Result<Value, String>| -> Value {
		match result {
			Ok(v) => v,
			Err(e) => json!({ "error": e }),
		}
	};
	fn to_json<T: serde::Serialize>(
		r: Result<T, ldk_server_client::error::LdkServerError>,
	) -> Result<Value, String> {
		use ldk_server_client::error::LdkServerError;
		let r: Result<T, LdkServerError> = r;
		r.map_err(|e| e.to_string())
			.and_then(|v| serde_json::to_value(v).map_err(|e| e.to_string()))
	}

	json!({
		"ok": node.is_ok(),
		"fetched_at": now_unix(),
		"node": section(to_json(node)),
		"balances": section(to_json(balances)),
		"channels": section(to_json(channels)),
		"payments": section(to_json(payments)),
		"peers": section(to_json(peers)),
	})
}

fn event_message(envelope: &ldk_server_client::ldk_server_grpc::events::EventEnvelope) -> Arc<str> {
	use ldk_server_client::ldk_server_grpc::events::event_envelope::Event;
	let kind = match &envelope.event {
		Some(Event::PaymentReceived(_)) => "payment_received",
		Some(Event::PaymentSuccessful(_)) => "payment_successful",
		Some(Event::PaymentFailed(_)) => "payment_failed",
		Some(Event::PaymentForwarded(_)) => "payment_forwarded",
		Some(Event::PaymentClaimable(_)) => "payment_claimable",
		Some(Event::ChannelStateChanged(_)) => "channel_state_changed",
		Some(Event::SpliceNegotiated(_)) => "splice_negotiated",
		Some(Event::SpliceNegotiationFailed(_)) => "splice_negotiation_failed",
		None => "unknown",
	};
	let data = serde_json::to_value(envelope).unwrap_or_else(|_| json!(null));
	Arc::<str>::from(
		json!({ "kind": kind, "received_at": now_unix(), "data": data }).to_string(),
	)
}

fn wrap(kind: &str, payload: &str) -> Arc<str> {
	Arc::<str>::from(format!("{{\"type\":\"{kind}\",\"payload\":{payload}}}"))
}

fn now_unix() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

async fn index(State(state): State<Arc<AppState>>) -> Html<String> {
	Html(state.index_html.to_string())
}

async fn kyoto_page(State(state): State<Arc<AppState>>) -> Html<String> {
	Html(state.kyoto_html.to_string())
}

async fn resources_page(State(state): State<Arc<AppState>>) -> Html<String> {
	Html(state.resources_html.to_string())
}

// A page with the Control link put into its navigation bar, if actions are enabled, and the marker
// removed if they are not.
fn with_control_link(html: &str, actions_enabled: bool) -> Arc<str> {
	let link = if actions_enabled { r#"<a href="/control">Control</a>"# } else { "" };
	with_versions(html).replace("<!--CONTROL_LINK-->", link).into()
}

// A page with the footer that shows the versions put in place of its `<!--VERSIONS-->` marker.
fn with_versions(html: &str) -> String {
	html.replace("<!--VERSIONS-->", VERSIONS_FOOTER)
}

// `version|branch|commit|dirty` as the JSON the footer shows.
fn build_info_json(component: &str, info: &str) -> Value {
	let mut fields = info.split('|');
	json!({
		"component": component,
		"version": fields.next().unwrap_or("unknown"),
		"branch": fields.next().unwrap_or("unknown"),
		"commit": fields.next().unwrap_or("unknown"),
		"dirty": fields.next().unwrap_or("unknown"),
	})
}

// The `ldk_server_build_info{component="...",version="...",...} 1` samples of the node's `/metrics`,
// one object per sample with its labels as fields.
fn parse_build_info(text: &str) -> Vec<Value> {
	text.lines()
		.filter_map(|line| {
			let labels = line.strip_prefix("ldk_server_build_info{")?.split_once("} ")?.0;
			let mut fields = serde_json::Map::new();
			let mut rest = labels;
			while let Some((key, after)) = rest.split_once("=\"") {
				let mut value = String::new();
				let mut chars = after.chars();
				while let Some(c) = chars.next() {
					match c {
						'\\' => match chars.next() {
							Some('n') => value.push('\n'),
							Some(other) => value.push(other),
							None => break,
						},
						'"' => break,
						other => value.push(other),
					}
				}
				fields.insert(key.to_string(), json!(value));
				rest = chars.as_str().trim_start_matches(',');
			}
			fields.contains_key("component").then(|| Value::Object(fields))
		})
		.collect()
}

// What the dashboard and the node it talks to were built from. The node reports its own, its
// ldk-node's and its kyoto's in `/metrics`; if it cannot be reached or reports none, only the
// dashboard's is listed and `error` says why.
async fn api_versions(State(state): State<Arc<AppState>>) -> Json<Value> {
	let mut components = vec![build_info_json("ldk-server-gui", GUI_BUILD_INFO)];
	let mut error = Value::Null;
	match fetch_metrics_text(&state).await {
		Ok(text) => {
			let node = parse_build_info(&text);
			if node.is_empty() {
				error = json!("the node reports none (an older ldk-server, or metrics are off)");
			}
			components.extend(node);
		},
		Err(e) => error = json!(e),
	}
	Json(json!({ "components": components, "error": error }))
}

// Everything the Kyoto page shows, in one response with two independent sections:
// - `kyoto`: kyoto's own `/api/stats` (peers and traffic), fetched here rather than by the browser
//   to avoid cross-origin requests to another port.
// - `cbf`: what the node's compact block filter chain source has done (filters checked and
//   matched, blocks fetched, scripts watched), read from the node's `/metrics`.
// - `node`: how long the node has been running, from the same `/metrics`.
// A failing section is reported in the body, not as an HTTP error, so the page can show it
// while still showing the other one.
async fn api_kyoto(State(state): State<Arc<AppState>>) -> Json<Value> {
	let (kyoto, history, metrics) = tokio::join!(
		fetch_json(&state, &state.kyoto_stats_url),
		fetch_json(&state, &state.kyoto_history_url),
		fetch_metrics_text(&state)
	);
	let cbf = metrics.clone().and_then(|text| {
		parse_cbf_metrics(&text)
			.ok_or_else(|| "the node reports no CBF stats (not using the CBF chain source?)".to_string())
	});
	let node = metrics.and_then(|text| {
		parse_node_stats(&text)
			.ok_or_else(|| "the node reports no uptime (an older ldk-server?)".to_string())
	});
	let section = |result: Result<Value, String>| match result {
		Ok(data) => json!({ "ok": true, "data": data }),
		Err(error) => json!({ "ok": false, "error": error }),
	};
	Json(json!({
		"fetched_at": now_unix(),
		"kyoto": section(kyoto),
		"history": section(history),
		"cbf": section(cbf),
		"node": section(node),
	}))
}

async fn fetch_json(state: &AppState, url: &str) -> Result<Value, String> {
	let response = state.http.get(url).send().await.map_err(|e| format!("{url}: {e}"))?;
	if !response.status().is_success() {
		return Err(format!("{url}: HTTP {}", response.status()));
	}
	let body = response.text().await.map_err(|e| format!("{url}: {e}"))?;
	serde_json::from_str(&body).map_err(|e| format!("{url}: invalid JSON: {e}"))
}

// The node's `/metrics` in the Prometheus text format.
async fn fetch_metrics_text(state: &AppState) -> Result<String, String> {
	match &state.metrics_auth {
		Some((user, password)) => state.client.get_metrics_with_auth(Some(user), Some(password)).await,
		None => state.client.get_metrics().await,
	}
	.map_err(|e| format!("node /metrics: {e} (is [metrics] enabled, and are --metrics-user/--metrics-password needed?)"))
}

// One reading of the node's process: when, its resident memory in bytes, and the CPU time it has
// used since it started, in seconds.
#[derive(Clone, Copy)]
struct UsageSample {
	t: u64,
	rss: f64,
	cpu: f64,
}

const USAGE_INTERVAL: Duration = Duration::from_secs(10);
// How much history is kept: a day, at one reading every 10 seconds.
const USAGE_WINDOW_SECS: u64 = 24 * 3600;
// The file gets one line per reading and is rewritten from memory after this many, so it never
// holds more than about two days of lines.
const USAGE_FILE_REWRITE_EVERY: usize = 8_640;

fn load_usage_history(path: &std::path::Path) -> VecDeque<UsageSample> {
	let cutoff = now_unix().saturating_sub(USAGE_WINDOW_SECS);
	let Ok(text) = std::fs::read_to_string(path) else { return VecDeque::new() };
	text.lines()
		.filter_map(|line| {
			let value: Value = serde_json::from_str(line).ok()?;
			Some(UsageSample {
				t: value["t"].as_u64()?,
				rss: value["rss"].as_f64()?,
				cpu: value["cpu"].as_f64()?,
			})
		})
		.filter(|sample| sample.t >= cutoff)
		.collect()
}

fn usage_line(sample: &UsageSample) -> String {
	json!({ "t": sample.t, "rss": sample.rss, "cpu": sample.cpu }).to_string()
}

// Replaces the file with the readings in memory. It is written next to the file and renamed over it,
// so a crash cannot leave half a file.
fn write_usage_file(
	path: &std::path::Path, history: &VecDeque<UsageSample>,
) -> std::io::Result<()> {
	let mut contents = String::new();
	for sample in history {
		contents.push_str(&usage_line(sample));
		contents.push('\n');
	}
	let temporary = path.with_extension("tmp");
	std::fs::write(&temporary, contents)?;
	std::fs::rename(&temporary, path)
}

fn append_usage_line(path: &std::path::Path, sample: &UsageSample) -> std::io::Result<()> {
	use std::io::Write;
	let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
	writeln!(file, "{}", usage_line(sample))
}

// (time, memory in bytes, CPU use in percent of one core) for each reading. The CPU use needs the
// reading before it, and is `None` where there is none, where the readings are more than three
// intervals apart (a gap, not a measurement), and where the CPU time went down (the node restarted).
fn usage_points(samples: &[UsageSample]) -> Vec<(u64, f64, Option<f64>)> {
	samples
		.iter()
		.enumerate()
		.map(|(i, sample)| {
			let cpu = i.checked_sub(1).and_then(|previous| {
				let previous = samples[previous];
				let seconds = sample.t.saturating_sub(previous.t);
				let measurable = seconds > 0
					&& seconds <= 3 * USAGE_INTERVAL.as_secs()
					&& sample.cpu >= previous.cpu;
				measurable.then(|| (sample.cpu - previous.cpu) / seconds as f64 * 100.0)
			});
			(sample.t, sample.rss, cpu)
		})
		.collect()
}

#[derive(serde::Deserialize)]
struct UsageQuery {
	minutes: Option<u64>,
}

// The data of the graphs: for the last `minutes` (60 by default, at most a day), the node's memory
// in bytes and its CPU use in percent of one core. The CPU use is how much the CPU time grew between
// two readings over the time between them, so it is `null` for the first reading, after a gap, and
// after the node restarted (its CPU time went back to zero).
async fn api_usage_history(
	State(state): State<Arc<AppState>>, Query(query): Query<UsageQuery>,
) -> Json<Value> {
	const MAX_POINTS: usize = 600;
	let minutes = query.minutes.unwrap_or(60).clamp(1, USAGE_WINDOW_SECS / 60);
	let since = now_unix().saturating_sub(minutes * 60);
	let samples: Vec<UsageSample> =
		state.usage.lock().expect("lock").iter().filter(|s| s.t >= since).copied().collect();

	let points = usage_points(&samples);

	// A day is 8,640 readings: average them in groups so the page stays light.
	let group = points.len().div_ceil(MAX_POINTS).max(1);
	let data: Vec<Value> = points
		.chunks(group)
		.map(|chunk| {
			let rss = chunk.iter().map(|p| p.1).sum::<f64>() / chunk.len() as f64;
			let cpus: Vec<f64> = chunk.iter().filter_map(|p| p.2).collect();
			let cpu = (!cpus.is_empty()).then(|| cpus.iter().sum::<f64>() / cpus.len() as f64);
			json!({ "t": chunk[chunk.len() - 1].0, "rss": rss, "cpu": cpu })
		})
		.collect();
	Json(json!({ "ok": true, "minutes": minutes, "data": data }))
}

// The chain source events (started, restarted after an error, gave up, stopped) that the node wrote
// in its log, newest last. The node already logs every one of them at info or error level, with the
// error text, so this only reads them back.
async fn api_chain_events(State(state): State<Arc<AppState>>) -> Json<Value> {
	let Some(path) = state.node_log.clone() else {
		return Json(json!({ "ok": false, "error": "the dashboard was started without --node-log" }));
	};
	match tokio::task::spawn_blocking(move || read_chain_events(&path)).await {
		Ok(Ok(events)) => Json(json!({ "ok": true, "data": events })),
		Ok(Err(error)) => Json(json!({ "ok": false, "error": error })),
		Err(error) => Json(json!({ "ok": false, "error": error.to_string() })),
	}
}

// How much of the end of the log is searched, and how many events are kept.
const LOG_TAIL_BYTES: u64 = 8 * 1024 * 1024;
const CHAIN_EVENTS_KEPT: usize = 50;

fn read_chain_events(path: &std::path::Path) -> Result<Vec<Value>, String> {
	use std::io::{Read, Seek, SeekFrom};
	let mut file = std::fs::File::open(path)
		.map_err(|e| format!("cannot read the node log '{}': {e}", path.display()))?;
	let len = file.metadata().map_err(|e| e.to_string())?.len();
	let start = len.saturating_sub(LOG_TAIL_BYTES);
	file.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
	let mut bytes = Vec::new();
	file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
	let text = String::from_utf8_lossy(&bytes);
	let mut lines = text.lines();
	if start > 0 {
		// The first line is most likely cut in half.
		lines.next();
	}
	let mut events: Vec<Value> = lines.filter_map(parse_chain_event).collect();
	let excess = events.len().saturating_sub(CHAIN_EVENTS_KEPT);
	events.drain(..excess);
	Ok(events)
}

// One log line, `[2026-11-03T03:03:26.390Z INFO  ldk_node::chain::cbf:638] message`, as an event if
// it is about the chain source. The kinds: `started`, `restart` (exited with an error and is tried
// again), `gave_up` (the chain source is down for good), `stopped` (shut down cleanly) and
// `start_failed` (it could not be started, for example the first sync timed out).
fn parse_chain_event(line: &str) -> Option<Value> {
	let (head, message) = line.strip_prefix('[')?.split_once("] ")?;
	let mut parts = head.split_whitespace();
	let (time, level, target) = (parts.next()?, parts.next()?, parts.next()?);
	let kind = if target.starts_with("ldk_node::chain::cbf") {
		if message.starts_with("CBF node exited with error") {
			"restart"
		} else if message.starts_with("CBF node failed") {
			"gave_up"
		} else if message.starts_with("CBF node shut down") {
			"stopped"
		} else if message.starts_with("CBF chain source started") {
			"started"
		} else if message.starts_with("Timed out after")
			|| message.starts_with("Failed to discover the chain tip")
		{
			"start_failed"
		} else {
			return None;
		}
	} else if target.starts_with("ldk_server") && message.starts_with("Failed to build LDK Node") {
		"start_failed"
	} else {
		return None;
	};
	Some(json!({ "time": time, "level": level, "kind": kind, "message": message }))
}

// How long the node has been running and what its process uses, from `/metrics`. The chain source
// runs inside the node's process, so its memory is part of these numbers. `None` if the node does
// not report an uptime; the process numbers are left out when it does not report those.
fn parse_node_stats(text: &str) -> Option<Value> {
	let sample = |name: &str| -> Option<f64> {
		text.lines()
			.filter(|line| !line.starts_with('#'))
			.find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))?
			.trim()
			.parse()
			.ok()
	};
	let mut stats = json!({ "uptime_seconds": sample("ldk_server_uptime_seconds")? as u64 });
	for (key, name) in [
		("rss_bytes", "ldk_server_process_resident_memory_bytes"),
		("peak_rss_bytes", "ldk_server_process_peak_resident_memory_bytes"),
		("threads", "ldk_server_process_threads"),
		("cpu_seconds", "ldk_server_process_cpu_seconds_total"),
	] {
		if let Some(value) = sample(name) {
			stats[key] = json!(value);
		}
	}
	Some(stats)
}

// Picks the `ldk_server_cbf_*_count` samples out of the Prometheus text, keyed by the part
// between the prefix and `_count`. `None` if there are none.
fn parse_cbf_metrics(text: &str) -> Option<Value> {
	let mut values = serde_json::Map::new();
	for line in text.lines().filter(|line| !line.starts_with('#')) {
		let Some((name, value)) = line.split_once(' ') else { continue };
		let Some(key) =
			name.strip_prefix("ldk_server_cbf_").and_then(|rest| rest.strip_suffix("_count"))
		else {
			continue;
		};
		if let Ok(value) = value.trim().parse::<u64>() {
			values.insert(key.to_string(), json!(value));
		}
	}
	(!values.is_empty()).then(|| Value::Object(values))
}

async fn api_snapshot(State(state): State<Arc<AppState>>) -> Response {
	let snap = state.snapshot.read().await.clone();
	(
		StatusCode::OK,
		[(axum::http::header::CONTENT_TYPE, "application/json")],
		snap.to_string(),
	)
		.into_response()
}

async fn api_activity(State(state): State<Arc<AppState>>) -> Json<Value> {
	let activity = state.activity.read().await;
	let items: Vec<Value> = activity
		.iter()
		.filter_map(|s| serde_json::from_str(s).ok())
		.collect();
	Json(json!(items))
}

async fn api_stream(
	State(state): State<Arc<AppState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
	use tokio_stream::StreamExt;
	let rx = state.tx.subscribe();
	let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|msg| {
		msg.ok().map(|m| Ok(Event::default().data(m.to_string())))
	});
	Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn build_info_is_read_from_the_node_metrics() {
		let text = "# HELP ldk_server_build_info x\n# TYPE ldk_server_build_info gauge\n\
			ldk_server_build_info{component=\"ldk-server\",version=\"0.1.0\",branch=\"cbd-chain-source\",commit=\"7ad0bed5\",dirty=\"true\"} 1\n\
			ldk_server_build_info{component=\"kyoto\",version=\"0.8.1\",branch=\"a\\\"b\",commit=\"6c3f631d\",dirty=\"false\"} 1\n\
			ldk_server_uptime_seconds 5\n";
		let found = parse_build_info(text);
		assert_eq!(found.len(), 2);
		assert_eq!(found[0]["component"], "ldk-server");
		assert_eq!(found[0]["commit"], "7ad0bed5");
		assert_eq!(found[0]["dirty"], "true");
		assert_eq!(found[1]["component"], "kyoto");
		assert_eq!(found[1]["branch"], "a\"b");
		assert!(parse_build_info("ldk_server_uptime_seconds 5\n").is_empty());
	}

	#[test]
	fn own_build_info_has_four_fields() {
		let info = build_info_json("ldk-server-gui", "0.1.0|master|abc12345|false");
		assert_eq!(info["branch"], "master");
		assert_eq!(info["commit"], "abc12345");
		assert_eq!(build_info_json("x", "0.1.0")["commit"], "unknown");
	}
}
