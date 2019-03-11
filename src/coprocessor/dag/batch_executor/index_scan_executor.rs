// Copyright 2019 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use cop_datatype::EvalType;
use kvproto::coprocessor::KeyRange;
use tipb::expression::FieldType;
use tipb::schema::ColumnInfo;

use crate::storage::Store;

use super::interface::*;
use crate::coprocessor::codec::batch::{LazyBatchColumn, LazyBatchColumnVec};
use crate::coprocessor::dag::expr::{EvalConfig, EvalContext};
use crate::coprocessor::dag::Scanner;
use crate::coprocessor::{Error, Result};

pub struct BatchIndexScanExecutor<C: ExecSummaryCollector, S: Store>(
    super::scan_executor::ScanExecutor<
        C,
        S,
        IndexScanExecutorImpl,
        super::ranges_iter::PointRangeConditional,
    >,
);

impl<C: ExecSummaryCollector, S: Store> BatchIndexScanExecutor<C, S> {
    pub fn new(
        summary_collector: C,
        store: S,
        config: Arc<EvalConfig>,
        columns_info: Vec<ColumnInfo>,
        key_ranges: Vec<KeyRange>,
        desc: bool,
        unique: bool,
        // TODO: this does not mean that it is a unique index scan. What does it mean?
    ) -> Result<Self> {
        let mut schema = Vec::with_capacity(columns_info.len());
        let mut columns_len_without_handle = 0;
        let mut decode_handle = false;
        for ci in &columns_info {
            schema.push(super::scan_executor::field_type_from_column_info(&ci));
            if ci.get_pk_handle() {
                decode_handle = true;
            } else {
                columns_len_without_handle += 1;
            }
        }

        let imp = IndexScanExecutorImpl {
            context: EvalContext::new(config),
            schema,
            columns_len_without_handle,
            decode_handle,
        };
        let wrapper = super::scan_executor::ScanExecutor::new(
            summary_collector,
            imp,
            store,
            desc,
            key_ranges,
            super::ranges_iter::PointRangeConditional::new(unique),
        )?;
        Ok(Self(wrapper))
    }
}

impl<C: ExecSummaryCollector, S: Store> BatchExecutor for BatchIndexScanExecutor<C, S> {
    #[inline]
    fn schema(&self) -> &[FieldType] {
        self.0.schema()
    }

    #[inline]
    fn next_batch(&mut self, expect_rows: usize) -> BatchExecuteResult {
        self.0.next_batch(expect_rows)
    }

    #[inline]
    fn collect_statistics(&mut self, destination: &mut BatchExecuteStatistics) {
        self.0.collect_statistics(destination);
    }
}

struct IndexScanExecutorImpl {
    /// See `TableScanExecutorImpl`'s `context`.
    context: EvalContext,

    /// See `TableScanExecutorImpl`'s `schema`.
    schema: Vec<FieldType>,

    /// Number of interested columns (exclude PK handle column).
    columns_len_without_handle: usize,

    /// Whether PK handle column is interested. Handle will be always placed in the last column.
    decode_handle: bool,
}

impl super::scan_executor::ScanExecutorImpl for IndexScanExecutorImpl {
    #[inline]
    fn schema(&self) -> &[FieldType] {
        &self.schema
    }

    #[inline]
    fn mut_context(&mut self) -> &mut EvalContext {
        &mut self.context
    }

    #[inline]
    fn build_scanner<S: Store>(
        &self,
        store: &S,
        desc: bool,
        range: KeyRange,
    ) -> Result<Scanner<S>> {
        Scanner::new(
            store,
            crate::coprocessor::dag::ScanOn::Index,
            desc,
            false,
            range,
        )
    }

    fn build_column_vec(&self, expect_rows: usize) -> LazyBatchColumnVec {
        // Construct empty columns, with PK in decoded format and the rest in raw format.

        let columns_len = self.schema.len();
        let mut columns = Vec::with_capacity(columns_len);
        for _ in 0..self.columns_len_without_handle {
            columns.push(LazyBatchColumn::raw_with_capacity(expect_rows));
        }
        if self.decode_handle {
            // For primary key, we construct a decoded `VectorValue` because it is directly
            // stored as i64, without a datum flag, in the value (for unique index).
            // Note that for normal index, primary key is appended at the end of key with a
            // datum flag.
            columns.push(LazyBatchColumn::decoded_with_capacity_and_tp(
                expect_rows,
                EvalType::Int,
            ));
        }

        LazyBatchColumnVec::from(columns)
    }

    fn process_kv_pair(
        &mut self,
        key: &[u8],
        mut value: &[u8],
        columns: &mut LazyBatchColumnVec,
    ) -> Result<()> {
        use crate::coprocessor::codec::{datum, table};
        use crate::util::codec::number;
        use byteorder::{BigEndian, ReadBytesExt};

        // The payload part of the key
        let mut key_payload = &key[table::PREFIX_LEN + table::ID_LEN..];

        for i in 0..self.columns_len_without_handle {
            let (val, remaining) = datum::split_datum(key_payload, false)?;
            columns[i].push_raw(val);
            key_payload = remaining;
        }

        if self.decode_handle {
            // For normal index, it is placed at the end and any columns prior to it are
            // ensured to be interested. For unique index, it is placed in the value.
            let handle_val = if key_payload.is_empty() {
                // This is a unique index, and we should look up PK handle in value.

                // NOTE: it is not `number::decode_i64`.
                value.read_i64::<BigEndian>().map_err(|_| {
                    Error::Other(box_err!("Failed to decode handle in value as i64"))
                })?
            } else {
                // This is a normal index. The remaining payload part is the PK handle.
                // Let's decode it and put in the column.

                let flag = key_payload[0];
                let mut val = &key_payload[1..];

                match flag {
                    datum::INT_FLAG => number::decode_i64(&mut val).map_err(|_| {
                        Error::Other(box_err!("Failed to decode handle in key as i64"))
                    })?,
                    datum::UINT_FLAG => {
                        (number::decode_u64(&mut val).map_err(|_| {
                            Error::Other(box_err!("Failed to decode handle in key as u64"))
                        })?) as i64
                    }
                    _ => {
                        return Err(Error::Other(box_err!("Unexpected handle flag {}", flag)));
                    }
                }
            };

            columns[self.columns_len_without_handle]
                .mut_decoded()
                .push_int(Some(handle_val));
        }

        Ok(())
    }
}

/*
#[cfg(test)]
mod tests {
    use std::i64;

    use kvproto::kvrpcpb::IsolationLevel;

    use crate::storage::SnapshotStore;

    use super::*;
    use crate::coprocessor::dag::scanner::tests::{
        get_point_range, get_range, prepare_table_data, TestStore,
    };

    const TABLE_ID: i64 = 1;
    const KEY_NUMBER: usize = 100;

    #[test]
    fn test_point_get() {
        let test_data = prepare_table_data(KEY_NUMBER, TABLE_ID);
        let mut test_store = TestStore::new(&test_data.kv_data);
        let context = {
            let columns_info = test_data.get_prev_2_cols();
            BatchExecutorContext::with_default_config(columns_info)
        };

        const HANDLE: i64 = 0;

        // point get returns none
        let r1 = get_point_range(TABLE_ID, i64::MIN);
        // point get return something
        let r2 = get_point_range(TABLE_ID, HANDLE);
        let ranges = vec![r1, r2];

        let (snapshot, start_ts) = test_store.get_snapshot();
        let store = SnapshotStore::new(snapshot, start_ts, IsolationLevel::SI, true);
        let mut table_scanner =
            BatchTableScanExecutor::new(store, context.clone(), ranges,false).unwrap();

        let result = table_scanner.next_batch(10);
        assert!(result.error.is_none());
        assert_eq!(result.data.columns_len(), 2);
        assert_eq!(result.data.rows_len(), 1);

        let expect_row = &test_data.expect_rows[HANDLE as usize];
        for (idx, col) in context.columns_info.iter().enumerate() {
            let cid = col.get_column_id();
            let v = &result.data[idx].raw()[0];
            assert_eq!(expect_row[&cid].as_slice(), v.as_slice());
        }
    }

    #[test]
    fn test_multiple_ranges() {
        use rand::Rng;

        let test_data = prepare_table_data(KEY_NUMBER, TABLE_ID);
        let mut test_store = TestStore::new(&test_data.kv_data);
        let context = {
            let mut columns_info = test_data.get_prev_2_cols();
            columns_info.push(test_data.get_col_pk());
            BatchExecutorContext::with_default_config(columns_info)
        };

        let r1 = get_range(TABLE_ID, i64::MIN, 0);
        let r2 = get_range(TABLE_ID, 0, (KEY_NUMBER / 2) as i64);
        let r3 = get_point_range(TABLE_ID, (KEY_NUMBER / 2) as i64);
        let r4 = get_range(TABLE_ID, (KEY_NUMBER / 2) as i64 + 1, i64::MAX);
        let ranges = vec![r1, r2, r3, r4];

        let (snapshot, start_ts) = test_store.get_snapshot();
        let store = SnapshotStore::new(snapshot, start_ts, IsolationLevel::SI, true);
        let mut table_scanner =
            BatchTableScanExecutor::new(context.clone(), false, ranges, store).unwrap();

        let mut data = table_scanner.next_batch(0).data;
        {
            let mut rng = rand::thread_rng();
            loop {
                let mut result = table_scanner.next_batch(rng.gen_range(1, KEY_NUMBER / 5));
                assert!(result.error.is_none());
                if result.data.rows_len() == 0 {
                    break;
                }
                data.append(&mut result.data);
            }
        }
        assert_eq!(data.columns_len(), 3);
        assert_eq!(data.rows_len(), KEY_NUMBER);

        for row_index in 0..KEY_NUMBER {
            // data[2] should be PK column, let's check it first.
            assert_eq!(
                data[2].decoded().as_int_slice()[row_index],
                Some(row_index as i64)
            );
            // check rest columns
            let expect_row = &test_data.expect_rows[row_index];
            for (col_index, col) in context.columns_info.iter().enumerate() {
                if col.get_pk_handle() {
                    continue;
                }
                let cid = col.get_column_id();
                let v = &data[col_index].raw()[row_index];
                assert_eq!(expect_row[&cid].as_slice(), v.as_slice());
            }
        }
    }
}
*/
