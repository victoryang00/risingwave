// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use anyhow::{anyhow, Context};
use itertools::Itertools;
use risingwave_common::catalog::TableId;
use risingwave_common::types::ParallelUnitId;
use risingwave_common::{bail, try_match_expand};
use risingwave_connector::source::SplitImpl;
use risingwave_pb::common::{Buffer, ParallelUnit, ParallelUnitMapping, WorkerNode};
use risingwave_pb::meta::subscribe_response::{Info, Operation};
use risingwave_pb::meta::table_fragments::actor_status::ActorState;
use risingwave_pb::meta::table_fragments::{ActorStatus, State};
use risingwave_pb::stream_plan::stream_node::NodeBody;
use risingwave_pb::stream_plan::{Dispatcher, FragmentType, StreamActor, StreamNode};
use tokio::sync::{RwLock, RwLockReadGuard};

use crate::barrier::Reschedule;
use crate::manager::cluster::WorkerId;
use crate::manager::{commit_meta, MetaSrvEnv};
use crate::model::{
    ActorId, BTreeMapTransaction, FragmentId, MetadataModel, TableFragments, ValTransaction,
};
use crate::storage::{MetaStore, Transaction};
use crate::stream::{actor_mapping_to_parallel_unit_mapping, SplitAssignment};
use crate::MetaResult;

pub struct FragmentManagerCore {
    table_fragments: BTreeMap<TableId, TableFragments>,
}

impl FragmentManagerCore {
    /// List all fragment vnode mapping info.
    pub fn all_fragment_mappings(&self) -> impl Iterator<Item = ParallelUnitMapping> + '_ {
        self.table_fragments.values().flat_map(|table_fragments| {
            table_fragments.fragments.values().map(|fragment| {
                let parallel_unit_mapping = fragment
                    .vnode_mapping
                    .as_ref()
                    .expect("no data distribution found");
                ParallelUnitMapping {
                    fragment_id: fragment.fragment_id,
                    original_indices: parallel_unit_mapping.original_indices.clone(),
                    data: parallel_unit_mapping.data.clone(),
                }
            })
        })
    }

    pub fn all_internal_tables(&self) -> impl Iterator<Item = &u32> + '_ {
        self.table_fragments.values().flat_map(|table_fragments| {
            table_fragments
                .fragments
                .values()
                .flat_map(|fragment| fragment.state_table_ids.iter())
        })
    }
}

/// `FragmentManager` stores definition and status of fragment as well as the actors inside.
pub struct FragmentManager<S: MetaStore> {
    env: MetaSrvEnv<S>,

    core: RwLock<FragmentManagerCore>,
}

pub struct ActorInfos {
    /// node_id => actor_ids
    pub actor_maps: HashMap<WorkerId, Vec<ActorId>>,

    /// all reachable source actors
    pub source_actor_maps: HashMap<WorkerId, Vec<ActorId>>,
}

pub struct FragmentVNodeInfo {
    /// actor id => parallel unit
    pub actor_parallel_unit_maps: BTreeMap<ActorId, ParallelUnit>,

    /// fragment vnode mapping info
    pub vnode_mapping: Option<ParallelUnitMapping>,
}

#[derive(Default)]
pub struct BuildGraphInfo {
    pub table_sink_actor_ids: HashMap<TableId, Vec<ActorId>>,
}

pub type FragmentManagerRef<S> = Arc<FragmentManager<S>>;

impl<S: MetaStore> FragmentManager<S>
where
    S: MetaStore,
{
    pub async fn new(env: MetaSrvEnv<S>) -> MetaResult<Self> {
        let table_fragments = try_match_expand!(
            TableFragments::list(env.meta_store()).await,
            Ok,
            "TableFragments::list fail"
        )?;

        let table_fragments = table_fragments
            .into_iter()
            .map(|tf| (tf.table_id(), tf))
            .collect();

        Ok(Self {
            env,
            core: RwLock::new(FragmentManagerCore { table_fragments }),
        })
    }

    pub async fn get_fragment_read_guard(&self) -> RwLockReadGuard<'_, FragmentManagerCore> {
        self.core.read().await
    }

    pub async fn list_table_fragments(&self) -> MetaResult<Vec<TableFragments>> {
        let map = &self.core.read().await.table_fragments;

        Ok(map.values().cloned().collect())
    }

    pub async fn batch_update_table_fragments(
        &self,
        table_fragments: &[TableFragments],
    ) -> MetaResult<()> {
        let map = &mut self.core.write().await.table_fragments;
        if table_fragments
            .iter()
            .any(|tf| !map.contains_key(&tf.table_id()))
        {
            bail!("update table fragments fail, table not found");
        }

        let mut table_fragments_txn = BTreeMapTransaction::new(map);
        table_fragments.iter().for_each(|tf| {
            table_fragments_txn.insert(tf.table_id(), tf.clone());
        });
        commit_meta!(self, table_fragments_txn)?;

        for table_fragment in table_fragments {
            self.notify_fragment_mapping(table_fragment, Operation::Update)
                .await;
        }

        Ok(())
    }

    async fn notify_fragment_mapping(&self, table_fragment: &TableFragments, operation: Operation) {
        for fragment in table_fragment.fragments.values() {
            if !fragment.state_table_ids.is_empty() {
                let mapping = fragment
                    .vnode_mapping
                    .clone()
                    .expect("no data distribution found");
                self.env
                    .notification_manager()
                    .notify_frontend(operation, Info::ParallelUnitMapping(mapping))
                    .await;
            }
        }
    }

    pub async fn select_table_fragments_by_table_id(
        &self,
        table_id: &TableId,
    ) -> MetaResult<TableFragments> {
        let map = &self.core.read().await.table_fragments;
        Ok(map
            .get(table_id)
            .cloned()
            .context(format!("table_fragment not exist: id={}", table_id))?)
    }

    /// Start create a new `TableFragments` and insert it into meta store, currently the actors'
    /// state is `ActorState::Inactive` and the table fragments' state is `State::Creating`.
    pub async fn start_create_table_fragments(
        &self,
        table_fragment: TableFragments,
    ) -> MetaResult<()> {
        let map = &mut self.core.write().await.table_fragments;
        let table_id = table_fragment.table_id();
        if map.contains_key(&table_id) {
            bail!("table_fragment already exist: id={}", table_id);
        }

        let mut table_fragments = BTreeMapTransaction::new(map);
        table_fragments.insert(table_id, table_fragment);
        commit_meta!(self, table_fragments)
    }

    /// Cancel creation of a new `TableFragments` and delete it from meta store.
    pub async fn cancel_create_table_fragments(&self, table_id: &TableId) -> MetaResult<()> {
        let map = &mut self.core.write().await.table_fragments;
        if !map.contains_key(table_id) {
            tracing::warn!("table_fragment cleaned: id={}", table_id);
        }

        let mut table_fragments = BTreeMapTransaction::new(map);
        table_fragments.remove(*table_id);
        commit_meta!(self, table_fragments)
    }

    /// Called after the barrier collection of `CreateMaterializedView` command, which updates the
    /// actors' state to `ActorState::Running`, besides also updates all dependent tables'
    /// downstream actors info.
    ///
    /// Note that the table fragments' state will be kept `Creating`, which is only updated when the
    /// materialized view is completely created.
    pub async fn post_create_table_fragments(
        &self,
        table_id: &TableId,
        dependent_table_actors: Vec<(TableId, HashMap<ActorId, Vec<Dispatcher>>)>,
        split_assignment: SplitAssignment,
    ) -> MetaResult<()> {
        let map = &mut self.core.write().await.table_fragments;

        let mut table_fragments = BTreeMapTransaction::new(map);
        let mut table_fragment = table_fragments
            .get_mut(*table_id)
            .context(format!("table_fragment not exist: id={}", table_id))?;

        assert_eq!(table_fragment.state(), State::Creating);
        table_fragment.update_actors_state(ActorState::Running);
        table_fragment.set_actor_splits_by_split_assignment(split_assignment);
        let table_fragment = table_fragment.clone();

        for (dependent_table_id, mut new_dispatchers) in dependent_table_actors {
            let mut dependent_table =
                table_fragments
                    .get_mut(dependent_table_id)
                    .context(format!(
                        "dependent table_fragment not exist: id={}",
                        dependent_table_id
                    ))?;
            for fragment in dependent_table.fragments.values_mut() {
                for actor in &mut fragment.actors {
                    // Extend new dispatchers to table fragments.
                    if let Some(new_dispatchers) = new_dispatchers.remove(&actor.actor_id) {
                        actor.dispatcher.extend(new_dispatchers);
                    }
                }
            }
        }
        commit_meta!(self, table_fragments)?;
        self.notify_fragment_mapping(&table_fragment, Operation::Add)
            .await;

        Ok(())
    }

    /// Called after the finish of `CreateMaterializedView` command, i.e., materialized view is
    /// completely created, which updates the state from `Creating` to `Created`.
    pub async fn mark_table_fragments_created(&self, table_id: TableId) -> MetaResult<()> {
        let map = &mut self.core.write().await.table_fragments;

        let mut table_fragments = BTreeMapTransaction::new(map);
        let mut table_fragment = table_fragments
            .get_mut(table_id)
            .context(format!("table_fragment not exist: id={}", table_id))?;

        assert_eq!(table_fragment.state(), State::Creating);
        table_fragment.set_state(State::Created);
        commit_meta!(self, table_fragments)
    }

    /// Drop table fragments info and remove downstream actor infos in fragments from its dependent
    /// tables.
    pub async fn drop_table_fragments_vec(&self, table_ids: &HashSet<TableId>) -> MetaResult<()> {
        let map = &mut self.core.write().await.table_fragments;
        let to_delete_table_fragments = table_ids
            .iter()
            .filter_map(|table_id| map.get(table_id).cloned())
            .collect_vec();

        let mut table_fragments = BTreeMapTransaction::new(map);
        for table_fragment in &to_delete_table_fragments {
            table_fragments.remove(table_fragment.table_id());
            let chain_actor_ids = table_fragment.chain_actor_ids();
            let dependent_table_ids = table_fragment.dependent_table_ids();
            for dependent_table_id in dependent_table_ids {
                if table_ids.contains(&dependent_table_id) {
                    continue;
                }
                let mut dependent_table =
                    table_fragments
                        .get_mut(dependent_table_id)
                        .context(format!(
                            "dependent table_fragment not exist: id={}",
                            dependent_table_id
                        ))?;

                dependent_table
                    .fragments
                    .values_mut()
                    .filter(|f| f.fragment_type() == FragmentType::Sink)
                    .flat_map(|f| &mut f.actors)
                    .for_each(|a| {
                        a.dispatcher.retain_mut(|d| {
                            d.downstream_actor_id
                                .retain(|x| !chain_actor_ids.contains(x));
                            !d.downstream_actor_id.is_empty()
                        })
                    });
            }
        }
        commit_meta!(self, table_fragments)?;

        for table_fragments in to_delete_table_fragments {
            self.notify_fragment_mapping(&table_fragments, Operation::Delete)
                .await;
        }

        Ok(())
    }

    /// Used in [`crate::barrier::GlobalBarrierManager`], load all actor that need to be sent or
    /// collected
    pub async fn load_all_actors(
        &self,
        check_state: impl Fn(ActorState, TableId, ActorId) -> bool,
    ) -> ActorInfos {
        let mut actor_maps = HashMap::new();
        let mut source_actor_maps = HashMap::new();

        let map = &self.core.read().await.table_fragments;
        for fragments in map.values() {
            for (worker_id, actor_states) in fragments.worker_actor_states() {
                for (actor_id, actor_state) in actor_states {
                    if check_state(actor_state, fragments.table_id(), actor_id) {
                        actor_maps
                            .entry(worker_id)
                            .or_insert_with(Vec::new)
                            .push(actor_id);
                    }
                }
            }

            let source_actors = fragments.worker_source_actor_states();
            for (worker_id, actor_states) in source_actors {
                for (actor_id, actor_state) in actor_states {
                    if check_state(actor_state, fragments.table_id(), actor_id) {
                        source_actor_maps
                            .entry(worker_id)
                            .or_insert_with(Vec::new)
                            .push(actor_id);
                    }
                }
            }
        }

        ActorInfos {
            actor_maps,
            source_actor_maps,
        }
    }

    /// Used in [`crate::barrier::GlobalBarrierManager`]
    /// migrate actors and update fragments, generate migrate info
    pub async fn migrate_actors(
        &self,
        migrate_map: &HashMap<ActorId, WorkerId>,
        node_map: &HashMap<WorkerId, WorkerNode>,
    ) -> MetaResult<()> {
        let mut parallel_unit_migrate_map = HashMap::new();
        let mut pu_map: HashMap<WorkerId, Vec<&ParallelUnit>> = node_map
            .iter()
            .map(|(&worker_id, worker)| (worker_id, worker.parallel_units.iter().collect_vec()))
            .collect();

        // update actor status and generate pu to pu migrate info
        let mut table_fragments = self.list_table_fragments().await?;
        let mut new_fragments = Vec::new();
        table_fragments.iter_mut().for_each(|fragment| {
            let mut flag = false;
            fragment
                .actor_status
                .iter_mut()
                .for_each(|(actor_id, status)| {
                    if let Some(new_node_id) = migrate_map.get(actor_id) {
                        if let Some(ref old_parallel_unit) = status.parallel_unit {
                            flag = true;
                            if let Entry::Vacant(e) =
                                parallel_unit_migrate_map.entry(old_parallel_unit.id)
                            {
                                let new_parallel_unit =
                                    pu_map.get_mut(new_node_id).unwrap().pop().unwrap();
                                e.insert(new_parallel_unit.clone());
                                status.parallel_unit = Some(new_parallel_unit.clone());
                            } else {
                                status.parallel_unit = Some(
                                    parallel_unit_migrate_map
                                        .get(&old_parallel_unit.id)
                                        .unwrap()
                                        .clone(),
                                );
                            }
                        }
                    };
                });
            if flag {
                // update vnode mapping of updated fragments
                fragment.update_vnode_mapping(&parallel_unit_migrate_map);
                new_fragments.push(fragment.clone());
            }
        });
        // update fragments
        self.batch_update_table_fragments(&new_fragments).await?;
        Ok(())
    }

    pub async fn all_node_actors(
        &self,
        include_inactive: bool,
    ) -> HashMap<WorkerId, Vec<StreamActor>> {
        let mut actor_maps = HashMap::new();

        let map = &self.core.read().await.table_fragments;
        for fragments in map.values() {
            for (node_id, actor_ids) in fragments.worker_actors(include_inactive) {
                let node_actor_ids = actor_maps.entry(node_id).or_insert_with(Vec::new);
                node_actor_ids.extend(actor_ids);
            }
        }

        actor_maps
    }

    pub async fn all_chain_actor_ids(&self) -> HashSet<ActorId> {
        let map = &self.core.read().await.table_fragments;

        map.values()
            .flat_map(|table_fragment| table_fragment.chain_actor_ids())
            .collect::<HashSet<_>>()
    }

    pub async fn update_actor_splits_by_split_assignment(
        &self,
        split_assignment: &SplitAssignment,
    ) -> MetaResult<()> {
        let map = &mut self.core.write().await.table_fragments;
        let to_update_table_fragments: HashMap<TableId, HashMap<ActorId, Vec<SplitImpl>>> = map
            .values()
            .filter(|t| t.fragment_ids().any(|f| split_assignment.contains_key(&f)))
            .map(|f| {
                let mut actor_splits = HashMap::new();
                f.fragment_ids().for_each(|fragment_id| {
                    if let Some(splits) = split_assignment.get(&fragment_id).cloned() {
                        actor_splits.extend(splits);
                    }
                });
                (f.table_id(), actor_splits)
            })
            .collect();

        let mut table_fragments = BTreeMapTransaction::new(map);
        for (table_id, actor_splits) in to_update_table_fragments {
            let mut table_fragment = table_fragments.get_mut(table_id).unwrap();
            table_fragment.actor_splits.extend(actor_splits);
        }
        commit_meta!(self, table_fragments)
    }

    /// Get the actor ids of the fragment with `fragment_id` with `Running` status.
    pub async fn get_running_actors_of_fragment(
        &self,
        fragment_id: FragmentId,
    ) -> MetaResult<HashSet<ActorId>> {
        let map = &self.core.read().await.table_fragments;

        for table_fragment in map.values() {
            if let Some(fragment) = table_fragment.fragments.get(&fragment_id) {
                let running_actor_ids = fragment
                    .actors
                    .iter()
                    .map(|a| a.actor_id)
                    .filter(|a| table_fragment.actor_status[a].state == ActorState::Running as i32)
                    .collect();
                return Ok(running_actor_ids);
            }
        }

        bail!("fragment not found: {}", fragment_id)
    }

    /// Add the newly added Actor to the `FragmentManager`
    pub async fn pre_apply_reschedules(
        &self,
        mut created_actors: HashMap<FragmentId, HashMap<ActorId, (StreamActor, ActorStatus)>>,
    ) -> HashMap<FragmentId, HashSet<ActorId>> {
        let map = &mut self.core.write().await.table_fragments;

        let mut applied_reschedules = HashMap::new();

        for table_fragments in map.values_mut() {
            let mut updated_actor_status = HashMap::new();

            for (fragment_id, fragment) in &mut table_fragments.fragments {
                if let Some(fragment_create_actors) = created_actors.remove(fragment_id) {
                    applied_reschedules
                        .entry(*fragment_id)
                        .or_insert_with(HashSet::new)
                        .extend(fragment_create_actors.keys());

                    for (actor_id, (actor, actor_status)) in fragment_create_actors {
                        fragment.actors.push(actor);
                        updated_actor_status.insert(actor_id, actor_status);
                    }
                }
            }

            table_fragments.actor_status.extend(updated_actor_status);
        }

        applied_reschedules
    }

    /// Undo the changes in `pre_apply_reschedules`
    pub async fn cancel_apply_reschedules(
        &self,
        applied_reschedules: HashMap<FragmentId, HashSet<ActorId>>,
    ) {
        let map = &mut self.core.write().await.table_fragments;
        for table_fragments in map.values_mut() {
            for (fragment_id, fragment) in &mut table_fragments.fragments {
                if let Some(fragment_create_actors) = applied_reschedules.get(fragment_id) {
                    table_fragments
                        .actor_status
                        .drain_filter(|actor_id, _| fragment_create_actors.contains(actor_id));
                    fragment
                        .actors
                        .drain_filter(|actor| fragment_create_actors.contains(&actor.actor_id));
                }
            }
        }
    }

    /// Apply `Reschedule`s to fragments.
    pub async fn post_apply_reschedules(
        &self,
        mut reschedules: HashMap<FragmentId, Reschedule>,
    ) -> MetaResult<()> {
        let map = &mut self.core.write().await.table_fragments;

        fn update_actors(
            actors: &mut Vec<ActorId>,
            to_remove: &HashSet<ActorId>,
            to_create: &[ActorId],
        ) {
            let actor_id_set: HashSet<_> = actors.iter().copied().collect();
            for actor_id in to_create {
                assert!(!actor_id_set.contains(actor_id));
            }
            for actor_id in to_remove {
                assert!(actor_id_set.contains(actor_id));
            }

            actors.drain_filter(|actor_id| to_remove.contains(actor_id));
            actors.extend_from_slice(to_create);
        }

        fn update_merge_node_upstream(
            stream_node: &mut StreamNode,
            upstream_fragment_id: &FragmentId,
            upstream_actors_to_remove: &HashSet<ActorId>,
            upstream_actors_to_create: &Vec<ActorId>,
        ) {
            if let Some(NodeBody::Merge(s)) = stream_node.node_body.as_mut() {
                if s.upstream_fragment_id == *upstream_fragment_id {
                    update_actors(
                        s.upstream_actor_id.as_mut(),
                        upstream_actors_to_remove,
                        upstream_actors_to_create,
                    );
                }
            }

            for child in &mut stream_node.input {
                update_merge_node_upstream(
                    child,
                    upstream_fragment_id,
                    upstream_actors_to_remove,
                    upstream_actors_to_create,
                );
            }
        }

        let new_created_actors: HashSet<_> = reschedules
            .values()
            .flat_map(|reschedule| reschedule.added_actors.clone())
            .collect();

        let to_update_table_fragments = map
            .values()
            .filter(|t| t.fragment_ids().any(|f| reschedules.contains_key(&f)))
            .map(|t| t.table_id())
            .collect_vec();
        let mut table_fragments = BTreeMapTransaction::new(map);
        let mut fragment_mapping_to_notify = vec![];

        for table_id in to_update_table_fragments {
            // Takes out the reschedules of the fragments in this table.
            let reschedules = reschedules
                .drain_filter(|fragment_id, _| {
                    table_fragments
                        .get(&table_id)
                        .unwrap()
                        .fragments
                        .contains_key(fragment_id)
                })
                .collect_vec();

            for (fragment_id, reschedule) in reschedules {
                let Reschedule {
                    added_actors,
                    removed_actors,
                    vnode_bitmap_updates,
                    upstream_fragment_dispatcher_ids,
                    upstream_dispatcher_mapping,
                    downstream_fragment_ids: downstream_fragment_id,
                    actor_splits,
                } = reschedule;

                let mut table_fragment = table_fragments.get_mut(table_id).unwrap();

                // Add actors to this fragment: set the state to `Running`.
                for actor_id in &added_actors {
                    table_fragment
                        .actor_status
                        .get_mut(actor_id)
                        .unwrap()
                        .set_state(ActorState::Running);
                }

                // Remove actors from this fragment.
                let removed_actor_ids: HashSet<_> = removed_actors.iter().cloned().collect();

                for actor_id in &removed_actor_ids {
                    table_fragment.actor_status.remove(actor_id);
                    table_fragment.actor_splits.remove(actor_id);
                }

                table_fragment.actor_splits.extend(actor_splits);

                let actor_status = table_fragment.actor_status.clone();
                let fragment = table_fragment.fragments.get_mut(&fragment_id).unwrap();

                // update vnode mapping for actors.
                for actor in &mut fragment.actors {
                    if let Some(bitmap) = vnode_bitmap_updates.get(&actor.actor_id) {
                        actor.vnode_bitmap = Some(bitmap.to_protobuf());
                    }
                }

                fragment
                    .actors
                    .retain(|a| !removed_actor_ids.contains(&a.actor_id));

                // update fragment's vnode mapping
                if let Some(vnode_mapping) = fragment.vnode_mapping.as_mut() {
                    let mut actor_to_parallel_unit = HashMap::with_capacity(fragment.actors.len());
                    for actor in &fragment.actors {
                        if let Some(actor_status) = actor_status.get(&actor.actor_id) {
                            if let Some(parallel_unit) = actor_status.parallel_unit.as_ref() {
                                actor_to_parallel_unit.insert(
                                    actor.actor_id as ActorId,
                                    parallel_unit.id as ParallelUnitId,
                                );
                            }
                        }
                    }

                    if let Some(actor_mapping) = upstream_dispatcher_mapping.as_ref() {
                        *vnode_mapping = actor_mapping_to_parallel_unit_mapping(
                            fragment_id,
                            &actor_to_parallel_unit,
                            actor_mapping,
                        )
                    }

                    if !fragment.state_table_ids.is_empty() {
                        let mut mapping = vnode_mapping.clone();
                        mapping.fragment_id = fragment.fragment_id;
                        fragment_mapping_to_notify.push(mapping);
                    }
                }

                // Update the dispatcher of the upstream fragments.
                for (upstream_fragment_id, dispatcher_id) in upstream_fragment_dispatcher_ids {
                    // TODO: here we assume the upstream fragment is in the same materialized view
                    // as this fragment.
                    let upstream_fragment = table_fragment
                        .fragments
                        .get_mut(&upstream_fragment_id)
                        .unwrap();

                    for upstream_actor in &mut upstream_fragment.actors {
                        if new_created_actors.contains(&upstream_actor.actor_id) {
                            continue;
                        }

                        for dispatcher in &mut upstream_actor.dispatcher {
                            if dispatcher.dispatcher_id == dispatcher_id {
                                dispatcher.hash_mapping = upstream_dispatcher_mapping.clone();
                                update_actors(
                                    dispatcher.downstream_actor_id.as_mut(),
                                    &removed_actor_ids,
                                    &added_actors,
                                );
                            }
                        }
                    }
                }

                // Update the merge executor of the downstream fragment.
                if let Some(downstream_fragment_id) = downstream_fragment_id {
                    let downstream_fragment = table_fragment
                        .fragments
                        .get_mut(&downstream_fragment_id)
                        .unwrap();
                    for downstream_actor in &mut downstream_fragment.actors {
                        if new_created_actors.contains(&downstream_actor.actor_id) {
                            continue;
                        }

                        update_actors(
                            downstream_actor.upstream_actor_id.as_mut(),
                            &removed_actor_ids,
                            &added_actors,
                        );

                        if let Some(node) = downstream_actor.nodes.as_mut() {
                            update_merge_node_upstream(
                                node,
                                &fragment_id,
                                &removed_actor_ids,
                                &added_actors,
                            );
                        }
                    }
                }
            }
        }

        assert!(reschedules.is_empty(), "all reschedules must be applied");
        commit_meta!(self, table_fragments)?;

        for mapping in fragment_mapping_to_notify {
            self.env
                .notification_manager()
                .notify_frontend(Operation::Update, Info::ParallelUnitMapping(mapping))
                .await;
        }

        Ok(())
    }

    pub async fn table_node_actors(
        &self,
        table_ids: &HashSet<TableId>,
    ) -> MetaResult<BTreeMap<WorkerId, Vec<ActorId>>> {
        let map = &self.core.read().await.table_fragments;
        let table_fragments_vec = table_ids
            .iter()
            .map(|table_id| {
                map.get(table_id)
                    .ok_or_else(|| anyhow!("table_fragment not exist: id={}", table_id).into())
            })
            .collect::<MetaResult<Vec<_>>>()?;
        Ok(table_fragments_vec
            .iter()
            .map(|table_fragments| table_fragments.worker_actor_ids())
            .reduce(|mut btree_map, next_map| {
                next_map.into_iter().for_each(|(k, v)| {
                    btree_map.entry(k).or_insert_with(Vec::new).extend(v);
                });
                btree_map
            })
            .unwrap())
    }

    pub async fn get_table_actor_ids(
        &self,
        table_ids: &HashSet<TableId>,
    ) -> MetaResult<Vec<ActorId>> {
        let map = &self.core.read().await.table_fragments;
        table_ids
            .iter()
            .map(|table_id| {
                map.get(table_id)
                    .map(|table_fragment| table_fragment.actor_ids())
                    .ok_or_else(|| anyhow!("table_fragment not exist: id={}", table_id).into())
            })
            .flatten_ok()
            .collect::<MetaResult<Vec<_>>>()
    }

    pub async fn get_table_sink_actor_ids(&self, table_id: &TableId) -> MetaResult<Vec<ActorId>> {
        let map = &self.core.read().await.table_fragments;
        Ok(map
            .get(table_id)
            .context(format!("table_fragment not exist: id={}", table_id))?
            .sink_actor_ids())
    }

    // we will read three things at once, avoiding locking too much.
    pub async fn get_build_graph_info(
        &self,
        table_ids: &HashSet<TableId>,
    ) -> MetaResult<BuildGraphInfo> {
        let map = &self.core.read().await.table_fragments;
        let mut info: BuildGraphInfo = Default::default();

        for table_id in table_ids {
            info.table_sink_actor_ids.insert(
                *table_id,
                map.get(table_id)
                    .context(format!("table_fragment not exist: id={}", table_id))?
                    .sink_actor_ids(),
            );
        }
        Ok(info)
    }

    pub async fn get_sink_vnode_bitmap_info(
        &self,
        table_ids: &HashSet<TableId>,
    ) -> MetaResult<HashMap<TableId, Vec<(ActorId, Option<Buffer>)>>> {
        let map = &self.core.read().await.table_fragments;
        let mut info: HashMap<TableId, Vec<(ActorId, Option<Buffer>)>> = HashMap::new();

        for table_id in table_ids {
            info.insert(
                *table_id,
                map.get(table_id)
                    .context(format!("table_fragment not exist: id={}", table_id))?
                    .sink_vnode_bitmap_info(),
            );
        }

        Ok(info)
    }

    pub async fn get_sink_fragment_vnode_info(
        &self,
        table_ids: &HashSet<TableId>,
    ) -> MetaResult<HashMap<TableId, FragmentVNodeInfo>> {
        let map = &self.core.read().await.table_fragments;
        let mut info: HashMap<TableId, FragmentVNodeInfo> = HashMap::new();

        for table_id in table_ids {
            let table_fragment = map
                .get(table_id)
                .context(format!("table_fragment not exist: id={}", table_id))?;
            info.insert(
                *table_id,
                FragmentVNodeInfo {
                    actor_parallel_unit_maps: table_fragment.sink_actor_parallel_units(),
                    vnode_mapping: table_fragment.sink_vnode_mapping(),
                },
            );
        }

        Ok(info)
    }

    pub async fn get_tables_worker_actors(
        &self,
        table_ids: &HashSet<TableId>,
    ) -> MetaResult<HashMap<TableId, BTreeMap<WorkerId, Vec<ActorId>>>> {
        let map = &self.core.read().await.table_fragments;
        let mut info: HashMap<TableId, BTreeMap<WorkerId, Vec<ActorId>>> = HashMap::new();

        for table_id in table_ids {
            info.insert(
                *table_id,
                map.get(table_id)
                    .context(format!("table_fragment not exist: id={}", table_id))?
                    .worker_actor_ids(),
            );
        }

        Ok(info)
    }
}
