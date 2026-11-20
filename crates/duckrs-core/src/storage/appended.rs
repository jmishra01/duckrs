// Ported from the appends of duckdb/src/storage/local_storage.cpp and optimistic_data_writer.cpp @ rust-port-baseline
//
//! A table whose rows are parts one after another: rows of another source (a file, or a layer over one), row groups
//! that an append wrote to blocks ahead of a checkpoint (`OptimisticDataWriter`), and rows still in memory. C++ keeps
//! these as the row groups of one `RowGroupCollection`, persistent ones and ones in memory; here each part has its
//! groups, and the table's are theirs in turn. What is in blocks is read from them when a scan needs it, so that an
//! append of more rows than fit in memory goes through.

use super::writer::WrittenGroup;
use crate::catalog::{ColumnStatistics, ColumnZone, RowIds, SegmentInfo, TableSource};
use duckrs_common::column_data::ColumnDataCollection;
use duckrs_common::types::LogicalType;
use duckrs_common::vector::{DataChunk, SelectionVector, StringCopy, Vector};
use duckrs_common::{Result, STANDARD_VECTOR_SIZE};
use std::sync::Arc;

/// A part of an [`AppendedTable`].
#[derive(Debug, Clone)]
pub enum AppendedPart {
    /// all the rows of a source, in its groups
    Source(Arc<dyn TableSource>),
    /// a row group in blocks
    Written(Arc<WrittenGroup>),
    /// rows in memory, in groups of the table's size
    Memory(Arc<ColumnDataCollection>),
}

#[derive(Debug, Clone)]
pub struct AppendedTable {
    parts: Vec<AppendedPart>,
    /// per group, its part, which group of the part it is, and its rows
    groups: Vec<(usize, usize, usize)>,
    ids: RowIds,
    types: Vec<LogicalType>,
    group_rows: usize,
}

/// Rows `start..end` of `rows`, columns `columns`, as one chunk.
pub fn rows_between(rows: &ColumnDataCollection, start: usize, end: usize, columns: &[usize]) -> DataChunk {
    let count = end - start;
    let types = rows.types();
    let mut vectors: Vec<Vector> = columns
        .iter()
        .map(|c| Vector::new(types[*c].clone(), count.max(1)))
        .collect();
    let mut chunk_index = start / STANDARD_VECTOR_SIZE;
    let mut at = 0;
    while chunk_index * STANDARD_VECTOR_SIZE < end && chunk_index < rows.chunk_count() {
        let chunk = rows.chunk(chunk_index);
        let chunk_start = chunk_index * STANDARD_VECTOR_SIZE;
        let from = start.saturating_sub(chunk_start);
        let to = (end - chunk_start).min(chunk.len());
        if from < to {
            let selection = SelectionVector::from_vec((from as u32..to as u32).collect());
            for (vector, column) in vectors.iter_mut().zip(columns) {
                vector.copy_from(chunk.column(*column), &selection, to - from, at, StringCopy::Deep);
            }
            at += to - from;
        }
        chunk_index += 1;
    }
    DataChunk::from_columns(vectors, count)
}

/// Rows `start..end` of `rows` as a collection of their own.
pub fn collection_between(rows: &ColumnDataCollection, start: usize, end: usize) -> ColumnDataCollection {
    if let Some(shared) = rows.shared_range(start, end) {
        return shared;
    }
    let mut result = ColumnDataCollection::new(rows.types().to_vec());
    let columns: Vec<usize> = (0..rows.types().len()).collect();
    let mut at = start;
    while at < end {
        let next = (at + STANDARD_VECTOR_SIZE).min(end);
        result.append(&rows_between(rows, at, next, &columns));
        at = next;
    }
    result
}

impl AppendedTable {
    /// Of `parts`, none of which is an `AppendedTable` itself, of a table with rows `ids` and groups of `group_rows`.
    pub fn new(parts: Vec<AppendedPart>, ids: RowIds, types: Vec<LogicalType>, group_rows: usize) -> Self {
        let mut groups = Vec::new();
        for (index, part) in parts.iter().enumerate() {
            match part {
                AppendedPart::Source(source) => {
                    debug_assert!(source.as_any().downcast_ref::<AppendedTable>().is_none());
                    for group in 0..source.group_count() {
                        groups.push((index, group, source.group_rows(group)));
                    }
                }
                AppendedPart::Written(written) => groups.push((index, 0, written.rows)),
                AppendedPart::Memory(rows) => {
                    for (group, start) in (0..rows.len()).step_by(group_rows.max(1)).enumerate() {
                        groups.push((index, group, (rows.len() - start).min(group_rows)));
                    }
                }
            }
        }
        // the row groups a checkpoint keeps as they are: those of a file it has, and those in blocks
        let mut stored = Vec::new();
        let mut position = 0;
        for part in &parts {
            match part {
                AppendedPart::Source(source) => {
                    stored.extend_from_slice(source.row_ids().stored_groups());
                    position += source.row_count();
                }
                AppendedPart::Written(written) => {
                    stored.push((written.row_start, written.rows));
                    position += written.rows;
                }
                AppendedPart::Memory(rows) => position += rows.len(),
            }
        }
        debug_assert_eq!(position, ids.len());
        let ids = ids.with_stored_groups(stored);
        Self {
            parts,
            groups,
            ids,
            types,
            group_rows,
        }
    }

    /// The parts of `source`: itself if it isn't appended to.
    pub fn parts_of(source: &Arc<dyn TableSource>) -> Vec<AppendedPart> {
        match source.as_any().downcast_ref::<AppendedTable>() {
            Some(appended) => appended.parts.clone(),
            None => vec![AppendedPart::Source(source.clone())],
        }
    }

    pub fn parts(&self) -> &[AppendedPart] {
        &self.parts
    }

    /// The row groups in blocks, with the position of the first row of each.
    pub fn written_groups(&self) -> Vec<(usize, Arc<WrittenGroup>)> {
        let mut result = Vec::new();
        let mut position = 0;
        for part in &self.parts {
            match part {
                AppendedPart::Source(source) => position += source.row_count(),
                AppendedPart::Written(written) => {
                    result.push((position, written.clone()));
                    position += written.rows;
                }
                AppendedPart::Memory(rows) => position += rows.len(),
            }
        }
        result
    }

    /// The source in front of the rest, if it is one.
    pub fn base(&self) -> Option<&Arc<dyn TableSource>> {
        match self.parts.first() {
            Some(AppendedPart::Source(source)) => Some(source),
            _ => None,
        }
    }
}

impl TableSource for AppendedTable {
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
        self.groups.len()
    }

    fn group_rows(&self, group: usize) -> usize {
        self.groups[group].2
    }

    fn read_group(&self, group: usize, columns: &[usize]) -> Result<DataChunk> {
        let (part, inner, rows) = self.groups[group];
        match &self.parts[part] {
            AppendedPart::Source(source) => source.read_group(inner, columns),
            AppendedPart::Written(written) => {
                let vectors = columns
                    .iter()
                    .map(|column| written.read_column(*column))
                    .collect::<Result<Vec<_>>>()?;
                Ok(DataChunk::from_columns(vectors, rows))
            }
            AppendedPart::Memory(memory) => {
                let start = inner * self.group_rows;
                Ok(rows_between(memory, start, start + rows, columns))
            }
        }
    }

    fn read_all(&self) -> Result<ColumnDataCollection> {
        let columns: Vec<usize> = (0..self.types.len()).collect();
        let mut result = ColumnDataCollection::new(self.types.clone());
        for group in 0..self.groups.len() {
            let chunk = self.read_group(group, &columns)?;
            let mut at = 0;
            while at < chunk.len() {
                let next = (at + STANDARD_VECTOR_SIZE).min(chunk.len());
                let mut part = chunk.clone();
                part.slice(
                    &SelectionVector::from_vec((at as u32..next as u32).collect()),
                    next - at,
                );
                result.append(&part);
                at = next;
            }
        }
        Ok(result)
    }

    /// Of row groups in blocks alone, theirs; of others, unknown, so that they are worked out from the rows. So are
    /// those of strings, whose bounds a file cuts short, where rows that were in memory until now had them exact.
    fn column_statistics(&self) -> Option<Vec<ColumnStatistics>> {
        use duckrs_common::types::LogicalTypeId;
        if self
            .types
            .iter()
            .any(|ty| matches!(ty.id(), LogicalTypeId::Varchar | LogicalTypeId::Blob))
        {
            return None;
        }
        let mut groups = Vec::with_capacity(self.parts.len());
        for part in &self.parts {
            match part {
                AppendedPart::Written(group) => groups.push(group.clone()),
                // a table that has no rows in its file says nothing there
                AppendedPart::Source(source) if source.row_count() == 0 => {}
                _ => return None,
            }
        }
        super::writer::written_statistics(&groups)
    }

    fn zones(&self, column: usize) -> Result<Vec<ColumnZone>> {
        let unknown = || ColumnZone {
            unknown: true,
            ..ColumnZone::default()
        };
        let mut result = Vec::with_capacity(self.groups.len());
        for part in &self.parts {
            match part {
                AppendedPart::Source(source) => result.extend(source.zones(column)?),
                AppendedPart::Written(written) => result.push(written.zone(column)?),
                AppendedPart::Memory(rows) => {
                    result.extend((0..rows.len()).step_by(self.group_rows.max(1)).map(|_| unknown()));
                }
            }
        }
        Ok(result)
    }

    /// Of the rows in memory, what C++ lists of a column in memory: a segment and its validity per row group.
    fn segments(&self, include_info: bool, loaded_only: bool) -> Result<Vec<SegmentInfo>> {
        let mut result = Vec::new();
        let mut group = 0u64;
        for part in &self.parts {
            match part {
                AppendedPart::Source(source) => {
                    for mut segment in source.segments(include_info, loaded_only)? {
                        segment.row_group += group;
                        result.push(segment);
                    }
                    group += source.group_count() as u64;
                }
                AppendedPart::Written(written) => {
                    let buffer = include_info.then(|| written.buffer());
                    result.extend(written.segments(buffer.as_deref(), group)?);
                    group += 1;
                }
                AppendedPart::Memory(rows) => {
                    for start in (0..rows.len()).step_by(self.group_rows.max(1)) {
                        let count = (rows.len() - start).min(self.group_rows) as u64;
                        for (column, ty) in self.types.iter().enumerate() {
                            for (path, segment_type) in [
                                (format!("[{column}]"), ty.to_string()),
                                (format!("[{column}, 0]"), "VALIDITY".to_string()),
                            ] {
                                result.push(SegmentInfo {
                                    row_group: group,
                                    column,
                                    path,
                                    segment: 0,
                                    segment_type,
                                    start: 0,
                                    count,
                                    compression: "Uncompressed",
                                    block_id: -1,
                                    block_offset: 0,
                                    info: None,
                                });
                            }
                        }
                        group += 1;
                    }
                }
            }
        }
        Ok(result)
    }
}
