/// This file based on Axum extract ws.rs and is a port to rustgram
/// The Axum license is MIT
use std::borrow::Cow;
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
use tokio_tungstenite::tungstenite::{self as ts};
use tokio_tungstenite::WebSocketStream;

use crate::BoxError;

/// Status code used to indicate why an endpoint is closing the WebSocket connection.
pub type CloseCode = u16;

/// A struct representing the close command.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CloseFrame<'t>
{
	/// The reason as a code.
	pub code: CloseCode,
	/// The reason as text string.
	pub reason: Cow<'t, str>,
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub enum Message
{
	/// A text WebSocket message
	Text(String),
	/// A binary WebSocket message
	Binary(Vec<u8>),
	/// A ping message with the specified payload
	///
	/// The payload here must have a length less than 125 bytes.
	///
	/// Ping messages will be automatically responded to by the server, so you do not have to worry
	/// about dealing with them yourself.
	Ping(Vec<u8>),
	/// A pong message with the specified payload
	///
	/// The payload here must have a length less than 125 bytes.
	///
	/// Pong messages will be automatically sent to the client if a ping message is received, so
	/// you do not have to worry about constructing them yourself unless you want to implement a
	/// [unidirectional heartbeat](https://tools.ietf.org/html/rfc6455#section-5.5.3).
	Pong(Vec<u8>),
	/// A close message with the optional close frame.
	Close(Option<CloseFrame<'static>>),
}

impl Message
{
	fn into_tungstenite(self) -> ts::Message
	{
		match self {
			Self::Text(text) => ts::Message::Text(text),
			Self::Binary(binary) => ts::Message::Binary(binary),
			Self::Ping(ping) => ts::Message::Ping(ping),
			Self::Pong(pong) => ts::Message::Pong(pong),
			Self::Close(Some(close)) => {
				ts::Message::Close(Some(protocol::CloseFrame {
					code: ts::protocol::frame::coding::CloseCode::from(close.code),
					reason: close.reason,
				}))
			},
			Self::Close(None) => ts::Message::Close(None),
		}
	}

	fn from_tungstenite(message: ts::Message) -> Option<Self>
	{
		match message {
			ts::Message::Text(text) => Some(Self::Text(text)),
			ts::Message::Binary(binary) => Some(Self::Binary(binary)),
			ts::Message::Ping(ping) => Some(Self::Ping(ping)),
			ts::Message::Pong(pong) => Some(Self::Pong(pong)),
			ts::Message::Close(Some(close)) => {
				Some(Self::Close(Some(CloseFrame {
					code: close.code.into(),
					reason: close.reason,
				})))
			},
			ts::Message::Close(None) => Some(Self::Close(None)),
			// we can ignore `Frame` frames as recommended by the tungstenite maintainers
			// https://github.com/snapview/tungstenite-rs/issues/268
			ts::Message::Frame(_) => None,
		}
	}

	/// Consume the WebSocket and return it as binary data.
	pub fn into_data(self) -> Vec<u8>
	{
		match self {
			Self::Text(string) => string.into_bytes(),
			Self::Binary(data) | Self::Ping(data) | Self::Pong(data) => data,
			Self::Close(None) => Vec::new(),
			Self::Close(Some(frame)) => frame.reason.into_owned().into_bytes(),
		}
	}

	/// Attempt to consume the WebSocket message and convert it to a String.
	pub fn into_text(self) -> Result<String, BoxError>
	{
		match self {
			Self::Text(string) => Ok(string),
			Self::Binary(data) | Self::Ping(data) | Self::Pong(data) => Ok(String::from_utf8(data).map_err(|err| err.utf8_error())?),
			Self::Close(None) => Ok(String::new()),
			Self::Close(Some(frame)) => Ok(frame.reason.into_owned()),
		}
	}

	/// Attempt to get a &str from the WebSocket message,
	/// this will try to convert binary data to utf8.
	pub fn to_text(&self) -> Result<&str, BoxError>
	{
		match *self {
			Self::Text(ref string) => Ok(string),
			Self::Binary(ref data) | Self::Ping(ref data) | Self::Pong(ref data) => Ok(std::str::from_utf8(data)?),
			Self::Close(None) => Ok(""),
			Self::Close(Some(ref frame)) => Ok(&frame.reason),
		}
	}
}

impl From<String> for Message
{
	fn from(string: String) -> Self
	{
		Message::Text(string)
	}
}

impl<'s> From<&'s str> for Message
{
	fn from(string: &'s str) -> Self
	{
		Message::Text(string.into())
	}
}

impl<'b> From<&'b [u8]> for Message
{
	fn from(data: &'b [u8]) -> Self
	{
		Message::Binary(data.into())
	}
}

impl From<Vec<u8>> for Message
{
	fn from(data: Vec<u8>) -> Self
	{
		Message::Binary(data)
	}
}

impl From<Message> for Vec<u8>
{
	fn from(msg: Message) -> Self
	{
		msg.into_data()
	}
}

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
		self.inner
			.send(msg.into_tungstenite())
			.await
			.map_err(Into::into)
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
		loop {
			match futures_util::ready!(self.inner.poll_next_unpin(cx)) {
				Some(Ok(msg)) => {
					if let Some(msg) = Message::from_tungstenite(msg) {
						return Poll::Ready(Some(Ok(msg)));
					}
				},
				Some(Err(err)) => return Poll::Ready(Some(Err(err.into()))),
				None => return Poll::Ready(None),
			}
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
			.start_send(item.into_tungstenite())
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
