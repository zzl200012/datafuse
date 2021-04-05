// Copyright 2020-2021 The Datafuse Authors.
//
// SPDX-License-Identifier: Apache-2.0.

use std::any::Any;
use std::convert::TryInto;
use std::fs::File;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use common_arrow::parquet::arrow::{ArrowReader, ParquetFileArrowReader};
use common_arrow::parquet::file::reader::SerializedFileReader;
use common_datablocks::DataBlock;
use common_datavalues::DataSchemaRef;
use common_planners::{Partition, PlanNode, ReadDataSourcePlan, Statistics, TableOptions};
use common_streams::{ParquetStream, SendableDataBlockStream};
use crossbeam::channel::{bounded, Receiver, Sender};
use tokio::task;

use crate::datasources::ITable;
use crate::sessions::FuseQueryContextRef;

pub struct ParquetTable {
    db: String,
    name: String,
    schema: DataSchemaRef,
    file: String,
}

impl ParquetTable {
    pub fn try_create(
        db: String,
        name: String,
        schema: DataSchemaRef,
        options: TableOptions,
    ) -> Result<Box<dyn ITable>> {
        let file = options.get("location");
        return match file {
            Some(file) => {
                let table = ParquetTable {
                    db,
                    name,
                    schema,
                    file: file.trim_matches(|s| s == '\'' || s == '"').to_string(),
                };
                Ok(Box::new(table))
            }
            _ => bail!("Parquet Engine must contains file location options"),
        };
    }
}

fn read_file(
    file: &str,
    tx: Sender<Option<Result<DataBlock>>>,
    projection: &[usize],
) -> Result<()> {
    let file_reader = File::open(file)?;
    let file_reader = SerializedFileReader::new(file_reader)?;
    let mut arrow_reader = ParquetFileArrowReader::new(Arc::new(file_reader));

    // TODO projection, row filters, batch size configurable, schema judgement
    let batch_size = 2048;
    let mut batch_reader =
        arrow_reader.get_record_reader_by_columns(projection.to_owned(), batch_size)?;

    loop {
        match batch_reader.next() {
            Some(Ok(batch)) => {
                tx.send(Some(Ok(batch.try_into()?)))
                    .map_err(|e| anyhow!(e.to_string()))?;
            }
            None => {
                break;
            }
            Some(Err(e)) => {
                let err_msg = format!("Error reading batch from {:?}: {}", file, e.to_string());

                tx.send(Some(Err(anyhow!(err_msg.clone()))))
                    .map_err(|e| anyhow!(e.to_string()))?;
                bail!(err_msg);
            }
        }
    }
    Ok(())
}

#[async_trait]
impl ITable for ParquetTable {
    fn name(&self) -> &str {
        &self.name
    }

    fn engine(&self) -> &str {
        "Parquet"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> Result<DataSchemaRef> {
        Ok(self.schema.clone())
    }

    fn read_plan(
        &self,
        _ctx: FuseQueryContextRef,
        _push_down_plan: PlanNode,
    ) -> Result<ReadDataSourcePlan> {
        Ok(ReadDataSourcePlan {
            db: self.db.clone(),
            table: self.name().to_string(),
            schema: self.schema.clone(),
            partitions: vec![Partition {
                name: "".to_string(),
                version: 0,
            }],
            statistics: Statistics::default(),
            description: format!(
                "(Read from Parquet Engine table  {}.{})",
                self.db, self.name
            ),
        })
    }

    async fn read(&self, _ctx: FuseQueryContextRef) -> Result<SendableDataBlockStream> {
        type BlockSender = Sender<Option<Result<DataBlock>>>;
        type BlockReceiver = Receiver<Option<Result<DataBlock>>>;

        let (response_tx, response_rx): (BlockSender, BlockReceiver) = bounded(2);

        let file = self.file.clone();
        let projection: Vec<usize> = (0..self.schema.fields().len()).collect();
        task::spawn_blocking(move || {
            if let Err(e) = read_file(&file, response_tx, &projection) {
                println!("Parquet reader thread terminated due to error: {:?}", e);
            }
        });

        Ok(Box::pin(ParquetStream::try_create(response_rx)?))
    }
}