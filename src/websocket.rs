use std::net::SocketAddr;

use futures::{channel::oneshot, pin_mut, select, FutureExt, SinkExt, StreamExt, TryStreamExt};
use core::{
    events::{
        client::EventV1,
        server::ClientMessage,
        state::{State, SubscriptionStateChange},
    },
    models::{user::UserHint, User},
    presence::{presence_create_session, presence_delete_session},
    redis_kiss, Database,
};

use async_std::{net::TcpStream, sync::Mutex, task};

use crate::config::WebsocketHandshakeCallback;

pub fn spawn_client(db: &'static Database, stream: TcpStream, addr: SocketAddr) {
    task::spawn(async move {
        info!("User connected from {addr:?}");

        let (sender, receiver) = oneshot::channel();

        if let Ok(ws) = async_tungstenite::accept_hdr_async_with_config(
            stream,
            WebsocketHandshakeCallback::from(sender),
            None,
        )
        .await
        {
            if let Ok(mut config) = receiver.await {
                info! {
                    "User {addr:?} provided protocol configuration (version = {}, format = {:?})",
                    config.get_protocol_version(),
                    config.get_protocol_format()
                };

                let (write, mut read) = ws.split();
                let write = Mutex::new(write);

                if config.get_session_token().is_none() {
                    'outer: while let Ok(message) = read.try_next().await {
                        if let Ok(ClientMessage::Authenticate { token }) =
                            config.decode(message.as_ref().unwrap())
                        {
                            config.set_session_token(token);
                            break 'outer;
                        }
                    }
                }

                if let Some(token) = config.get_session_token().as_ref() {
                    match User::from_token(db, token, UserHint::Any).await {
                        Ok(user) => {
                            info!("User {addr:?} authenticated as @{}", user.username);
                            // Create local state.
                            let mut state = State::from(user);
                            let user_id = state.cache.user_id.clone();

                            // Create presence session.
                            let (first_session, session_id) =
                                presence_create_session(&user_id, 0).await;

                            // Notify socket we have authenticated.
                            write
                                .lock()
                                .await
                                .send(config.encode(&EventV1::Authenticated))
                                .await
                                .ok();

                            if let Ok(ready_payload) = state.generate_ready_payload(db).await {
                                write
                                    .lock()
                                    .await
                                    .send(config.encode(&ready_payload))
                                    .await
                                    .ok();

                                if first_session {
                                    state.broadcast_presence_change(true).await;
                                }

                                let listener = async{
                                    if let Ok(mut conn) = redis_kiss::open_pubsub_connection().await{
                                        loop {
                                            match state.apply_state() {
                                                SubscriptionStateChange::Reset => {
                                                    for id in state.iter_subscriptions() {
                                                        conn.subscribe(id).await.unwrap();
                                                    }

                                                    #[cfg(debug_assertions)]
                                                    info!("{addr:?} has reset their subscriptions");
                                                }
                                                SubscriptionStateChange::Change { add, remove } => {
                                                    for id in remove {
                                                        #[cfg(debug_assertions)]
                                                        info!("{addr:?} unsubscribing from {id}");

                                                        conn.unsubscribe(id).await.unwrap();
                                                    }

                                                    for id in add {
                                                        #[cfg(debug_assertions)]
                                                        info!("{addr:?} subscribing to {id}");

                                                        conn.subscribe(id).await.unwrap();
                                                    }
                                                }
                                                SubscriptionStateChange::None => {}
                                            }

                                            match conn.on_message().next().await.map(|item| {
                                                (
                                                    item.get_channel_name().to_string(),
                                                    redis_kiss::decode_payload::<EventV1>(&item),
                                                )
                                            }) {
                                                Some((channel, item)) => {
                                                    if let Ok(mut event) = item {
                                                        if state
                                                            .handle_incoming_event_v1(
                                                                db, &mut event,
                                                            )
                                                            .await
                                                            && write.lock().await
                                                                .send(config.encode(&event))
                                                                .await
                                                                .is_err()
                                                        {
                                                            break;
                                                        }
                                                    } else {
                                                        warn!("Failed to deserialise an event for {channel}!");
                                                    }
                                                }
                                                // No more data, assume we disconnected or otherwise
                                                // something bad occurred, so disconnect user.
                                                None => break,
                                            }
                                        }
                                        }
                                    }.fuse();

                                // Read from WebSocket stream.
                                let worker =
                                    async {
                                        while let Ok(Some(msg)) = read.try_next().await {
                                            if let Ok(payload) = config.decode(&msg) {
                                                match payload {
                                                    ClientMessage::BeginTyping { channel } => {
                                                        EventV1::ChannelStartTyping {
                                                            id: channel.clone(),
                                                            user: user_id.clone(),
                                                        }
                                                        .p(channel.clone())
                                                        .await;
                                                    }
                                                    ClientMessage::EndTyping { channel } => {
                                                        EventV1::ChannelStopTyping {
                                                            id: channel.clone(),
                                                            user: user_id.clone(),
                                                        }
                                                        .p(channel.clone())
                                                        .await;
                                                    }
                                                    ClientMessage::Ping { data, responded } => {
                                                        if responded.is_none() {
                                                            write
                                                                .lock()
                                                                .await
                                                                .send(config.encode(
                                                                    &EventV1::Pong { data },
                                                                ))
                                                                .await
                                                                .ok();
                                                        }
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
                                    }
                                    .fuse();

                                // Pin both tasks.
                                pin_mut!(listener, worker);

                                // Wait for either disconnect or for listener to die.
                                select!(
                                    () = listener => {},
                                    () = worker => {}
                                );

                                // Clean up presence session.
                            }
                            let last_session = presence_delete_session(&user_id, session_id).await;

                            // If this was the last session, notify other users that we just went offline.
                            if last_session {
                                state.broadcast_presence_change(false).await;
                            }
                        }
                        Err(err) => {
                            write.lock().await.send(config.encode(&err)).await.ok();
                        }
                    }
                }
            }
        }

        info!("User disconnected from {addr:?}");
    });
}
