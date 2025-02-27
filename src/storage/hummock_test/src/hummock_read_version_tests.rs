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

use std::ops::Bound;

use itertools::Itertools;
use risingwave_common::catalog::TableId;
use risingwave_hummock_sdk::key::key_with_epoch;
use risingwave_meta::hummock::test_utils::setup_compute_env;
use risingwave_pb::hummock::{KeyRange, SstableInfo};
use risingwave_storage::hummock::iterator::test_utils::iterator_test_key_of_epoch;
use risingwave_storage::hummock::shared_buffer::shared_buffer_batch::SharedBufferBatch;
use risingwave_storage::hummock::store::memtable::ImmutableMemtable;
use risingwave_storage::hummock::store::version::{
    HummockReadVersion, StagingData, StagingSstableInfo, VersionUpdate,
};
use risingwave_storage::hummock::test_utils::gen_dummy_batch;

use crate::test_utils::prepare_first_valid_version;

#[tokio::test]
async fn test_read_version_basic() {
    let (env, hummock_manager_ref, _cluster_manager_ref, worker_node) =
        setup_compute_env(8080).await;

    let (pinned_version, _, _) =
        prepare_first_valid_version(env, hummock_manager_ref, worker_node).await;

    let mut read_version = HummockReadVersion::new(pinned_version);
    let mut epoch = 1;
    let table_id = 0;

    {
        // single imm
        let kv_pairs = gen_dummy_batch(epoch);
        let imm = SharedBufferBatch::build_shared_buffer_batch(
            epoch,
            kv_pairs,
            TableId::from(table_id),
            None,
        )
        .await;

        read_version.update(VersionUpdate::Staging(StagingData::ImmMem(imm)));

        let key = iterator_test_key_of_epoch(0, epoch);
        let key_range = (Bound::Included(key.to_vec()), Bound::Included(key.to_vec()));

        let (staging_imm_iter, staging_sst_iter) =
            read_version
                .staging()
                .prune_overlap(epoch, TableId::default(), &key_range);

        let staging_imm = staging_imm_iter
            .cloned()
            .collect::<Vec<ImmutableMemtable>>();

        assert_eq!(1, staging_imm.len());
        assert_eq!(0, staging_sst_iter.count());
        assert!(staging_imm.iter().any(|imm| imm.epoch() <= epoch));
    }

    {
        // several epoch
        for _ in 0..5 {
            // epoch from 1 to 6
            epoch += 1;
            let kv_pairs = gen_dummy_batch(epoch);
            let imm = SharedBufferBatch::build_shared_buffer_batch(
                epoch,
                kv_pairs,
                TableId::from(table_id),
                None,
            )
            .await;

            read_version.update(VersionUpdate::Staging(StagingData::ImmMem(imm)));
        }

        let key = iterator_test_key_of_epoch(0, epoch);
        let key_range = (Bound::Included(key.to_vec()), Bound::Included(key.to_vec()));

        let (staging_imm_iter, staging_sst_iter) =
            read_version
                .staging()
                .prune_overlap(epoch, TableId::default(), &key_range);

        let staging_imm = staging_imm_iter
            .cloned()
            .collect::<Vec<ImmutableMemtable>>();

        assert_eq!(1, staging_imm.len());
        assert_eq!(0, staging_sst_iter.count());
        assert!(staging_imm.iter().any(|imm| imm.epoch() <= epoch));
    }

    {
        // test clean imm with sst update info
        let staging = read_version.staging();
        assert_eq!(6, staging.imm.len());
        let batch_id_vec_for_clear = staging
            .imm
            .iter()
            .rev()
            .map(|imm| imm.batch_id())
            .take(3)
            .rev()
            .collect::<Vec<_>>();

        let epoch_id_vec_for_clear = staging
            .imm
            .iter()
            .rev()
            .map(|imm| imm.epoch())
            .take(3)
            .rev()
            .collect::<Vec<_>>();

        let dummy_sst = StagingSstableInfo::new(
            vec![
                SstableInfo {
                    id: 1,
                    key_range: Some(KeyRange {
                        left: key_with_epoch(iterator_test_key_of_epoch(0, 1), 1),
                        right: key_with_epoch(iterator_test_key_of_epoch(0, 2), 2),
                    }),
                    file_size: 1,
                    table_ids: vec![0],
                    meta_offset: 1,
                    stale_key_count: 1,
                    total_key_count: 1,
                    divide_version: 0,
                },
                SstableInfo {
                    id: 2,
                    key_range: Some(KeyRange {
                        left: key_with_epoch(iterator_test_key_of_epoch(0, 3), 3),
                        right: key_with_epoch(iterator_test_key_of_epoch(0, 3), 3),
                    }),
                    file_size: 1,
                    table_ids: vec![0],
                    meta_offset: 1,
                    stale_key_count: 1,
                    total_key_count: 1,
                    divide_version: 0,
                },
            ],
            epoch_id_vec_for_clear,
            batch_id_vec_for_clear,
        );

        {
            read_version.update(VersionUpdate::Staging(StagingData::Sst(dummy_sst)));
        }
    }

    {
        // test clear related batch after update sst

        // after update sst
        // imm(0, 1, 2) => sst{sst_id: 1}
        // staging => {imm(3, 4, 5), sst[{sst_id: 1}, {sst_id: 2}]}
        let staging = read_version.staging();
        assert_eq!(3, read_version.staging().imm.len());
        assert_eq!(1, read_version.staging().sst.len());
        assert_eq!(2, read_version.staging().sst[0].sstable_infos().len());
        let remain_batch_id_vec = staging
            .imm
            .iter()
            .map(|imm| imm.batch_id())
            .collect::<Vec<_>>();
        assert!(remain_batch_id_vec.iter().any(|batch_id| *batch_id > 2));
    }

    {
        let key_range_left = iterator_test_key_of_epoch(0, 0);
        let key_range_right = iterator_test_key_of_epoch(0, 4);

        let key_range = (
            Bound::Included(key_range_left),
            Bound::Included(key_range_right),
        );

        let (staging_imm_iter, staging_sst_iter) =
            read_version
                .staging()
                .prune_overlap(epoch, TableId::default(), &key_range);

        let staging_imm = staging_imm_iter.cloned().collect_vec();
        assert_eq!(1, staging_imm.len());
        assert_eq!(4, staging_imm[0].epoch());

        let staging_ssts = staging_sst_iter.cloned().collect_vec();
        assert_eq!(2, staging_ssts.len());
        assert_eq!(1, staging_ssts[0].id);
        assert_eq!(2, staging_ssts[1].id);
    }

    {
        let key_range_left = iterator_test_key_of_epoch(0, 3);
        let key_range_right = iterator_test_key_of_epoch(0, 4);

        let key_range = (
            Bound::Included(key_range_left),
            Bound::Included(key_range_right),
        );

        let (staging_imm_iter, staging_sst_iter) =
            read_version
                .staging()
                .prune_overlap(epoch, TableId::default(), &key_range);

        let staging_imm = staging_imm_iter.cloned().collect_vec();
        assert_eq!(1, staging_imm.len());
        assert_eq!(4, staging_imm[0].epoch());

        let staging_ssts = staging_sst_iter.cloned().collect_vec();
        assert_eq!(1, staging_ssts.len());
        assert_eq!(2, staging_ssts[0].id);
    }
}
