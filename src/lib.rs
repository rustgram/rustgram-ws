pub mod ws;
pub use tokio_tungstenite::tungstenite::Message;
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
