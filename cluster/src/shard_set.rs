// Copyright 2023 The HoraeDB Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{collections::HashMap, sync::Arc};

use common_types::table::ShardVersion;
use generic_error::BoxError;
use meta_client::types::{ShardId, ShardInfo, ShardStatus, TableInfo, TablesOfShard};
use snafu::{ensure, OptionExt, ResultExt};

use crate::{
    shard_operator::{
        CloseContext, CloseTableContext, CreateTableContext, DropTableContext, OpenContext,
        OpenTableContext, ShardOperator,
    },
    OpenShardNoCause, OpenShardWithCause, Result, ShardVersionMismatch, TableAlreadyExists,
    TableNotFound, UpdateFrozenShard,
};

/// Shard set
///
/// Manage all shards opened on current node
#[derive(Debug, Default, Clone)]
pub struct ShardSet {
    inner: Arc<std::sync::RwLock<HashMap<ShardId, ShardRef>>>,
}

impl ShardSet {
    // Fetch all the shards, including not opened.
    pub fn all_shards(&self) -> Vec<ShardRef> {
        let inner = self.inner.read().unwrap();
        inner.values().cloned().collect()
    }

    // Get the shard by its id.
    pub fn get(&self, shard_id: ShardId) -> Option<ShardRef> {
        let inner = self.inner.read().unwrap();
        inner.get(&shard_id).cloned()
    }

    /// Remove the shard.
    pub fn remove(&self, shard_id: ShardId) -> Option<ShardRef> {
        let mut inner = self.inner.write().unwrap();
        inner.remove(&shard_id)
    }

    /// Insert the tables of one shard.
    pub fn insert(&self, shard_id: ShardId, shard: ShardRef) -> Option<ShardRef> {
        let mut inner = self.inner.write().unwrap();
        inner.insert(shard_id, shard)
    }
}

/// Shard
///
/// NOTICE: all write operations on a shard will be performed sequentially.
pub struct Shard {
    data: ShardDataRef,
    operator: tokio::sync::Mutex<ShardOperator>,
}

impl std::fmt::Debug for Shard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shard").field("data", &self.data).finish()
    }
}

impl Shard {
    pub fn new(tables_of_shard: TablesOfShard) -> Self {
        let data = Arc::new(std::sync::RwLock::new(ShardData {
            shard_info: tables_of_shard.shard_info,
            tables: tables_of_shard.tables,
        }));

        let operator = tokio::sync::Mutex::new(ShardOperator { data: data.clone() });

        Self { data, operator }
    }

    pub fn shard_info(&self) -> ShardInfo {
        let data = self.data.read().unwrap();

        data.shard_info.clone()
    }

    pub fn find_table(&self, schema_name: &str, table_name: &str) -> Option<TableInfo> {
        let data = self.data.read().unwrap();
        data.find_table(schema_name, table_name)
    }

    pub async fn open(&self, ctx: OpenContext) -> Result<()> {
        let operator = self
            .operator
            .try_lock()
            .box_err()
            .context(OpenShardWithCause {
                msg: "Failed to get shard operator lock",
            })?;

        {
            let mut data = self.data.write().unwrap();
            if !data.need_open() {
                return OpenShardNoCause {
                    msg: "Shard is already in opening",
                }
                .fail();
            }

            data.begin_open();
        }

        let ret = operator.open(ctx).await;

        if ret.is_ok() {
            let mut data = self.data.write().unwrap();
            data.finish_open();
        }
        // If open failed, shard status is unchanged(`Opening`), so it can be
        // rescheduled to open again.

        ret
    }

    pub fn is_opened(&self) -> bool {
        let data = self.data.read().unwrap();
        data.is_opened()
    }

    pub async fn close(&self, ctx: CloseContext) -> Result<()> {
        let operator = self.operator.lock().await;
        operator.close(ctx).await
    }

    pub async fn create_table(&self, ctx: CreateTableContext) -> Result<ShardVersion> {
        let operator = self.operator.lock().await;
        operator.create_table(ctx).await
    }

    pub async fn drop_table(&self, ctx: DropTableContext) -> Result<ShardVersion> {
        let operator = self.operator.lock().await;
        operator.drop_table(ctx).await
    }

    pub async fn open_table(&self, ctx: OpenTableContext) -> Result<()> {
        let operator = self.operator.lock().await;
        operator.open_table(ctx).await
    }

    pub async fn close_table(&self, ctx: CloseTableContext) -> Result<()> {
        let operator = self.operator.lock().await;
        operator.close_table(ctx).await
    }
}

pub type ShardRef = Arc<Shard>;

#[derive(Debug, Clone)]
pub struct UpdatedTableInfo {
    pub shard_info: ShardInfo,
    pub table_info: TableInfo,
}

/// Shard data
#[derive(Debug)]
pub struct ShardData {
    /// Shard info
    pub shard_info: ShardInfo,

    /// Tables in shard
    pub tables: Vec<TableInfo>,
}

impl ShardData {
    pub fn find_table(&self, schema_name: &str, table_name: &str) -> Option<TableInfo> {
        self.tables
            .iter()
            .find(|table| table.schema_name == schema_name && table.name == table_name)
            .cloned()
    }

    #[inline]
    pub fn freeze(&mut self) {
        self.shard_info.status = ShardStatus::Frozen;
    }

    #[inline]
    pub fn begin_open(&mut self) {
        self.shard_info.status = ShardStatus::Opening;
    }

    #[inline]
    pub fn finish_open(&mut self) {
        assert_eq!(self.shard_info.status, ShardStatus::Opening);

        self.shard_info.status = ShardStatus::Ready;
    }

    #[inline]
    pub fn need_open(&self) -> bool {
        !self.is_opened()
    }

    #[inline]
    pub fn is_opened(&self) -> bool {
        self.shard_info.is_opened()
    }

    #[inline]
    fn is_frozen(&self) -> bool {
        matches!(self.shard_info.status, ShardStatus::Frozen)
    }

    #[inline]
    fn inc_shard_version(&mut self) {
        self.shard_info.version += 1;
    }

    /// Create the table on the shard, whose version will be incremented.
    #[inline]
    pub fn try_create_table(&mut self, updated_info: UpdatedTableInfo) -> Result<ShardVersion> {
        self.try_insert_table(updated_info, true)
    }

    /// Open the table on the shard, whose version won't change.
    #[inline]
    pub fn try_open_table(&mut self, updated_info: UpdatedTableInfo) -> Result<()> {
        self.try_insert_table(updated_info, false)?;

        Ok(())
    }

    /// Try to insert the table into the shard.
    ///
    /// The shard version may be incremented and the new version will be
    /// returned.
    fn try_insert_table(
        &mut self,
        updated_info: UpdatedTableInfo,
        inc_version: bool,
    ) -> Result<ShardVersion> {
        let UpdatedTableInfo {
            shard_info: curr_shard_info,
            table_info: new_table,
        } = updated_info;

        ensure!(
            !self.is_frozen(),
            UpdateFrozenShard {
                shard_id: curr_shard_info.id,
            }
        );

        ensure!(
            self.shard_info.version == curr_shard_info.version,
            ShardVersionMismatch {
                shard_info: self.shard_info.clone(),
                expect_version: curr_shard_info.version,
            }
        );

        let table = self.tables.iter().find(|v| v.id == new_table.id);
        ensure!(
            table.is_none(),
            TableAlreadyExists {
                msg: "the table to insert has already existed",
            }
        );

        // Insert the new table into the shard.
        self.tables.push(new_table);

        // Update the shard version if necessary.
        if inc_version {
            self.inc_shard_version();
        }

        Ok(self.shard_info.version)
    }

    /// Drop the table from the shard, whose version will be incremented.
    #[inline]
    pub fn try_drop_table(&mut self, updated_info: UpdatedTableInfo) -> Result<ShardVersion> {
        self.try_remove_table(updated_info, true)
    }

    /// Close the table from the shard, whose version won't change.
    #[inline]
    pub fn try_close_table(&mut self, updated_info: UpdatedTableInfo) -> Result<()> {
        self.try_remove_table(updated_info, false)?;

        Ok(())
    }

    /// Try to remove the table from the shard.
    ///
    /// The shard version may be incremented and the new version will be
    /// returned.
    fn try_remove_table(
        &mut self,
        updated_info: UpdatedTableInfo,
        inc_version: bool,
    ) -> Result<ShardVersion> {
        let UpdatedTableInfo {
            shard_info: curr_shard_info,
            table_info: new_table,
        } = updated_info;

        ensure!(
            !self.is_frozen(),
            UpdateFrozenShard {
                shard_id: curr_shard_info.id,
            }
        );

        ensure!(
            self.shard_info.version == curr_shard_info.version,
            ShardVersionMismatch {
                shard_info: self.shard_info.clone(),
                expect_version: curr_shard_info.version,
            }
        );

        let table_idx = self
            .tables
            .iter()
            .position(|v| v.id == new_table.id)
            .with_context(|| TableNotFound {
                msg: format!("the table to remove is not found, table:{new_table:?}"),
            })?;

        // Remove the table from the shard.
        self.tables.swap_remove(table_idx);

        // Update the shard version if necessary.
        if inc_version {
            self.inc_shard_version();
        }

        Ok(self.shard_info.version)
    }
}

pub type ShardDataRef = Arc<std::sync::RwLock<ShardData>>;
