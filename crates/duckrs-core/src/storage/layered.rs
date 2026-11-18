// Ported from the per-column parts of duckdb/src/storage/table/row_group.cpp (`RowGroup::AddColumn`,
// `RemoveColumn`, `AlterType`, `ColumnData::Update`) @ rust-port-baseline
//
//! A table whose columns are each either one of another source, read when something needs it, or
//! held in memory: what UPDATE and ALTER leave of a table in a file. C++ keeps the `ColumnData` of a
//! row group that nothing changed and loads it on demand; a column that an UPDATE or an ALTER
//! wrote is in memory. Here the rows stay where the source has them, a group at a time, so that a
//! column changed in memory is a vector per group of the rows the group has.

use crate::catalog::{ColumnStatistics, ColumnZone, RowIds, SegmentInfo, TableSource};
use duckrs_common::Result;
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::types::LogicalType;
use duckrs_common::vector::{DataChunk, SelectionVector, Vector};
use std::sync::Arc;

/// Where a column of a [`LayeredTable`] is.
#[derive(Debug, Clone)]
pub enum Layer {
    /// that column of the source, as it is there
    Base(usize),
    /// in memory: per group, the values of the rows the group has. An UPDATE changes the column of the source
    /// `from` in the groups it updates, and the others are still that column (`None`); a column an ALTER makes
    /// has all its groups.
    Memory {
        groups: Arc<Vec<Option<Vector>>>,
        from: Option<usize>,
    },
}

#[derive(Debug)]
pub struct LayeredTable {
    base: Arc<dyn TableSource>,
    columns: Vec<Layer>,
    types: Vec<LogicalType>,
}

impl LayeredTable {
    /// Of `base`, which isn't layered itself, with columns `columns` of types `types`.
    pub fn new(base: Arc<dyn TableSource>, columns: Vec<Layer>, types: Vec<LogicalType>) -> Self {
        debug_assert!(base.as_any().downcast_ref::<LayeredTable>().is_none());
        Self { base, columns, types }
    }

    /// What `source`, of `columns` columns, is a layer of, and where each of its columns is in that.
    pub fn parts(source: &Arc<dyn TableSource>, columns: usize) -> (Arc<dyn TableSource>, Vec<Layer>) {
        match source.as_any().downcast_ref::<LayeredTable>() {
            Some(layered) => (layered.base.clone(), layered.columns.clone()),
            None => (source.clone(), (0..columns).map(Layer::Base).collect()),
        }
    }

    /// The source the columns of it that are not in memory are of.
    pub fn base(&self) -> &Arc<dyn TableSource> {
        &self.base
    }

    pub fn layers(&self) -> &[Layer] {
        &self.columns
    }

    /// A column of a group: in memory, or read from the source.
    pub fn column_of(base: &dyn TableSource, layer: &Layer, group: usize) -> Result<Vector> {
        match layer {
            Layer::Base(column) => Ok(base.read_group(group, &[*column])?.column(0).clone()),
            Layer::Memory { groups, from } => match (&groups[group], from) {
                (Some(vector), _) => Ok(vector.clone()),
                (None, Some(column)) => Ok(base.read_group(group, &[*column])?.column(0).clone()),
                (None, None) => unreachable!("a column an ALTER makes has all its groups"),
            },
        }
    }

    /// Where in the source a column of a group is, if it is there.
    fn in_base(&self, column: usize, group: usize) -> Option<usize> {
        match &self.columns[column] {
            Layer::Base(base) => Some(*base),
            Layer::Memory { groups, from } => groups[group].is_none().then_some(*from).flatten(),
        }
    }
}

impl TableSource for LayeredTable {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn row_count(&self) -> usize {
        self.base.row_count()
    }

    fn row_ids(&self) -> RowIds {
        self.base.row_ids()
    }

    fn group_count(&self) -> usize {
        self.base.group_count()
    }

    fn group_rows(&self, group: usize) -> usize {
        self.base.group_rows(group)
    }

    fn read_group(&self, group: usize, columns: &[usize]) -> Result<DataChunk> {
        let wanted: Vec<usize> = columns
            .iter()
            .filter_map(|column| self.in_base(*column, group))
            .collect();
        let rows = self.base.group_rows(group);
        let mut from_base = if wanted.is_empty() {
            Vec::new()
        } else {
            self.base.read_group(group, &wanted)?.columns().to_vec()
        }
        .into_iter();
        let vectors = columns
            .iter()
            .map(|column| match (self.in_base(*column, group), &self.columns[*column]) {
                (Some(_), _) => from_base.next().expect("read with the others"),
                (None, Layer::Memory { groups, .. }) => groups[group].clone().expect("in memory"),
                (None, Layer::Base(_)) => unreachable!("a column of the source is in it"),
            })
            .collect();
        Ok(DataChunk::from_columns(vectors, rows))
    }

    fn fetch_rows(&self, group: usize, column: usize, rows: &[u32]) -> Result<Vector> {
        if let Some(base) = self.in_base(column, group) {
            return self.base.fetch_rows(group, base, rows);
        }
        let mut vector = Self::column_of(&*self.base, &self.columns[column], group)?;
        vector.slice(&SelectionVector::from_vec(rows.to_vec()), rows.len());
        Ok(vector)
    }

    fn read_all(&self) -> Result<ColumnDataCollection> {
        let columns: Vec<usize> = (0..self.columns.len()).collect();
        let mut result = ColumnDataCollection::new(self.types.clone());
        for group in 0..self.group_count() {
            result.append(&self.read_group(group, &columns)?);
        }
        Ok(result)
    }

    fn column_statistics(&self) -> Option<Vec<ColumnStatistics>> {
        let base = self.base.column_statistics()?;
        let statistics = self.columns.iter().map(|layer| match layer {
            Layer::Base(column) => base[*column].clone(),
            Layer::Memory { .. } => ColumnStatistics::unknown(),
        });
        Some(statistics.collect())
    }

    /// What the file says of the groups still there, and nothing of those in memory.
    fn zones(&self, column: usize) -> Result<Vec<ColumnZone>> {
        let unknown = || ColumnZone {
            unknown: true,
            ..ColumnZone::default()
        };
        match &self.columns[column] {
            Layer::Base(base) => self.base.zones(*base),
            Layer::Memory {
                groups,
                from: Some(from),
            } if groups.iter().any(Option::is_none) => {
                let zones = self.base.zones(*from)?;
                Ok(groups
                    .iter()
                    .zip(zones)
                    .map(|(group, zone)| if group.is_none() { zone } else { unknown() })
                    .collect())
            }
            Layer::Memory { .. } => Ok((0..self.group_count()).map(|_| unknown()).collect()),
        }
    }

    /// Those of the source of each column, an updated one too, and of a column it doesn't have, what C++ lists of
    /// a column in memory: a segment and its validity per row group.
    fn segments(&self, include_info: bool, loaded_only: bool) -> Result<Vec<SegmentInfo>> {
        let base = self.base.segments(include_info, loaded_only)?;
        let mut result = Vec::new();
        let mut start = 0;
        for group in 0..self.group_count() {
            let rows = self.base.group_rows(group) as u64;
            for (column, layer) in self.columns.iter().enumerate() {
                let from = match layer {
                    Layer::Base(base) => Some(*base),
                    Layer::Memory { from, .. } => *from,
                };
                let Some(from) = from else {
                    let ty = &self.types[column];
                    for (path, segment_type) in [
                        (format!("[{column}]"), ty.to_string()),
                        (format!("[{column}, 0]"), "VALIDITY".to_string()),
                    ] {
                        result.push(SegmentInfo {
                            row_group: group as u64,
                            column,
                            path,
                            segment: 0,
                            segment_type,
                            start,
                            count: rows,
                            compression: "Uncompressed",
                            block_id: -1,
                            block_offset: 0,
                            info: None,
                        });
                    }
                    continue;
                };
                let prefix = format!("[{from}");
                for segment in base.iter().filter(|s| s.row_group == group as u64 && s.column == from) {
                    let mut segment = segment.clone();
                    segment.column = column;
                    segment.path = format!("[{column}{}", &segment.path[prefix.len()..]);
                    result.push(segment);
                }
            }
            start += rows;
        }
        Ok(result)
    }
}
