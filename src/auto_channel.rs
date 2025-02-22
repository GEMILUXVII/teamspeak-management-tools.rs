use crate::configure::Config;
use crate::configure::config::MutePorter;
use crate::observer::PrivateMessageRequest;
use crate::plugins::KVMap;
use crate::socketlib::SocketConn;
use crate::types::notifies::ClientBasicInfo;
use crate::types::{QueryResult, SafeUserState};
use crate::{AUTO_CHANNEL_NICKNAME_OVERRIDE, DEFAULT_AUTO_CHANNEL_NICKNAME};
use anyhow::anyhow;
use log::{debug, error, info, trace, warn};
use std::time::Duration;
use tap::TapFallible;
use tokio::sync::mpsc;

pub enum AutoChannelEvent {
    Update(ClientBasicInfo),
    DeleteChannel(i64, String),
    ShouldRefresh,
    Terminate,
}

#[derive(Clone, Debug)]
pub struct AutoChannelInstance {
    channel_ids: Vec<i64>,
    sender: Option<mpsc::Sender<AutoChannelEvent>>,
}

impl AutoChannelInstance {
    pub async fn send_terminate(&self) -> anyhow::Result<()> {
        self.send_signal(AutoChannelEvent::Terminate)
            .await
            .map(|_| ())
    }

    async fn send_signal(&self, signal: AutoChannelEvent) -> anyhow::Result<bool> {
        match self.sender {
            Some(ref sender) => sender
                .send(signal)
                .await
                .map_err(|_| anyhow!("Got error while send event to auto channel staff"))
                .map(|_| true),
            _ => Ok(false),
        }
    }

    pub async fn send_delete(&self, user_id: i64, uid: String) -> anyhow::Result<bool> {
        self.send_signal(AutoChannelEvent::DeleteChannel(user_id, uid))
            .await
    }

    pub async fn send(&self, view: ClientBasicInfo) -> anyhow::Result<bool> {
        if self.sender.is_none() {
            return Ok(false);
        }
        if !self.channel_ids.iter().any(|id| id == &view.channel_id()) {
            self.send_signal(AutoChannelEvent::ShouldRefresh).await?;
            return Ok(false);
        }
        self.send_signal(AutoChannelEvent::Update(view)).await
    }

    pub fn new(channel_ids: Vec<i64>, sender: Option<mpsc::Sender<AutoChannelEvent>>) -> Self {
        Self {
            channel_ids,
            sender,
        }
    }

    pub fn valid(&self) -> bool {
        self.sender.is_some()
    }
}

pub async fn mute_porter_function(
    conn: &mut SocketConn,
    mute_porter: &MutePorter,
    thread_id: &str,
) -> QueryResult<()> {
    for client in conn
        .query_clients()
        .await
        .map_err(|e| anyhow!("Unable query clients: {e:?}"))?
    {
        if client.client_is_user()
            && client.channel_id() == mute_porter.monitor_channel()
            && !mute_porter.check_whitelist(client.client_database_id())
        {
            if let Some(true) = conn
                .query_client_info(client.client_id())
                .await
                .inspect_err(|e| error!("[{thread_id}] Unable query client information: {e:?}",))
                .ok()
                .flatten()
                .map(|r| r.is_client_muted())
            {
                conn.move_client(client.client_id(), mute_porter.target_channel())
                    .await
                    .inspect_err(|e| {
                        error!(
                            "[{thread_id}] Unable move client {} to channel {}: {e:?}",
                            client.client_id(),
                            mute_porter.target_channel(),
                        )
                    })
                    .map(|_| {
                        info!(
                            "[{thread_id}] Moved {} to {}",
                            client.client_id(),
                            mute_porter.target_channel()
                        )
                    })
                    .ok();
            }
        }
    }
    Ok(())
}

fn build_redis_key(client_database_id: i64, server_id: &str, channel_id: i64) -> String {
    format!(
        "ts_autochannel_{client_database_id}_{server_id}_{pid}",
        pid = channel_id
    )
}

pub async fn auto_channel_staff(
    mut conn: SocketConn,
    mut receiver: mpsc::Receiver<AutoChannelEvent>,
    private_message_sender: mpsc::Sender<PrivateMessageRequest>,
    config: Config,
    thread_id: String,
    mut kv_map: Box<dyn KVMap>,
    user_map: SafeUserState,
) -> anyhow::Result<()> {
    let monitor_channels = config.server().channels();
    let privilege_group = config.server().privilege_group_id();
    let channel_permissions = config.channel_permissions();
    let moved_message = config.message().move_to_channel();
    conn.change_nickname(
        AUTO_CHANNEL_NICKNAME_OVERRIDE.get_or_init(|| DEFAULT_AUTO_CHANNEL_NICKNAME.to_string()),
    )
    .await
    .map_err(|e| anyhow!("Got error while change nickname: {e:?}"))?;

    let who_am_i = conn
        .who_am_i()
        .await
        .map_err(|e| anyhow!("Whoami failed: {e:?}"))?;

    let server_info = conn
        .query_server_info()
        .await
        .map_err(|e| anyhow!("Query server info error: {e:?}"))?;

    info!("[{thread_id}] Connected: {}", who_am_i.client_id());
    debug!("[{thread_id}] Monitor: {}", monitor_channels.len());

    let mut should_refresh = false;
    let mut skip_sleep = true;
    loop {
        if !skip_sleep {
            //std::thread::sleep(Duration::from_millis(interval));
            match tokio::time::timeout(Duration::from_secs(30), receiver.recv()).await {
                Ok(Some(event)) => match event {
                    AutoChannelEvent::Terminate => break,
                    AutoChannelEvent::Update(view) => {
                        if view.client_id() == who_am_i.client_id() {
                            continue;
                        }
                    }
                    AutoChannelEvent::DeleteChannel(client_id, uid) => {
                        let result = conn
                            .client_get_database_id_from_uid(&uid)
                            .await
                            .map_err(|e| anyhow!("Got error while query {uid} {e:?}",))?;
                        for channel_id in &monitor_channels {
                            let key = build_redis_key(
                                result.client_database_id(),
                                server_info.virtual_server_unique_identifier(),
                                *channel_id,
                            );

                            kv_map
                                .delete(key)
                                .await
                                .tap_ok(|_| trace!("[{thread_id}] Deleted"))
                                .inspect_err(|e| {
                                    error!("[{thread_id}] Got error while delete from redis: {e:?}")
                                })
                                .ok();
                        }
                        private_message_sender
                            .send(PrivateMessageRequest::Message(
                                client_id,
                                "Received.".into(),
                            ))
                            .await
                            .inspect_err(|_| {
                                error!("[{thread_id}] Got error in request send message")
                            })
                            .ok();
                    }
                    AutoChannelEvent::ShouldRefresh => {
                        should_refresh = true;
                    }
                },
                Ok(None) => {
                    error!("[{thread_id}] Channel closed!");
                    break;
                }
                Err(_) => {
                    conn.who_am_i()
                        .await
                        .inspect_err(|e| {
                            error!("[{thread_id}] Got error while doing keep alive {e:?}")
                        })
                        .ok();
                    if config.mute_porter().enable() {
                        mute_porter_function(&mut conn, config.mute_porter(), &thread_id).await?;
                    }
                    if !should_refresh {
                        continue;
                    }
                }
            }
        } else {
            skip_sleep = false;
        }
        let Ok(clients) = conn
            .query_clients()
            .await
            .inspect_err(|e| error!("[{thread_id}] Got error while query clients: {e:?}"))
        else {
            continue;
        };

        'outer: for client in &clients {
            if client.client_database_id() == who_am_i.client_database_id()
                || !monitor_channels.iter().any(|v| *v == client.channel_id())
                || client.client_type() == 1
            {
                continue;
            }
            // TODO: May need add thread id
            let key = format!(
                "ts_autochannel_{}_{server_id}_{pid}",
                client.client_database_id(),
                server_id = server_info.virtual_server_unique_identifier(),
                pid = client.channel_id()
            );

            let ret: Option<i64> = kv_map
                .get(key.clone())
                .await?
                .map(|v| v.parse())
                .transpose()
                .inspect_err(|e| error!("[{thread_id}] Unable to parse result: {e:?}"))
                .ok()
                .flatten();
            let create_new = ret.is_none();
            let target_channel = if create_new {
                let mut name = format!("{}'s channel", client.client_nickname());
                let channel_id = loop {
                    let create_channel = match conn.create_channel(&name, client.channel_id()).await
                    {
                        Ok(Some(ret)) => ret.cid(),
                        Err(e) => {
                            if e.code() == 771 {
                                name.push('1');
                                continue;
                            }
                            error!("[{thread_id}] Got error while create {name:?} channel: {e:?}",);
                            continue 'outer;
                        }
                        _ => unreachable!(),
                    };

                    break create_channel;
                };

                conn.set_client_channel_group(
                    client.client_database_id(),
                    channel_id,
                    privilege_group,
                )
                .await
                .inspect_err(|e| {
                    error!("[{thread_id}] Got error while set client channel group: {e:?}",)
                })
                .ok();

                conn.add_channel_permission(channel_id, &[(133, 75)])
                    .await
                    .inspect_err(|e| {
                        error!(
                            "[{thread_id}] Got error while set default channel permissions: {e:?}",
                        )
                    })
                    .ok();

                if let Some(permissions) = channel_permissions.get(&client.channel_id()) {
                    conn.add_channel_permission(channel_id, permissions)
                        .await
                        .inspect_err(|e| {
                            error!("[{thread_id}] Got error while set channel permissions: {e:?}",)
                        })
                        .ok();
                }

                channel_id
            } else {
                ret.unwrap()
            };

            if let Err(e) = conn.move_client(client.client_id(), target_channel).await {
                if e.code() == 768 {
                    kv_map.delete(key.clone()).await?;
                    skip_sleep = true;
                    continue;
                }
                error!("[{thread_id}] Got error while move client: {e:?}");
                continue;
            };

            private_message_sender
                .send(PrivateMessageRequest::Message(
                    client.client_id(),
                    moved_message.clone().into(),
                ))
                .await
                .inspect_err(|_| warn!("[{thread_id}] Send message request fail"))
                .ok();

            if create_new {
                conn.move_client(who_am_i.client_id(), client.channel_id())
                    .await
                    .map_err(|e| anyhow!("Unable move self out of channel. {e:?}"))?;
                kv_map.set(key.clone(), target_channel.to_string()).await?;
            }

            info!(
                "[{thread_id}] Move {} to {target_channel}",
                client.client_nickname(),
            );
        }

        if !user_map.enabled() {
            continue;
        }
        if let Ok(channels) = conn.query_channels().await {
            user_map.update(channels, clients).await;
        }
        should_refresh = false;
    }
    conn.logout().await?;
    Ok(())
}
