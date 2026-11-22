// Ported from the per-column parts of duckdb/src/storage/table/row_group.cpp (`RowGroup::AddColumn`,
// `RemoveColumn`, `AlterType`, `ColumnData::Update`) @ rust-port-baseline
//
//! A table whose columns are each either one of another source, read when something needs it, or
//! held in memory: what UPDATE and ALTER leave of a table in a file. C++ keeps the `ColumnData` of a
//! row group that nothing changed and loads it on demand; a column that an UPDATE or an ALTER
//! wrote is in memory, and a DELETE only says which rows of the source are left (its version info). Here the rows stay where the source has them, a group at a time, so that a
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
    /// in memory: per group, the values of the rows the group has left. An UPDATE changes the column of the
    /// source `from` in the groups it updates, and the others are still that column (`None`); a column an ALTER
    /// makes has all its groups.
    Memory {
        groups: Arc<Vec<Option<Vector>>>,
        from: Option<usize>,
    },
}

#[derive(Debug, Clone)]
pub struct LayeredTable {
    base: Arc<dyn TableSource>,
    columns: Vec<Layer>,
    types: Vec<LogicalType>,
    /// Per group, the rows of it in the source that are left, ascending, where a DELETE took some
    /// (`RowVersionManager::Delete`); `None` where they all are.
    alive: Arc<Vec<Option<Arc<[u32]>>>>,
    ids: RowIds,
}

impl LayeredTable {
    /// Of `base`, which isn't layered itself, with columns `columns` of types `types`.
    pub fn new(base: Arc<dyn TableSource>, columns: Vec<Layer>, types: Vec<LogicalType>) -> Self {
        debug_assert!(base.as_any().downcast_ref::<LayeredTable>().is_none());
        let alive = Arc::new(vec![None; base.group_count()]);
        let ids = base.row_ids();
        Self {
            base,
            columns,
            types,
            alive,
            ids,
        }
    }

    /// `source` as a layer: itself if it is one, else one of all its `types` as they are there.
    pub fn of(source: &Arc<dyn TableSource>, types: Vec<LogicalType>) -> Self {
        match source.as_any().downcast_ref::<LayeredTable>() {
            Some(layered) => layered.clone(),
            None => {
                let columns = (0..types.len()).map(Layer::Base).collect();
                Self::new(source.clone(), columns, types)
            }
        }
    }

    /// The same rows with columns `columns` of types `types` (an ALTER).
    pub fn with_columns(mut self, columns: Vec<Layer>, types: Vec<LogicalType>) -> Self {
        self.columns = columns;
        self.types = types;
        self
    }

    /// The source the columns of it that are not in memory are of.
    pub fn base(&self) -> &Arc<dyn TableSource> {
        &self.base
    }

    pub fn layers(&self) -> &[Layer] {
        &self.columns
    }

    pub fn set_layer(&mut self, column: usize, layer: Layer) {
        self.columns[column] = layer;
    }

    /// The rows of a group in the source that are left, if a DELETE took some.
    pub fn alive_rows(&self, group: usize) -> Option<&[u32]> {
        self.alive[group].as_deref()
    }

    /// A column of a group, as many rows as it has left: in memory, or read from the source.
    pub fn column(&self, column: usize, group: usize) -> Result<Vector> {
        match (&self.columns[column], self.in_base(column, group)) {
            (_, Some(at)) => self.base_column(group, at),
            (Layer::Memory { groups, .. }, None) => Ok(groups[group].clone().expect("in memory")),
            (Layer::Base(_), None) => unreachable!("a column of the source is in it"),
        }
    }

    /// `column` of the source, of the rows of the group that are left.
    fn base_column(&self, group: usize, column: usize) -> Result<Vector> {
        match self.alive_rows(group) {
            None => Ok(self.base.read_group(group, &[column])?.column(0).clone()),
            Some(rows) => self.base.fetch_rows(group, column, rows),
        }
    }

    /// Where in the source a column of a group is, if it is there.
    fn in_base(&self, column: usize, group: usize) -> Option<usize> {
        match &self.columns[column] {
            Layer::Base(base) => Some(*base),
            Layer::Memory { groups, from } => groups[group].is_none().then_some(*from).flatten(),
        }
    }

    /// Of the row at a position, its group, and where in the group's rows that are left it is.
    pub fn locator(&self) -> impl Fn(usize) -> (usize, usize) + use<> {
        let mut starts = Vec::with_capacity(self.group_count() + 1);
        let mut start = 0;
        for group in 0..self.group_count() {
            starts.push(start);
            start += self.group_rows(group);
        }
        starts.push(start);
        move |position| {
            let group = starts.partition_point(|start| *start <= position) - 1;
            (group, position - starts[group])
        }
    }

    /// `DataTable::Delete`: the rows at `positions` (ascending) go. Their values stay in the source; the columns in
    /// memory lose them.
    pub fn delete(&mut self, positions: &[usize]) {
        let mut by_group: Vec<Vec<u32>> = vec![Vec::new(); self.group_count()];
        let locate = self.locator();
        for &position in positions {
            let (group, row) = locate(position);
            by_group[group].push(row as u32);
        }
        let mut alive = (*self.alive).clone();
        let mut columns = self.columns.clone();
        for (group, gone) in by_group.iter().enumerate().filter(|(_, gone)| !gone.is_empty()) {
            let rows = self.group_rows(group) as u32;
            let mut gone = gone.iter().peekable();
            let stay: Vec<u32> = (0..rows)
                .filter(|row| {
                    let deleted = gone.peek() == Some(&row);
                    if deleted {
                        gone.next();
                    }
                    !deleted
                })
                .collect();
            for layer in &mut columns {
                let Layer::Memory { groups, .. } = layer else { continue };
                let Some(vector) = &groups[group] else { continue };
                let mut vector = vector.clone();
                vector.slice(&SelectionVector::from_vec(stay.clone()), stay.len());
                Arc::make_mut(groups)[group] = Some(vector);
            }
            alive[group] = Some(match &alive[group] {
                None => stay.into(),
                Some(before) => stay.iter().map(|row| before[*row as usize]).collect(),
            });
        }
        self.alive = Arc::new(alive);
        self.columns = columns;
        self.ids.remove(positions);
    }
}

impl TableSource for LayeredTable {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn row_count(&self) -> usize {
        self.ids.len()
    }

    fn row_ids(&self) -> RowIds {
        self.ids.clone()
    }

    fn group_count(&self) -> usize {
        self.base.group_count()
    }

    fn group_rows(&self, group: usize) -> usize {
        self.alive_rows(group)
            .map_or_else(|| self.base.group_rows(group), <[u32]>::len)
    }

    fn read_group(&self, group: usize, columns: &[usize]) -> Result<DataChunk> {
        let wanted: Vec<usize> = columns
            .iter()
            .filter_map(|column| self.in_base(*column, group))
            .collect();
        let rows = self.group_rows(group);
        let mut from_base = if wanted.is_empty() {
            Vec::new()
        } else if let Some(alive) = self.alive_rows(group) {
            let mut fetched = Vec::with_capacity(wanted.len());
            for column in &wanted {
                fetched.push(self.base.fetch_rows(group, *column, alive)?);
            }
            fetched
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
            return match self.alive_rows(group) {
                None => self.base.fetch_rows(group, base, rows),
                Some(alive) => {
                    let rows: Vec<u32> = rows.iter().map(|row| alive[*row as usize]).collect();
                    self.base.fetch_rows(group, base, &rows)
                }
            };
        }
        let mut vector = self.column(column, group)?;
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

    /// `DistinctStatistics` stay as they are through a DELETE, as in C++: while every column is the file's, the
    /// file's. A column a change put in memory has none, so then they are counted again from the rows.
    fn distinct_states(&self) -> Option<Vec<Option<crate::optimizer::distinct_statistics::DistinctState>>> {
        let base = self.base.distinct_states()?;
        let states = self.columns.iter().map(|layer| match layer {
            Layer::Base(column) => base.get(*column).cloned(),
            Layer::Memory { .. } => None,
        });
        states.collect()
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
