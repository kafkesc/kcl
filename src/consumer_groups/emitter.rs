use std::collections::HashMap;

use rdkafka::{
    admin::AdminClient, client::DefaultClientContext, groups::GroupList,
    ClientConfig,
};
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinHandle,
    time::{interval, Duration},
};

use crate::internals::Emitter;

const CHANNEL_SIZE: usize = 1;
const SEND_TIMEOUT: Duration = Duration::from_millis(100);

const FETCH_TIMEOUT: Duration = Duration::from_millis(100);
const FETCH_INTERVAL: Duration = Duration::from_secs(1);

/// Holds Consumer Group Member information, like `client.id`, host and other client specifics.
///
/// TODO Dynamically update content by consuming [`konsumer_offsets::GroupMetadata`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Member {
    id: String,
    client_id: String,
    client_host: String,
}

/// Hold Consumer Group information, like memberships.
///
/// TODO Dynamically update content by consuming [`konsumer_offsets::GroupMetadata`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Group {
    name: String,
    members: HashMap<String, Member>,
    state: String,
    protocol: String,
    protocol_type: String,
}

/// Holds a map of all the known Consumer Groups, at a given point in time.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConsumerGroups {
    groups: HashMap<String, Group>,
}

impl From<GroupList> for ConsumerGroups {
    fn from(gl: GroupList) -> Self {
        let mut res = Self {
            groups: HashMap::with_capacity(gl.groups().len()),
        };

        for g in gl.groups() {
            let mut res_members = HashMap::with_capacity(g.members().len());

            for m in g.members() {
                res_members.insert(
                    m.id().to_string(),
                    Member {
                        id: m.id().to_string(),
                        client_id: m.client_id().to_string(),
                        client_host: m.client_host().to_string(),
                        ..Default::default()
                    },
                );
            }

            res.groups.insert(
                g.name().to_string(),
                Group {
                    name: g.name().to_string(),
                    members: res_members,
                    state: g.state().to_string(),
                    protocol: g.protocol().to_string(),
                    protocol_type: g.protocol_type().to_string(),
                    ..Default::default()
                },
            );
        }

        res
    }
}

pub struct ConsumerGroupsEmitter {
    admin_client_config: ClientConfig,
}

impl ConsumerGroupsEmitter {
    /// Create a new [`ConsumerGroupsEmitter`]
    ///
    /// # Arguments
    ///
    /// * `admin_client_config` - Kafka admin client configuration, used to fetch Consumer Groups
    pub fn new(admin_client_config: ClientConfig) -> Self {
        Self {
            admin_client_config,
        }
    }
}

impl Emitter for ConsumerGroupsEmitter {
    type Emitted = ConsumerGroups;

    fn spawn(
        &self,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) -> (mpsc::Receiver<Self::Emitted>, JoinHandle<()>) {
        let admin_client: AdminClient<DefaultClientContext> = self
            .admin_client_config
            .create()
            .expect("Failed to allocate Admin Client");

        let (sx, rx) = mpsc::channel::<Self::Emitted>(CHANNEL_SIZE);

        let join_handle = tokio::spawn(async move {
            let mut interval = interval(FETCH_INTERVAL);

            'outer: loop {
                let res_groups = admin_client
                    .inner()
                    .fetch_group_list(None, FETCH_TIMEOUT)
                    .map(Self::Emitted::from);

                match res_groups {
                    Ok(groups) => {
                        let ch_cap = sx.capacity();
                        if ch_cap == 0 {
                            warn!("Emitting channel saturated: receiver too slow?");
                        }

                        tokio::select! {
                            res = sx.send_timeout(groups, SEND_TIMEOUT) => {
                                if let Err(e) = res {
                                    error!("Failed to emit cluster status: {e}");
                                }
                            },

                            // Initiate shutdown: by letting this task conclude,
                            // the receiver will detect the channel is closing
                            // on the sender end, and conclude its own activity/task.
                            _ = shutdown_rx.recv() => {
                                info!("Received shutdown signal");
                                break 'outer;
                            },
                        }
                    },
                    Err(e) => {
                        error!("Failed to fetch consumer groups: {e}");
                    },
                }

                interval.tick().await;
            }
        });

        (rx, join_handle)
    }
}
