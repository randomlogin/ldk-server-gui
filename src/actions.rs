// Web-invokable node commands ("actions") and the read-only forwarding stats.
//
// Action handlers require the `X-Action-Token` header to match the token
// generated at startup (see `--enable-actions`). Stats handlers are plain
// read-only GETs like the rest of the dashboard.

use std::future::Future;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use ldk_server_client::error::LdkServerError;
use ldk_server_client::ldk_server_grpc::api::{
	onchain_send_request, open_channel_request, splice_in_request, AllFunds,
	Bolt11ReceiveRequest, Bolt11SendRequest, Bolt12SendRequest, CloseChannelRequest,
	ConnectPeerRequest, DecodeInvoiceRequest, DecodeOfferRequest, DisconnectPeerRequest,
	ForceCloseChannelRequest, GetChannelForwardingStatsRequest,
	GetForwardedPaymentTrackingModeRequest, ListForwardedPaymentsRequest, OnchainReceiveRequest,
	OnchainSendRequest, OpenChannelRequest, SpliceInRequest, SpliceOutRequest,
	SpontaneousSendRequest, UpdateChannelConfigRequest,
};
use ldk_server_client::ldk_server_grpc::types::{
	bolt11_invoice_description, Bolt11InvoiceDescription, ChannelConfig,
};
use ldk_server_client::DEFAULT_EXPIRY_SECS;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{AppState, CONTROL_HTML};

pub async fn control_page(State(state): State<Arc<AppState>>) -> Response {
	if state.action_token.is_some() {
		Html(crate::with_versions(CONTROL_HTML)).into_response()
	} else {
		err_json(
			StatusCode::NOT_FOUND,
			"controls are disabled; restart with --enable-actions to use this page",
		)
	}
}

// Rejects the request when actions are disabled or the token doesn't match.
// Returns the error response to send back, or None when the request may proceed.
fn denied(state: &AppState, headers: &HeaderMap) -> Option<Response> {
	let Some(expected) = state.action_token.as_deref() else {
		return Some(err_json(StatusCode::NOT_FOUND, "actions are disabled"));
	};
	let provided = headers.get("x-action-token").and_then(|v| v.to_str().ok()).unwrap_or("");
	let mismatch = provided.bytes().zip(expected.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b));
	if provided.len() != expected.len() || mismatch != 0 {
		return Some(err_json(StatusCode::FORBIDDEN, "invalid or missing action token"));
	}
	None
}

// Tells the control page whether the token it holds is the right one, without doing anything. The
// page uses it to refuse to send commands with a wrong token, instead of finding out on each one.
pub async fn check_token(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
	if let Some(r) = denied(&state, &headers) {
		return r;
	}
	Json(json!({ "ok": true })).into_response()
}

fn err_json(status: StatusCode, msg: impl std::fmt::Display) -> Response {
	(status, Json(json!({ "ok": false, "error": msg.to_string() }))).into_response()
}

fn ok_json<T: serde::Serialize>(v: &T) -> Response {
	match serde_json::to_value(v) {
		Ok(mut val) => {
			if let Some(obj) = val.as_object_mut() {
				obj.insert("ok".to_string(), Value::Bool(true));
			}
			Json(val).into_response()
		},
		Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, e),
	}
}

// Runs a node command: on success triggers an immediate snapshot refresh and
// records the action in the activity feed; on failure reports the node error.
async fn run<T, F>(state: &AppState, summary: String, fut: F) -> Response
where
	F: Future<Output = Result<T, LdkServerError>>,
	T: serde::Serialize,
{
	match fut.await {
		Ok(resp) => {
			state.refresh.notify_one();
			state.log_action(summary).await;
			ok_json(&resp)
		},
		Err(e) => err_json(StatusCode::BAD_GATEWAY, e),
	}
}

fn is_hex(s: &str) -> bool {
	!s.is_empty() && s.len() % 2 == 0 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

// Hex-encoded compressed public key / node id.
fn valid_node_id(s: &str) -> bool {
	s.len() == 66 && is_hex(s)
}

fn short_key(k: &str) -> String {
	if k.len() > 16 {
		format!("{}…{}", &k[..10], &k[k.len() - 6..])
	} else {
		k.to_string()
	}
}

fn channel_ref_ok(user_channel_id: &str, counterparty_node_id: &str) -> bool {
	!user_channel_id.is_empty() && valid_node_id(counterparty_node_id)
}

#[derive(Deserialize)]
pub struct OpenChannelInput {
	node_pubkey: String,
	address: Option<String>,
	amount_sats: Option<u64>,
	push_to_counterparty_msat: Option<u64>,
	announce_channel: Option<bool>,
}

pub async fn open_channel(
	State(state): State<Arc<AppState>>, headers: HeaderMap, Json(input): Json<OpenChannelInput>,
) -> Response {
	if let Some(r) = denied(&state, &headers) {
		return r;
	}
	// Accept "pubkey@address" in a single field like ldk-server-cli.
	let (node_pubkey, address) = match input.address.filter(|a| !a.trim().is_empty()) {
		Some(address) => (input.node_pubkey, address),
		None => match input.node_pubkey.split_once('@') {
			Some((pk, addr)) => (pk.to_string(), addr.to_string()),
			None => {
				return err_json(
					StatusCode::BAD_REQUEST,
					"address is required; provide it separately or as pubkey@address",
				)
			},
		},
	};
	if !valid_node_id(&node_pubkey) || address.trim().is_empty() {
		return err_json(StatusCode::BAD_REQUEST, "invalid node pubkey or address");
	}
	let Some(amount_sats) = input.amount_sats.filter(|a| *a > 0) else {
		return err_json(StatusCode::BAD_REQUEST, "amount_sats must be a positive number");
	};
	let request = OpenChannelRequest {
		node_pubkey: node_pubkey.clone(),
		address,
		amount: Some(open_channel_request::Amount::ChannelAmountSats(amount_sats)),
		push_to_counterparty_msat: input.push_to_counterparty_msat,
		channel_config: None,
		announce_channel: input.announce_channel.unwrap_or(false),
		disable_counterparty_reserve: false,
	};
	let summary = format!("open channel: {amount_sats} sat → {}", short_key(&node_pubkey));
	run(&state, summary, state.client.open_channel(request)).await
}

#[derive(Deserialize)]
pub struct CloseChannelInput {
	user_channel_id: String,
	counterparty_node_id: String,
}

pub async fn close_channel(
	State(state): State<Arc<AppState>>, headers: HeaderMap, Json(input): Json<CloseChannelInput>,
) -> Response {
	if let Some(r) = denied(&state, &headers) {
		return r;
	}
	if !channel_ref_ok(&input.user_channel_id, &input.counterparty_node_id) {
		return err_json(StatusCode::BAD_REQUEST, "invalid channel reference");
	}
	let summary = format!("close channel {}", short_key(&input.user_channel_id));
	let request = CloseChannelRequest {
		user_channel_id: input.user_channel_id,
		counterparty_node_id: input.counterparty_node_id,
	};
	run(&state, summary, state.client.close_channel(request)).await
}

#[derive(Deserialize)]
pub struct ForceCloseChannelInput {
	user_channel_id: String,
	counterparty_node_id: String,
	reason: Option<String>,
}

pub async fn force_close_channel(
	State(state): State<Arc<AppState>>, headers: HeaderMap, Json(input): Json<ForceCloseChannelInput>,
) -> Response {
	if let Some(r) = denied(&state, &headers) {
		return r;
	}
	if !channel_ref_ok(&input.user_channel_id, &input.counterparty_node_id) {
		return err_json(StatusCode::BAD_REQUEST, "invalid channel reference");
	}
	let summary = format!("force-close channel {}", short_key(&input.user_channel_id));
	let request = ForceCloseChannelRequest {
		user_channel_id: input.user_channel_id,
		counterparty_node_id: input.counterparty_node_id,
		force_close_reason: input.reason.filter(|r| !r.trim().is_empty()),
	};
	run(&state, summary, state.client.force_close_channel(request)).await
}

#[derive(Deserialize)]
pub struct SpliceInInput {
	user_channel_id: String,
	counterparty_node_id: String,
	splice_amount_sats: u64,
}

pub async fn splice_in(
	State(state): State<Arc<AppState>>, headers: HeaderMap, Json(input): Json<SpliceInInput>,
) -> Response {
	if let Some(r) = denied(&state, &headers) {
		return r;
	}
	if !channel_ref_ok(&input.user_channel_id, &input.counterparty_node_id)
		|| input.splice_amount_sats == 0
	{
		return err_json(StatusCode::BAD_REQUEST, "invalid channel reference or amount");
	}
	let summary = format!(
		"splice in {} sat → channel {}",
		input.splice_amount_sats,
		short_key(&input.user_channel_id)
	);
	let request = SpliceInRequest {
		user_channel_id: input.user_channel_id,
		counterparty_node_id: input.counterparty_node_id,
		amount: Some(splice_in_request::Amount::SpliceAmountSats(input.splice_amount_sats)),
	};
	run(&state, summary, state.client.splice_in(request)).await
}

#[derive(Deserialize)]
pub struct SpliceOutInput {
	user_channel_id: String,
	counterparty_node_id: String,
	address: Option<String>,
	splice_amount_sats: u64,
}

pub async fn splice_out(
	State(state): State<Arc<AppState>>, headers: HeaderMap, Json(input): Json<SpliceOutInput>,
) -> Response {
	if let Some(r) = denied(&state, &headers) {
		return r;
	}
	if !channel_ref_ok(&input.user_channel_id, &input.counterparty_node_id)
		|| input.splice_amount_sats == 0
	{
		return err_json(StatusCode::BAD_REQUEST, "invalid channel reference or amount");
	}
	let summary = format!(
		"splice out {} sat ← channel {}",
		input.splice_amount_sats,
		short_key(&input.user_channel_id)
	);
	let request = SpliceOutRequest {
		user_channel_id: input.user_channel_id,
		counterparty_node_id: input.counterparty_node_id,
		address: input.address.filter(|a| !a.trim().is_empty()),
		splice_amount_sats: input.splice_amount_sats,
	};
	run(&state, summary, state.client.splice_out(request)).await
}

#[derive(Deserialize)]
pub struct UpdateChannelConfigInput {
	user_channel_id: String,
	counterparty_node_id: String,
	forwarding_fee_proportional_millionths: Option<u32>,
	forwarding_fee_base_msat: Option<u32>,
	cltv_expiry_delta: Option<u32>,
}

pub async fn update_channel_config(
	State(state): State<Arc<AppState>>, headers: HeaderMap, Json(input): Json<UpdateChannelConfigInput>,
) -> Response {
	if let Some(r) = denied(&state, &headers) {
		return r;
	}
	if !channel_ref_ok(&input.user_channel_id, &input.counterparty_node_id) {
		return err_json(StatusCode::BAD_REQUEST, "invalid channel reference");
	}
	if input.forwarding_fee_proportional_millionths.is_none()
		&& input.forwarding_fee_base_msat.is_none()
		&& input.cltv_expiry_delta.is_none()
	{
		return err_json(StatusCode::BAD_REQUEST, "set at least one config field");
	}
	let channel_config = Some(ChannelConfig {
		forwarding_fee_proportional_millionths: input.forwarding_fee_proportional_millionths,
		forwarding_fee_base_msat: input.forwarding_fee_base_msat,
		cltv_expiry_delta: input.cltv_expiry_delta,
		force_close_avoidance_max_fee_satoshis: None,
		accept_underpaying_htlcs: None,
		max_dust_htlc_exposure: None,
	});
	let summary = format!("update channel config: {}", short_key(&input.user_channel_id));
	let request = UpdateChannelConfigRequest {
		user_channel_id: input.user_channel_id,
		counterparty_node_id: input.counterparty_node_id,
		channel_config,
	};
	run(&state, summary, state.client.update_channel_config(request)).await
}

#[derive(Deserialize)]
pub struct Bolt11SendInput {
	invoice: String,
	amount_msat: Option<u64>,
}

pub async fn bolt11_send(
	State(state): State<Arc<AppState>>, headers: HeaderMap, Json(input): Json<Bolt11SendInput>,
) -> Response {
	if let Some(r) = denied(&state, &headers) {
		return r;
	}
	if !input.invoice.trim_start().to_ascii_lowercase().starts_with("ln") {
		return err_json(StatusCode::BAD_REQUEST, "invalid bolt11 invoice");
	}
	let summary = format!(
		"pay bolt11 invoice {}{}",
		short_key(&input.invoice),
		input.amount_msat.map(|a| format!(" ({} msat)", a)).unwrap_or_default()
	);
	let request = Bolt11SendRequest {
		invoice: input.invoice.trim().to_string(),
		amount_msat: input.amount_msat.filter(|a| *a > 0),
		route_parameters: None,
	};
	run(&state, summary, state.client.bolt11_send(request)).await
}

#[derive(Deserialize)]
pub struct Bolt12SendInput {
	offer: String,
	amount_msat: Option<u64>,
	quantity: Option<u64>,
	payer_note: Option<String>,
}

pub async fn bolt12_send(
	State(state): State<Arc<AppState>>, headers: HeaderMap, Json(input): Json<Bolt12SendInput>,
) -> Response {
	if let Some(r) = denied(&state, &headers) {
		return r;
	}
	if !input.offer.trim_start().to_ascii_lowercase().starts_with("lno") {
		return err_json(StatusCode::BAD_REQUEST, "invalid bolt12 offer");
	}
	let summary = format!("pay bolt12 offer {}", short_key(&input.offer));
	let request = Bolt12SendRequest {
		offer: input.offer.trim().to_string(),
		amount_msat: input.amount_msat.filter(|a| *a > 0),
		quantity: input.quantity.filter(|q| *q > 0),
		payer_note: input.payer_note.filter(|n| !n.trim().is_empty()),
		route_parameters: None,
	};
	run(&state, summary, state.client.bolt12_send(request)).await
}

#[derive(Deserialize)]
pub struct SpontaneousSendInput {
	node_id: String,
	amount_msat: u64,
}

pub async fn spontaneous_send(
	State(state): State<Arc<AppState>>, headers: HeaderMap, Json(input): Json<SpontaneousSendInput>,
) -> Response {
	if let Some(r) = denied(&state, &headers) {
		return r;
	}
	if !valid_node_id(&input.node_id) || input.amount_msat == 0 {
		return err_json(StatusCode::BAD_REQUEST, "invalid node id or amount");
	}
	let summary = format!("keysend {} msat → {}", input.amount_msat, short_key(&input.node_id));
	let request = SpontaneousSendRequest {
		amount_msat: input.amount_msat,
		node_id: input.node_id,
		route_parameters: None,
		custom_tlvs: Vec::new(),
		preimage: None,
	};
	run(&state, summary, state.client.spontaneous_send(request)).await
}

#[derive(Deserialize)]
pub struct Bolt11ReceiveInput {
	amount_msat: Option<u64>,
	description: Option<String>,
	expiry_secs: Option<u32>,
}

pub async fn bolt11_receive(
	State(state): State<Arc<AppState>>, headers: HeaderMap, Json(input): Json<Bolt11ReceiveInput>,
) -> Response {
	if let Some(r) = denied(&state, &headers) {
		return r;
	}
	let description = input
		.description
		.filter(|d| !d.trim().is_empty())
		.map(|desc| Bolt11InvoiceDescription {
			kind: Some(bolt11_invoice_description::Kind::Direct(desc)),
		});
	let amount_msat = input.amount_msat.filter(|a| *a > 0);
	let summary = format!(
		"create invoice{}",
		amount_msat.map(|a| format!(" for {} msat", a)).unwrap_or_default()
	);
	let request = Bolt11ReceiveRequest {
		description,
		expiry_secs: input.expiry_secs.unwrap_or(DEFAULT_EXPIRY_SECS),
		amount_msat,
	};
	run(&state, summary, state.client.bolt11_receive(request)).await
}

#[derive(Deserialize)]
pub struct OnchainSendInput {
	address: String,
	amount_sats: Option<u64>,
	all_funds: Option<bool>,
	fee_rate_sat_per_vb: Option<u64>,
}

pub async fn onchain_send(
	State(state): State<Arc<AppState>>, headers: HeaderMap, Json(input): Json<OnchainSendInput>,
) -> Response {
	if let Some(r) = denied(&state, &headers) {
		return r;
	}
	if input.address.trim().is_empty() {
		return err_json(StatusCode::BAD_REQUEST, "address is required");
	}
	let amount = match (input.amount_sats.filter(|a| *a > 0), input.all_funds.unwrap_or(false)) {
		(Some(sats), false) => onchain_send_request::Amount::AmountSats(sats),
		(None, true) => onchain_send_request::Amount::AllFunds(AllFunds {}),
		(Some(_), true) => {
			return err_json(
				StatusCode::BAD_REQUEST,
				"set either amount_sats or all_funds, not both",
			)
		},
		(None, false) => {
			return err_json(StatusCode::BAD_REQUEST, "set amount_sats or all_funds")
		},
	};
	let summary = format!("on-chain send → {}", short_key(&input.address));
	let request = OnchainSendRequest {
		address: input.address,
		fee_rate_sat_per_vb: input.fee_rate_sat_per_vb.filter(|f| *f > 0),
		amount: Some(amount),
	};
	run(&state, summary, state.client.onchain_send(request)).await
}

pub async fn onchain_receive(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
	if let Some(r) = denied(&state, &headers) {
		return r;
	}
	run(&state, "new on-chain address".to_string(), state.client.onchain_receive(OnchainReceiveRequest {}))
		.await
}

#[derive(Deserialize)]
pub struct ConnectPeerInput {
	node_pubkey: String,
	address: Option<String>,
	persist: Option<bool>,
}

pub async fn connect_peer(
	State(state): State<Arc<AppState>>, headers: HeaderMap, Json(input): Json<ConnectPeerInput>,
) -> Response {
	if let Some(r) = denied(&state, &headers) {
		return r;
	}
	// Accept "pubkey@address" in a single field like ldk-server-cli.
	let (node_pubkey, address) = match input.address.filter(|a| !a.trim().is_empty()) {
		Some(address) => (input.node_pubkey, address),
		None => match input.node_pubkey.split_once('@') {
			Some((pk, addr)) => (pk.to_string(), addr.to_string()),
			None => {
				return err_json(
					StatusCode::BAD_REQUEST,
					"address is required; provide it separately or as pubkey@address",
				)
			},
		},
	};
	if !valid_node_id(&node_pubkey) {
		return err_json(StatusCode::BAD_REQUEST, "invalid node pubkey");
	}
	let summary = format!("connect peer {}", short_key(&node_pubkey));
	let request = ConnectPeerRequest {
		node_pubkey,
		address,
		persist: input.persist.unwrap_or(false),
	};
	run(&state, summary, state.client.connect_peer(request)).await
}

#[derive(Deserialize)]
pub struct DisconnectPeerInput {
	node_pubkey: String,
}

pub async fn disconnect_peer(
	State(state): State<Arc<AppState>>, headers: HeaderMap, Json(input): Json<DisconnectPeerInput>,
) -> Response {
	if let Some(r) = denied(&state, &headers) {
		return r;
	}
	if !valid_node_id(&input.node_pubkey) {
		return err_json(StatusCode::BAD_REQUEST, "invalid node pubkey");
	}
	let summary = format!("disconnect peer {}", short_key(&input.node_pubkey));
	let request = DisconnectPeerRequest { node_pubkey: input.node_pubkey };
	run(&state, summary, state.client.disconnect_peer(request)).await
}

// Read-only helpers below: no token needed, in line with the rest of the dashboard.

#[derive(Deserialize)]
pub struct DecodeInvoiceInput {
	invoice: String,
}

pub async fn decode_invoice(
	State(state): State<Arc<AppState>>, Json(input): Json<DecodeInvoiceInput>,
) -> Response {
	let request = DecodeInvoiceRequest { invoice: input.invoice.trim().to_string() };
	match state.client.decode_invoice(request).await {
		Ok(resp) => ok_json(&resp),
		Err(e) => err_json(StatusCode::BAD_GATEWAY, e),
	}
}

#[derive(Deserialize)]
pub struct DecodeOfferInput {
	offer: String,
}

pub async fn decode_offer(
	State(state): State<Arc<AppState>>, Json(input): Json<DecodeOfferInput>,
) -> Response {
	let request = DecodeOfferRequest { offer: input.offer.trim().to_string() };
	match state.client.decode_offer(request).await {
		Ok(resp) => ok_json(&resp),
		Err(e) => err_json(StatusCode::BAD_GATEWAY, e),
	}
}

pub async fn tracking_mode(State(state): State<Arc<AppState>>) -> Response {
	match state.client.get_forwarded_payment_tracking_mode(GetForwardedPaymentTrackingModeRequest {}).await {
		Ok(resp) => ok_json(&resp),
		Err(e) => err_json(StatusCode::BAD_GATEWAY, e),
	}
}

#[derive(Deserialize)]
pub struct PageQuery {
	page_token: Option<String>,
}

pub async fn forwarded_payments(
	State(state): State<Arc<AppState>>, Query(query): Query<PageQuery>,
) -> Response {
	let request = ListForwardedPaymentsRequest {
		page_token: query.page_token.filter(|t| !t.is_empty()),
	};
	match state.client.list_forwarded_payments(request).await {
		Ok(resp) => ok_json(&resp),
		Err(e) => err_json(StatusCode::BAD_GATEWAY, e),
	}
}

pub async fn channel_stats(
	State(state): State<Arc<AppState>>, Path(channel_id): Path<String>,
) -> Response {
	if !is_hex(&channel_id) {
		return err_json(StatusCode::BAD_REQUEST, "invalid channel id");
	}
	let request = GetChannelForwardingStatsRequest { channel_id };
	match state.client.get_channel_forwarding_stats(request).await {
		Ok(resp) => ok_json(&resp),
		Err(e) => err_json(StatusCode::BAD_GATEWAY, e),
	}
}
