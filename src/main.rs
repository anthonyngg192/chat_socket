use std::env;

use async_std::net::TcpListener;
use chat_core::presence::presence_clear_region;

#[macro_use]
extern crate log;

pub mod config;

mod database;
mod websocket;

#[async_std::main]
async fn main() {
    chat_core::configure!();
    database::connect().await;

    presence_clear_region(None).await;

    let bind = env::var("HOST").unwrap_or_else(|_| "0.0.0.0:9000".into());
    info!("Listening on host {bind}");

    let try_socket = TcpListener::bind(bind).await;

    let listener = try_socket.expect("Failed to bind");
    while let Ok((stream, addr)) = listener.accept().await {
        websocket::spawn_client(database::get_db(), stream, addr);
    }
}
