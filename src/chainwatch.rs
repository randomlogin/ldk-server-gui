// Chain-source health page: detects the configured chain source from the
// ldk-server config file ([bitcoind]/[electrum]/[esplora]/[cbf] sections),
// probes it directly (the node exposes no stats for non-CBF sources), and
// compares its chain tip with the node's tip from the snapshot.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::Json;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::{now_unix, AppState};

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The chain source configured for the node, as read from its config file.
#[derive(Clone)]
pub enum ChainSource {
	Bitcoind {
		rpc: Option<String>,
		user: Option<String>,
		password: Option<String>,
		rest: Option<String>,
	},
	Electrum { url: String },
	Esplora { url: String },
	Cbf,
	/// No config file or no recognized chain source section.
	Unknown,
}

impl ChainSource {
	pub fn kind(&self) -> &'static str {
		match self {
			ChainSource::Bitcoind { .. } => "bitcoind",
			ChainSource::Electrum { .. } => "electrum",
			ChainSource::Esplora { .. } => "esplora",
			ChainSource::Cbf => "cbf",
			ChainSource::Unknown => "unknown",
		}
	}

	/// The endpoint to show in the UI. Never contains credentials.
	fn endpoint(&self) -> Option<String> {
		match self {
			ChainSource::Bitcoind { rpc, rest, .. } => rest.clone().or_else(|| rpc.clone()),
			ChainSource::Electrum { url } => Some(url.clone()),
			ChainSource::Esplora { url } => Some(url.clone()),
			ChainSource::Cbf | ChainSource::Unknown => None,
		}
	}
}

#[derive(Deserialize, Default)]
struct TomlChainSections {
	bitcoind: Option<BitcoindSection>,
	electrum: Option<UrlSection>,
	esplora: Option<UrlSection>,
	cbf: Option<toml::Value>,
}

#[derive(Deserialize)]
struct BitcoindSection {
	// All optional, as in the node's own config (they can also come from its CLI or environment).
	rpc_address: Option<String>,
	rpc_user: Option<String>,
	rpc_password: Option<String>,
	rest_address: Option<String>,
}

#[derive(Deserialize)]
struct UrlSection {
	server_url: String,
}

/// Reads only the chain-source sections out of the node's TOML config file.
pub fn detect_chain_source(config_path: Option<&std::path::Path>) -> ChainSource {
	let Some(path) = config_path else { return ChainSource::Unknown };
	let Ok(content) = std::fs::read_to_string(path) else { return ChainSource::Unknown };
	let Ok(sections) = toml::from_str::<TomlChainSections>(&content) else {
		return ChainSource::Unknown;
	};
	if let Some(b) = sections.bitcoind {
		ChainSource::Bitcoind {
			rpc: b.rpc_address,
			user: b.rpc_user,
			password: b.rpc_password,
			rest: b.rest_address,
		}
	} else if let Some(e) = sections.electrum {
		ChainSource::Electrum { url: e.server_url }
	} else if let Some(e) = sections.esplora {
		ChainSource::Esplora { url: e.server_url }
	} else if sections.cbf.is_some() {
		ChainSource::Cbf
	} else {
		ChainSource::Unknown
	}
}

pub async fn api_chainsource(State(state): State<Arc<AppState>>) -> Json<Value> {
	let source = state.chain_source.clone();
	let (probe, node) = tokio::join!(probe_source(&state, &source), node_tip(&state));

	let probe = match probe {
		Ok(v) => json!({ "ok": true, "data": v }),
		Err(e) => json!({ "ok": false, "error": e }),
	};
	let lag = match (
		probe.pointer("/data/tip_height").and_then(Value::as_u64),
		node.pointer("/tip_height").and_then(Value::as_u64),
	) {
		(Some(src), Some(node_tip)) => json!(src as i64 - node_tip as i64),
		_ => json!(null),
	};

	Json(json!({
		"fetched_at": now_unix(),
		"kind": source.kind(),
		"endpoint": source.endpoint(),
		"probe": probe,
		"node": node,
		"lag_blocks": lag,
	}))
}

// The node's own view: best block and wallet-sync staleness, pulled from the
// cached snapshot instead of hitting gRPC again.
async fn node_tip(state: &AppState) -> Value {
	let snap = state.snapshot.read().await;
	let Ok(v) = serde_json::from_str::<Value>(&snap) else {
		return json!({ "available": false });
	};
	let node = &v["node"];
	if node.get("error").is_some() {
		return json!({ "available": false, "error": node["error"] });
	}
	if !node.is_object() || node.get("current_best_block").is_none() {
		return json!({ "available": false });
	}
	json!({
		"available": true,
		"tip_height": node["current_best_block"]["height"],
		"tip_hash": node["current_best_block"]["block_hash"],
		"onchain_wallet_sync_timestamp": node["latest_onchain_wallet_sync_timestamp"],
		"lightning_wallet_sync_timestamp": node["latest_lightning_wallet_sync_timestamp"],
	})
}

async fn probe_source(state: &AppState, source: &ChainSource) -> Result<Value, String> {
	match source {
		ChainSource::Esplora { url } => probe_esplora(state, url).await,
		ChainSource::Bitcoind { rpc, user, password, rest } => {
			probe_bitcoind(
				state,
				rpc.as_deref(),
				user.as_deref(),
				password.as_deref(),
				rest.as_deref(),
			)
			.await
		},
		ChainSource::Electrum { url } => probe_electrum(url).await,
		ChainSource::Cbf => probe_cbf(state).await,
		ChainSource::Unknown => {
			Err("no chain source found in the node config (bitcoind/electrum/esplora/cbf section)"
				.to_string())
		},
	}
}

async fn get_text(state: &AppState, url: &str) -> Result<String, String> {
	let response = state.http.get(url).send().await.map_err(|e| format!("{url}: {e}"))?;
	if !response.status().is_success() {
		return Err(format!("{url}: HTTP {}", response.status()));
	}
	response.text().await.map_err(|e| format!("{url}: {e}"))
}

async fn get_json(state: &AppState, url: &str) -> Result<Value, String> {
	let body = get_text(state, url).await?;
	serde_json::from_str(&body).map_err(|e| format!("{url}: invalid JSON: {e}"))
}

/// [`ChainSource::Esplora`]: plain HTTP, tip height + tip hash.
async fn probe_esplora(state: &AppState, base: &str) -> Result<Value, String> {
	let base = base.trim_end_matches('/');
	let height: u64 = get_text(state, &format!("{base}/blocks/tip/height"))
		.await?
		.trim()
		.parse()
		.map_err(|e| format!("{base}: invalid tip height: {e}"))?;
	let hash = get_text(state, &format!("{base}/blocks/tip/hash")).await?;
	Ok(json!({ "via": "esplora http", "tip_height": height, "tip_hash": hash.trim() }))
}

/// [`ChainSource::Bitcoind`]: prefers the REST interface when configured,
/// otherwise a `getblockchaininfo` RPC with the credentials from the config.
async fn probe_bitcoind(
	state: &AppState, rpc: Option<&str>, user: Option<&str>, password: Option<&str>,
	rest: Option<&str>,
) -> Result<Value, String> {
	if let Some(rest) = rest {
		let info = get_json(state, &format!("http://{rest}/rest/chaininfo.json")).await?;
		return Ok(json!({
			"via": "bitcoind rest",
			"tip_height": info["blocks"],
			"tip_hash": info["bestblockhash"],
		}));
	}
	let (Some(rpc), Some(user), Some(password)) = (rpc, user, password) else {
		return Err("the node config has no complete bitcoind RPC settings (rpc_address, rpc_user, \
			rpc_password) and no rest_address to probe instead"
			.to_string());
	};
	let auth = BASE64.encode(format!("{user}:{password}"));
	let body = r#"{"jsonrpc":"1.0","id":"ldk-gui","method":"getblockchaininfo","params":[]}"#;
	let response = state
		.http
		.post(format!("http://{rpc}/"))
		.header("authorization", format!("Basic {auth}"))
		.header("content-type", "application/json")
		.body(body)
		.send()
		.await
		.map_err(|e| format!("{rpc}: {e}"))?;
	if !response.status().is_success() {
		return Err(format!("{rpc}: HTTP {}", response.status()));
	}
	let text = response.text().await.map_err(|e| format!("{rpc}: {e}"))?;
	let v: Value =
		serde_json::from_str(&text).map_err(|e| format!("{rpc}: invalid JSON: {e}"))?;
	let result = &v["result"];
	if result.is_null() {
		return Err(format!("{rpc}: {}", v["error"]));
	}
	Ok(json!({
		"via": "bitcoind rpc",
		"tip_height": result["blocks"],
		"tip_hash": result["bestblockhash"],
	}))
}

/// [`ChainSource::Electrum`]: asks `blockchain.headers.subscribe` for the tip, over plain TCP
/// for `tcp://` and over TLS for `ssl://`. The TLS certificate is verified against the web PKI
/// roots, which is also what the node does, so a server the probe cannot verify is one the node
/// cannot use either.
async fn probe_electrum(url: &str) -> Result<Value, String> {
	let (scheme, addr) =
		url.split_once("://").ok_or_else(|| format!("invalid electrum URL '{url}'"))?;
	let addr = addr.trim_end_matches('/');
	let tcp = tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(addr))
		.await
		.map_err(|_| format!("{addr}: connection timed out"))?
		.map_err(|e| format!("{addr}: {e}"))?;
	match scheme {
		"tcp" => electrum_tip(tcp, addr, "electrum headers.subscribe").await,
		"ssl" => {
			let host = addr.rsplit_once(':').map_or(addr, |(host, _)| host);
			let name = tokio_rustls::rustls::ServerName::try_from(host)
				.map_err(|e| format!("{host}: invalid server name: {e}"))?;
			let tls = tokio::time::timeout(PROBE_TIMEOUT, tls_connector().connect(name, tcp))
				.await
				.map_err(|_| format!("{addr}: TLS handshake timed out"))?
				.map_err(|e| format!("{addr}: TLS: {e}"))?;
			electrum_tip(tls, addr, "electrum headers.subscribe (tls)").await
		},
		other => Err(format!("unsupported electrum scheme '{other}' in '{url}'")),
	}
}

fn tls_connector() -> tokio_rustls::TlsConnector {
	use tokio_rustls::rustls::{ClientConfig, OwnedTrustAnchor, RootCertStore};
	let mut roots = RootCertStore::empty();
	roots.add_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.iter().map(|ta| {
		OwnedTrustAnchor::from_subject_spki_name_constraints(
			ta.subject,
			ta.spki,
			ta.name_constraints,
		)
	}));
	let config = ClientConfig::builder()
		.with_safe_defaults()
		.with_root_certificates(roots)
		.with_no_client_auth();
	tokio_rustls::TlsConnector::from(Arc::new(config))
}

async fn electrum_tip<S>(stream: S, addr: &str, via: &str) -> Result<Value, String>
where
	S: tokio::io::AsyncRead + tokio::io::AsyncWrite,
{
	let (reader, mut writer) = tokio::io::split(stream);
	let request =
		r#"{"jsonrpc":"2.0","id":"ldk-gui","method":"blockchain.headers.subscribe","params":[]}"#
			.to_string() + "\n";
	tokio::time::timeout(PROBE_TIMEOUT, writer.write_all(request.as_bytes()))
		.await
		.map_err(|_| format!("{addr}: write timed out"))?
		.map_err(|e| format!("{addr}: {e}"))?;
	let mut line = String::new();
	tokio::time::timeout(PROBE_TIMEOUT, BufReader::new(reader).read_line(&mut line))
		.await
		.map_err(|_| format!("{addr}: response timed out"))?
		.map_err(|e| format!("{addr}: {e}"))?;
	if line.is_empty() {
		return Err(format!("{addr}: connection closed without a response"));
	}
	let v: Value =
		serde_json::from_str(line.trim()).map_err(|e| format!("{addr}: invalid JSON: {e}"))?;
	let height = v["result"]["height"]
		.as_u64()
		.ok_or_else(|| format!("{addr}: unexpected response: {}", line.trim()))?;
	let hash = v["result"]["hex"].as_str().and_then(header_hash);
	Ok(json!({ "via": via, "tip_height": height, "tip_hash": hash }))
}

/// The block hash of a hex-encoded 80-byte block header: double SHA-256, byte-reversed.
fn header_hash(header_hex: &str) -> Option<String> {
	if header_hex.len() != 160 || !header_hex.is_ascii() {
		return None;
	}
	let header: Vec<u8> = (0..80)
		.map(|i| u8::from_str_radix(&header_hex[i * 2..i * 2 + 2], 16).ok())
		.collect::<Option<_>>()?;
	let first = ring::digest::digest(&ring::digest::SHA256, &header);
	let second = ring::digest::digest(&ring::digest::SHA256, first.as_ref());
	Some(second.as_ref().iter().rev().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
	use super::header_hash;

	#[test]
	fn genesis_header_hash() {
		let header = "0100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4a29ab5f49ffff001d1dac2b7c";
		assert_eq!(
			header_hash(header).as_deref(),
			Some("000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f")
		);
		assert_eq!(header_hash("00"), None);
	}
}

/// [`ChainSource::Cbf`]: the chain source is the local kyoto light client;
/// use its own stats endpoint as the liveness probe.
async fn probe_cbf(state: &AppState) -> Result<Value, String> {
	let stats = get_json(state, &state.kyoto_stats_url).await?;
	Ok(json!({
		"via": "kyoto (local compact block filter client)",
		"peers_connected": stats["connected_peers"].as_u64().unwrap_or(0),
		"details_page": "/kyoto",
	}))
}
