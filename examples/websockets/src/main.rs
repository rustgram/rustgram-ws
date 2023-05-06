use std::net::SocketAddr;

use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use futures_util::SinkExt;
use hyper::StatusCode;
use rustgram::service::IntoResponse;
use rustgram::{r, Request, Response, Router};
use rustgram_ws::ws::WebSocket;
use rustgram_ws::TungsteniteMessage;

async fn not_found_handler(_req: Request) -> Response
{
	hyper::Response::builder()
		.status(StatusCode::NOT_FOUND)
		.body("Not found".into())
		.unwrap()
}

#[tokio::main]
async fn main()
{
	let mut router = Router::new(not_found_handler);

	router.get("/ws", r(ws_handler));
	router.get("/ws_split", r(ws_handler_concurrently));

	let addr = SocketAddr::from(([127, 0, 0, 1], 3000));

	//start the app
	rustgram::start(router, addr).await;
}

async fn ws_handler(req: Request) -> impl IntoResponse<Response>
{
	rustgram_ws::ws::on_upgrade(req, handle)
}

async fn handle(mut socket: WebSocket)
{
	while let Some(msg) = socket.recv().await {
		let msg = if let Ok(msg) = msg {
			msg
		} else {
			// client disconnected
			return;
		};

		if socket.send(msg).await.is_err() {
			// client disconnected
			return;
		}
	}
}

async fn ws_handler_concurrently(req: Request) -> impl IntoResponse<Response>
{
	rustgram_ws::ws::on_upgrade(req, handle_concurrently)
}

async fn handle_concurrently(socket: WebSocket)
{
	let (sender, receiver) = socket.split();

	tokio::spawn(write_socket(sender));
	tokio::spawn(read_socket(receiver));
}

async fn read_socket(mut receiver: SplitStream<WebSocket>)
{
	while let Some(msg) = receiver.next().await {
		let msg = if let Ok(msg) = msg {
			msg
		} else {
			// client disconnected
			return;
		};

		println!("got msg: {}", msg.to_text().unwrap());
	}
}

async fn write_socket(mut sender: SplitSink<WebSocket, TungsteniteMessage>)
{
	sender.send("hello world".into()).await.unwrap();
}
