/// This file based on Axum extract ws.rs and is a port to rustgram
/// The Axum license is MIT
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::sink::{Sink, SinkExt};
use futures_util::stream::{Stream, StreamExt};
use hyper::header::HeaderName;
use hyper::http::HeaderValue;
use hyper::upgrade::Upgraded;
use hyper::{header, Body, HeaderMap, StatusCode};
use rustgram::{Request, Response};
use sha1::{Digest, Sha1};
use tokio_tungstenite::tungstenite::protocol::{self};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::BoxError;

fn sign(key: &[u8]) -> HeaderValue
{
	use base64::engine::Engine as _;

	let mut sha1 = Sha1::default();
	sha1.update(key);
	sha1.update(&b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"[..]);
	let b64 = Bytes::from(base64::engine::general_purpose::STANDARD.encode(sha1.finalize()));
	HeaderValue::from_maybe_shared(b64).expect("base64 is a valid value")
}

pub struct WebSocket
{
	inner: WebSocketStream<Upgraded>,
	protocol: Option<HeaderValue>,
}

impl WebSocket
{
	/// Receive another message.
	///
	/// Returns `None` if the stream has closed.
	pub async fn recv(&mut self) -> Option<Result<Message, BoxError>>
	{
		self.next().await
	}

	/// Send a message.
	pub async fn send(&mut self, msg: Message) -> Result<(), BoxError>
	{
		self.inner.send(msg).await.map_err(Into::into)
	}

	/// Gracefully close this WebSocket.
	pub async fn close(mut self) -> Result<(), BoxError>
	{
		self.inner.close(None).await.map_err(Into::into)
	}

	/// Return the selected WebSocket subprotocol, if one has been chosen.
	pub fn protocol(&self) -> Option<&HeaderValue>
	{
		self.protocol.as_ref()
	}
}

impl Stream for WebSocket
{
	type Item = Result<Message, BoxError>;

	fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>>
	{
		match futures_util::ready!(self.inner.poll_next_unpin(cx)) {
			Some(Ok(msg)) => Poll::Ready(Some(Ok(msg))),
			Some(Err(err)) => Poll::Ready(Some(Err(err.into()))),
			None => Poll::Ready(None),
		}
	}
}

impl Sink<Message> for WebSocket
{
	type Error = BoxError;

	fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>>
	{
		Pin::new(&mut self.inner).poll_ready(cx).map_err(Into::into)
	}

	fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error>
	{
		Pin::new(&mut self.inner)
			.start_send(item)
			.map_err(Into::into)
	}

	fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>>
	{
		Pin::new(&mut self.inner).poll_flush(cx).map_err(Into::into)
	}

	fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>>
	{
		Pin::new(&mut self.inner).poll_close(cx).map_err(Into::into)
	}
}

//__________________________________________________________________________________________________

/// What to do when a connection upgrade fails.
///
/// See [`WebSocketUpgrade::on_failed_upgrade`] for more details.
pub trait OnFailedUpgrade: Send + 'static
{
	/// Call the callback.
	fn call(self, error: BoxError);
}

impl<F> OnFailedUpgrade for F
where
	F: FnOnce(BoxError) + Send + 'static,
{
	fn call(self, error: BoxError)
	{
		self(error)
	}
}

#[non_exhaustive]
#[derive(Debug)]
pub struct DefaultOnFailedUpgrade;

impl OnFailedUpgrade for DefaultOnFailedUpgrade
{
	#[inline]
	fn call(self, _error: BoxError) {}
}

//__________________________________________________________________________________________________

pub fn on_upgrade<C, Fut>(req: Request, callback: C) -> Result<Response, Response>
where
	C: FnOnce(WebSocket) -> Fut + Send + 'static,
	Fut: Future<Output = ()> + Send + 'static,
{
	on_upgrade_with_err(req, callback, DefaultOnFailedUpgrade)
}

pub fn on_upgrade_with_err<C, Fut, E>(mut req: Request, callback: C, on_failed_upgrade: E) -> Result<Response, Response>
where
	C: FnOnce(WebSocket) -> Fut + Send + 'static,
	Fut: Future<Output = ()> + Send + 'static,
	E: OnFailedUpgrade + Send + 'static,
{
	let headers = req.headers();

	if !header_contains(headers, header::CONNECTION, "upgrade") {
		return Err(err_res("Connection header did not include 'upgrade'"));
	}

	if !header_eq(headers, header::UPGRADE, "websocket") {
		return Err(err_res("`Upgrade` header did not include 'websocket'"));
	}

	if !header_eq(headers, header::SEC_WEBSOCKET_VERSION, "13") {
		return Err(err_res("`Sec-WebSocket-Version` header did not include '13'"));
	}

	let sec_websocket_key = headers
		.get(header::SEC_WEBSOCKET_KEY)
		.ok_or(err_res("`Sec-WebSocket-Key` header missing"))?
		.clone();

	#[allow(clippy::declare_interior_mutable_const)]
	const WEBSOCKET: HeaderValue = HeaderValue::from_static("websocket");

	let builder = hyper::Response::builder()
		.status(StatusCode::SWITCHING_PROTOCOLS)
		.header(header::CONNECTION, header::UPGRADE)
		.header(header::UPGRADE, WEBSOCKET)
		.header(header::SEC_WEBSOCKET_ACCEPT, sign(sec_websocket_key.as_bytes()));

	tokio::task::spawn(async move {
		let upgraded = match hyper::upgrade::on(&mut req).await {
			Ok(upgraded) => upgraded,
			Err(e) => {
				on_failed_upgrade.call(e.into());
				return;
			},
		};

		let socket = WebSocketStream::from_raw_socket(upgraded, protocol::Role::Server, None).await;

		let socket = WebSocket {
			inner: socket,
			protocol: None,
		};

		callback(socket).await;
	});

	Ok(builder.body(Body::empty()).unwrap())
}

fn header_eq(headers: &HeaderMap, key: HeaderName, value: &'static str) -> bool
{
	if let Some(header) = headers.get(&key) {
		header.as_bytes().eq_ignore_ascii_case(value.as_bytes())
	} else {
		false
	}
}

fn header_contains(headers: &HeaderMap, key: HeaderName, value: &'static str) -> bool
{
	let header = if let Some(header) = headers.get(&key) {
		header
	} else {
		return false;
	};

	if let Ok(header) = std::str::from_utf8(header.as_bytes()) {
		header.to_ascii_lowercase().contains(value)
	} else {
		false
	}
}

fn err_res(body: &'static str) -> Response
{
	hyper::Response::builder()
		.status(StatusCode::BAD_REQUEST)
		.body(Body::from(body))
		.unwrap()
}
