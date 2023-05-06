use std::net::SocketAddr;

use hyper::StatusCode;
use rustgram::service::IntoResponse;
use rustgram::{r, Request, Response, Router};
use rustgram_ws::ws::WebSocket;

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
