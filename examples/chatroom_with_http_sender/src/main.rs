use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use futures_util::{SinkExt, StreamExt};
use hyper::StatusCode;
use rustgram::service::IntoResponse;
use rustgram::{r, Request, Response, RouteParams, Router};
use rustgram_ws::ws::WebSocket;
use rustgram_ws::Message;
use serde::Deserialize;
use tokio::sync::{broadcast, OnceCell, RwLock};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

struct RoomState
{
	/// Previously stored in AppState
	user_set: HashSet<String>,
	/// Previously created in main.
	tx: broadcast::Sender<String>,
}

impl RoomState
{
	fn new() -> Self
	{
		Self {
			// Track usernames per room rather than globally.
			user_set: HashSet::new(),
			// Create a new channel for every room
			tx: broadcast::channel(100).0,
		}
	}
}

static ROOMS: OnceCell<RwLock<HashMap<String, RoomState>>> = OnceCell::const_new();

#[tokio::main]
async fn main()
{
	tracing_subscriber::registry()
		.with(tracing_subscriber::EnvFilter::new(
			std::env::var("RUST_LOG").unwrap_or_else(|_| "example_chat=trace".into()),
		))
		.with(tracing_subscriber::fmt::layer())
		.init();

	ROOMS
		.get_or_init(|| async move { RwLock::new(HashMap::new()) })
		.await;

	let mut router = Router::new(not_found_handler);

	router.get("/websocket", r(websocket_handler));
	router.get("/http_sender/:room_id/:msg", r(http_sender));

	let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
	tracing::debug!("listening on {}", addr);

	//start the app
	rustgram::start(router, addr).await;
}

async fn not_found_handler(_req: Request) -> Response
{
	hyper::Response::builder()
		.status(StatusCode::NOT_FOUND)
		.body("Not found".into())
		.unwrap()
}

async fn http_sender(req: Request) -> &'static str
{
	let params = req.extensions().get::<RouteParams>().unwrap();

	let room_id = params.get("room_id").unwrap();
	let msg = params.get("msg").unwrap();

	let mut rooms = ROOMS.get().unwrap().write().await;
	let room = rooms
		.entry(room_id.to_string())
		.or_insert_with(RoomState::new);
	let tx = room.tx.clone();

	tx.send(msg.to_string()).unwrap();

	"Send"
}

async fn websocket_handler(req: Request) -> impl IntoResponse<Response>
{
	rustgram_ws::ws::on_upgrade(req, websocket)
}

async fn websocket(stream: WebSocket)
{
	// By splitting we can send and receive at the same time.
	let (mut sender, mut receiver) = stream.split();

	// Username gets set in the receive loop, if it's valid.

	// We have more state now that needs to be pulled out of the connect loop
	let mut tx = None::<broadcast::Sender<String>>;
	let mut username = String::new();
	let mut channel = String::new();

	// Loop until a text message is found.
	while let Some(Ok(message)) = receiver.next().await {
		if let Message::Text(name) = message {
			#[derive(Deserialize)]
			struct Connect
			{
				username: String,
				channel: String,
			}

			let connect: Connect = match serde_json::from_str(&name) {
				Ok(connect) => connect,
				Err(error) => {
					tracing::error!(%error);
					let _ = sender
						.send(Message::Text(String::from("Failed to parse connect message")))
						.await;
					break;
				},
			};

			// Scope to drop the mutex guard before the next await
			{
				// If username that is sent by client is not taken, fill username string.
				let mut rooms = ROOMS.get().unwrap().write().await;

				channel = connect.channel.clone();
				let room = rooms.entry(connect.channel).or_insert_with(RoomState::new);

				tx = Some(room.tx.clone());

				if !room.user_set.contains(&connect.username) {
					room.user_set.insert(connect.username.to_owned());
					username = connect.username.clone();
				}
			}

			// If not empty we want to quit the loop else we want to quit function.
			if tx.is_some() && !username.is_empty() {
				break;
			} else {
				// Only send our client that username is taken.
				let _ = sender
					.send(Message::Text(String::from("Username already taken.")))
					.await;

				return;
			}
		}
	}

	// We know if the loop exited `tx` is not `None`.
	let tx = tx.unwrap();
	// Subscribe before sending joined message.
	let mut rx = tx.subscribe();

	// Send joined message to all subscribers.
	let msg = format!("{} joined.", username);
	tracing::debug!("{}", msg);
	let _ = tx.send(msg);

	// This task will receive broadcast messages and send text message to our client.
	let mut send_task = tokio::spawn(async move {
		while let Ok(msg) = rx.recv().await {
			// In any websocket error, break loop.
			if sender.send(Message::Text(msg)).await.is_err() {
				break;
			}
		}
	});

	// We need to access the `tx` variable directly again, so we can't shadow it here.
	// I moved the task spawning into a new block so the original `tx` is still visible later.
	let mut recv_task = {
		// Clone things we want to pass to the receiving task.
		let tx = tx.clone();
		let name = username.clone();

		// This task will receive messages from client and send them to broadcast subscribers.
		tokio::spawn(async move {
			while let Some(Ok(Message::Text(text))) = receiver.next().await {
				// Add username before message.
				let _ = tx.send(format!("{}: {}", name, text));
			}
		})
	};

	// If any one of the tasks exit, abort the other.
	tokio::select! {
		_ = (&mut send_task) => recv_task.abort(),
		_ = (&mut recv_task) => send_task.abort(),
	}

	// Send user left message.
	let msg = format!("{} left.", username);
	tracing::debug!("{}", msg);
	let _ = tx.send(msg);
	let mut rooms = ROOMS.get().unwrap().write().await;

	// Remove username from map so new clients can take it.
	rooms.get_mut(&channel).unwrap().user_set.remove(&username);

	// TODO: Check if the room is empty now and remove the `RoomState` from the map.
}
