use std::{
    any::type_name,
    collections::{hash_map::Entry, HashMap},
    sync::Arc,
    time::Duration as StdDuration,
};

use chrono::{DateTime, Duration, Utc};
use konsumer_offsets::{GroupMetadata, KonsumerOffsetsData, OffsetCommit};
use log::Level::Trace;
use tokio::{
    sync::{mpsc, RwLock},
    time::interval,
};

use crate::constants::KOMMITTED_CONSUMER_OFFSETS_CONSUMER;
use crate::consumer_groups::ConsumerGroupsRegister;
use crate::internals::Awaitable;
use crate::kafka_types::{Group, Member, TopicPartition};
use crate::partition_offsets::PartitionOffsetsRegister;

const RECONCILE_INTERVAL: StdDuration = StdDuration::from_secs(1);
const LAG_STALE_AFTER: Duration = Duration::seconds(5);

/// Describes the "lag" (or "latency"), and it's usually paired with a Consumer [`GroupWithMembers`].
///
/// Additionally, it carries the "context" of the lag, including the offsets like the one
/// it was measured against, the earliest and the latest (tracked and available).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lag {
    /// Offset that a given Consumer [`GroupWithMembers`] is at when consuming a specific [`TopicPartition`], at the given `offset_timestamp`.
    pub(crate) offset: u64,

    /// [`DateTime<Utc>`] that the `offset` was consumed by the Consumer Group.
    pub(crate) offset_timestamp: DateTime<Utc>,

    /// Last [`DateTime<Utc>`] when this [`Self`] was updated.
    ///
    /// When [`Self`] gets updated by the Consumer committed offset information, this and
    /// `offset_timestamp` have the same value. But if the Consumer stalls for whatever reason,
    /// and [`Self`] gets stale, this fields starts drifting forward.
    pub(crate) timestamp: DateTime<Utc>,

    /// Lag in consuming a specific [`TopicPartition`] as reported by the Consumer [`GroupWithMembers`].
    ///
    /// It is the numeric distance between the last offset consumed by a group,
    /// and the high watermark (end offset) of the [`TopicPartition`] was produced.
    pub(crate) offset_lag: u64,

    /// Estimated time latency between the Consumer [`GroupWithMembers`] consuming a specific [`TopicPartition`],
    /// and the [`DateTime<Utc>`] when the high watermark (end offset) was produced.
    pub(crate) time_lag: Duration,
}

impl Lag {
    /// Returns `true` when last time this [`Self`] was updated via Consumer committed offset information
    /// was longer than [`LAG_STALE_AFTER`] ago.
    fn is_stale(&self) -> bool {
        Utc::now() - self.offset_timestamp > LAG_STALE_AFTER
    }
}

impl Default for Lag {
    fn default() -> Self {
        Lag {
            offset: 0,
            offset_timestamp: DateTime::<Utc>::default(),
            offset_lag: 0,
            time_lag: Duration::zero(),
            timestamp: DateTime::<Utc>::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct LagWithOwner {
    pub(crate) lag: Option<Lag>,
    pub(crate) owner: Option<Member>,
}

/// Describes the "lag" (or "latency") of a specific Consumer [`GroupWithMembers`] in respect to a collection of [`TopicPartition`] that it consumes.
#[derive(Debug, Clone, Default)]
pub struct GroupWithLag {
    pub(crate) group: Group,
    // TODO https://github.com/kafkesc/kommitted/issues/58
    pub(crate) lag_by_topic_partition: HashMap<TopicPartition, LagWithOwner>,
}

#[derive(Debug)]
pub struct LagRegister {
    pub(crate) lag_by_group: Arc<RwLock<HashMap<String, GroupWithLag>>>,
}

impl LagRegister {
    pub fn new(
        mut kod_rx: mpsc::Receiver<KonsumerOffsetsData>,
        cg_reg: Arc<ConsumerGroupsRegister>,
        po_reg: Arc<PartitionOffsetsRegister>,
    ) -> Self {
        let lr = LagRegister {
            lag_by_group: Arc::new(RwLock::new(HashMap::default())),
        };

        let lag_by_group_clone = lr.lag_by_group.clone();

        tokio::spawn(async move {
            // Processing Consumer Groups Register to populate this register for the first time
            process_consumer_groups(cg_reg.clone(), lag_by_group_clone.clone()).await;
            let mut cg_reg_curr_hash = cg_reg.get_hash().await;

            // Setup interval for "reconciling" this Register with the Consumer Group Register
            // and doing other necessary internal updates (e.g. lag).
            let mut reconcile_timeout = interval(RECONCILE_INTERVAL);

            loop {
                tokio::select! {
                    Some(kod) = kod_rx.recv() => {
                        match kod {
                            KonsumerOffsetsData::OffsetCommit(oc) => {
                                trace!(
                                    "Processing {} of Group '{}' for Topic Partition '{}:{}'",
                                    type_name::<OffsetCommit>(),
                                    oc.group,
                                    oc.topic,
                                    oc.partition
                                );
                                process_offset_commit(oc, lag_by_group_clone.clone(), po_reg.clone()).await;
                            },
                            KonsumerOffsetsData::GroupMetadata(gm) => {
                                trace!(
                                    "Processing {} of Group '{}' with {} Members",
                                    type_name::<GroupMetadata>(),
                                    gm.group,
                                    gm.members.len()
                                );
                                process_group_metadata(gm, lag_by_group_clone.clone()).await;
                            }
                        }
                    },
                    _ = reconcile_timeout.tick() => {
                        // Update internal Map of Groups if the ConsumerGroupsRegister has changed:
                        // we do that by keeping track of the register "hash".
                        let cg_reg_latest_hash = cg_reg.get_hash().await;
                        if cg_reg_curr_hash != cg_reg_latest_hash {
                            trace!(
                                "Processing updated {} (hash {} != {})",
                                type_name::<ConsumerGroupsRegister>(),
                                cg_reg_curr_hash,
                                cg_reg_latest_hash
                            );
                            process_consumer_groups(cg_reg.clone(), lag_by_group_clone.clone()).await;
                            cg_reg_curr_hash = cg_reg_latest_hash;
                        }

                        // Update stale Lags for all touples (Topic, Partition, Group) known to this register
                        update_stale_lags(lag_by_group_clone.clone(), po_reg.clone()).await;
                    },
                    else => {
                        info!("Emitters stopping: breaking (internal) loop");
                        break;
                    }
                }

                if log_enabled!(Trace) {
                    let r_guard = lag_by_group_clone.read().await;
                    for (name, gwl) in r_guard.iter() {
                        trace!(
                            "Group {} has Lag info for {} partitions: {} Lags, {} Owners",
                            name,
                            gwl.lag_by_topic_partition.len(),
                            gwl.lag_by_topic_partition.iter().filter(|x| x.1.lag.is_some()).count(),
                            gwl.lag_by_topic_partition
                                .iter()
                                .filter(|x| x.1.owner.is_some())
                                .count(),
                        );
                    }
                }
            }
        });

        lr
    }
}

async fn process_consumer_groups(
    cg_reg: Arc<ConsumerGroupsRegister>,
    lag_register_groups: Arc<RwLock<HashMap<String, GroupWithLag>>>,
) {
    let known_groups = cg_reg.get_groups().await;

    // First, Loop over list of known Groups, and update `lag_register_groups`
    for group_name in &known_groups {
        if let Some(group_with_members) = cg_reg.get_group(group_name).await {
            let mut w_guard = lag_register_groups.write().await;

            // Organise all the Group Members by the TopicPartition they own
            let members_by_topic_partition = group_with_members
                .members
                .into_iter()
                .flat_map(|(_, mwa)| {
                    mwa.assignment.into_iter().map(|tp| (tp, mwa.member.clone())).collect::<HashMap<
                        TopicPartition,
                        Member,
                    >>(
                    )
                })
                .collect::<HashMap<TopicPartition, Member>>();

            // Insert or update "group name -> group with lag" map entries
            if let Entry::Vacant(e) = w_guard.entry(group_name.clone()) {
                // Insert
                e.insert(GroupWithLag {
                    group: group_with_members.group,
                    // Given this is a new Group,
                    lag_by_topic_partition: members_by_topic_partition
                        .into_iter()
                        .map(|(tp, m)| {
                            (
                                tp,
                                LagWithOwner {
                                    owner: Some(m),
                                    ..Default::default()
                                },
                            )
                        })
                        .collect(),
                });
            } else {
                // Update
                let gwl = w_guard.get_mut(group_name).unwrap_or_else(|| {
                    panic!(
                        "{} for {:#?} could not be found (fatal)",
                        type_name::<GroupWithLag>(),
                        group_name
                    )
                });

                // Set the Group (probably unchanged)
                gwl.group = group_with_members.group;

                // Remove from map of LagWithOwner the entries with key TopicPartition not owned by any member of this group
                gwl.lag_by_topic_partition
                    .retain(|tp, _| members_by_topic_partition.contains_key(tp));

                // Create or Update an entries `TopicPartition -> LagWithOwner`:
                // either update the owner Member of an existing one,
                // or create a new entry with no Lag set.
                for (tp, m) in members_by_topic_partition.into_iter() {
                    gwl.lag_by_topic_partition
                        .entry(tp)
                        .and_modify(|lwo| lwo.owner = Some(m.clone()))
                        .or_insert_with(|| LagWithOwner {
                            owner: Some(m),
                            ..Default::default()
                        });
                }
            };
        }
    }

    // ... then, remove groups that are in `lag_register_groups` but are not known (anymore)
    lag_register_groups.write().await.retain(|g, _| known_groups.contains(g));
}

async fn update_stale_lags(
    lag_register_groups: Arc<RwLock<HashMap<String, GroupWithLag>>>,
    po_reg: Arc<PartitionOffsetsRegister>,
) {
    // Loop over all the existing Lag data we have
    for (g, group_wl) in lag_register_groups.write().await.iter_mut() {
        for (tp, lag_wo) in group_wl.lag_by_topic_partition.iter_mut() {
            if let Some(curr_lag) = &mut lag_wo.lag {
                // Only proceed to update the lag, if it is stale
                if !curr_lag.is_stale() {
                    break;
                }

                // Fetch the latest produced offset we know about for this Topic-Partition
                let latest_offset = match po_reg.get_latest_tracked_offset(tp).await {
                    Ok(latest_offset) => latest_offset,
                    Err(e) => {
                        error!("Failed to get latest tracked offset for Partition '{}': {}", tp, e);
                        break;
                    },
                };

                // Estimate new offset lag, considering the latest partition offset
                curr_lag.offset_lag =
                    po_reg.estimate_offset_lag(tp, curr_lag.offset).await.unwrap_or_else(|e| {
                        debug!(
                            "Failed to estimate Offset Lag of Group '{}' for Partition '{}': {}",
                            g, tp, e
                        );
                        0
                    });

                // Estimate new time lag considering the time when the latest partition offset was produced
                curr_lag.time_lag = po_reg.estimate_time_lag(tp, curr_lag.offset, latest_offset.at).await.unwrap_or_else(|e| {
                    debug!(
                            "Failed to estimate Time Lag of Group '{}' for Topic Partition '{}': {}",
                           g, tp, e
                        );
                    Duration::zero()
                });

                // Store last time we updated this lag
                curr_lag.timestamp = Utc::now();
            }
        }
    }
}

async fn process_offset_commit(
    oc: OffsetCommit,
    lag_register_groups: Arc<RwLock<HashMap<String, GroupWithLag>>>,
    po_reg: Arc<PartitionOffsetsRegister>,
) {
    let mut w_guard = lag_register_groups.write().await;
    match w_guard.get_mut(&oc.group) {
        Some(gwl) => {
            let tp = TopicPartition::new(oc.topic, oc.partition as u32);

            // Prepare all the Lag fields
            let l = Lag {
                offset: oc.offset as u64,
                offset_timestamp: oc.commit_timestamp,
                timestamp: oc.commit_timestamp,
                offset_lag: po_reg.estimate_offset_lag(&tp, oc.offset as u64)
                    .await
                    .unwrap_or_else(|e| {
                        debug!(
                            "Failed to estimate Offset Lag of Group '{}' for Topic Partition '{}': {}",
                            oc.group, tp, e
                        );
                        0
                    }),
                time_lag: po_reg
                    .estimate_time_lag(&tp, oc.offset as u64, oc.commit_timestamp)
                    .await
                    .unwrap_or_else(|e| {
                        debug!(
                            "Failed to estimate Time Lag of Group '{}' for Topic Partition '{}': {}",
                            oc.group, tp, e
                        );
                        Duration::zero()
                    }),
            };

            // Create or update entry `TopicPartition -> LagWithOwner`:
            // either update the Lag of an existing one,
            // or create a new entry with no owner set.
            gwl.lag_by_topic_partition
                .entry(tp)
                .and_modify(|lwo| lwo.lag = Some(l.clone()))
                .or_insert_with(|| LagWithOwner {
                    lag: Some(l),
                    owner: None,
                });
        },
        None if oc.group != KOMMITTED_CONSUMER_OFFSETS_CONSUMER => {
            warn!(
                "Received {} about unknown Group '{}': ignoring",
                type_name::<OffsetCommit>(),
                oc.group
            );
        },
        None => (),
    }
}

async fn process_group_metadata(
    gm: GroupMetadata,
    lag_register_groups: Arc<RwLock<HashMap<String, GroupWithLag>>>,
) {
    // Ignore event if the Group is empty: this usually means that the Group is gone (i.e. all
    // Consumers in the Group are gone), and we don't want to lose the lag information just yet.
    // The Group will be "forgotten" once the `ConsumerGroupRegister` says so.
    if gm.members.is_empty() {
        debug!("Ignoring {:?}: no Members", gm);
        return;
    }

    let mut w_guard = lag_register_groups.write().await;
    match w_guard.get_mut(&gm.group) {
        Some(gwl) => {
            // New map of Topic Partition->Member (owner), that the Group is consuming
            let new_tp_to_owner = gm
                .members
                .into_iter()
                .flat_map(|m| {
                    let owner = Member {
                        id: m.id,
                        client_id: m.client_id,
                        client_host: m.client_host,
                    };

                    // Collect all Group Coordinator Assigned Topic Partitions
                    let assignment_tps = m
                        .assignment
                        .assigned_topic_partitions
                        .into_iter()
                        .flat_map(TopicPartition::vec_from)
                        .map(|tp| (tp, owner.clone()))
                        .collect::<HashMap<TopicPartition, Member>>();

                    // Collect all Group Subscribed Topic Partitions
                    let subscription_tps = m
                        .subscription
                        .owned_topic_partitions
                        .into_iter()
                        .flat_map(TopicPartition::vec_from)
                        .map(|tp| (tp, owner.clone()))
                        .collect::<HashMap<TopicPartition, Member>>();

                    assignment_tps
                        .into_iter()
                        .chain(subscription_tps)
                        .collect::<HashMap<TopicPartition, Member>>()
                })
                .collect::<HashMap<TopicPartition, Member>>();

            // Keep a Topic-Partition Lag for this Group, only if it was in the GroupMetadata.
            //
            // NOTE: The new ones that are NOT YET in the map, will be added when an
            // OffsetCommit for this Group and this Topic-Partition is received and Lag calculated.
            gwl.lag_by_topic_partition.retain(|tp, _| new_tp_to_owner.contains_key(tp));

            // For all the Topic-Partition in the GroupMetadata, set the Member that owns it
            for (tp, owner) in new_tp_to_owner.into_iter() {
                if let Some(lwo) = gwl.lag_by_topic_partition.get_mut(&tp) {
                    lwo.owner = Some(owner)
                }
            }
        },
        None if gm.group != KOMMITTED_CONSUMER_OFFSETS_CONSUMER => {
            warn!(
                "Received {} about unknown Group '{}': ignoring",
                type_name::<GroupMetadata>(),
                gm.group
            );
        },
        None => (),
    }
}

impl Awaitable for LagRegister {
    async fn is_ready(&self) -> bool {
        // TODO https://github.com/kafkesc/kommitted/issues/59
        !self.lag_by_group.read().await.is_empty()
    }
}
