// Ported from duckdb/extension/parquet/parquet_page_index.cpp and the page index parts of parquet_reader.cpp
// (`ParquetReader::PreparePageIndex`) and column_reader.cpp (`VerifyPageAgainstOffsetIndex`)
// @ rust-port-baseline
//
//! The page index of a Parquet file (its ColumnIndex and OffsetIndex): the pages of a row group whose bounds rule
//! out every row a filter on one column is true for are not read. The rows of the pages that may match, intersected
//! over the filtered columns, are what the reader is told to read of the row group.

use super::{ParquetScanData, ThriftReader, ThriftValue, read_thrift_struct, statistic_text, try_cast_value};
use crate::catalog::ColumnZone;
use crate::planner::expression::{BoundExpr, BoundExprKind};
use duckrs_common::types::{LogicalType, LogicalTypeId};
use duckrs_common::value::Value;
use duckrs_common::{Error, ErrorKind, Result};
use parquet::file::metadata::{ColumnChunkMetaData, ParquetMetaData, RowGroupMetaData};
use std::fs::File;

/// `ParquetPageRowRange`: the rows `[begin, end)` of a row group that may still hold matching rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RowRange {
    pub begin: u64,
    pub end: u64,
}

/// `PageLocation` of an OffsetIndex.
#[derive(Debug, Clone, Copy)]
struct PageLocation {
    offset: i64,
    compressed_page_size: i64,
    first_row_index: i64,
}

/// The pages of a column chunk, by its OffsetIndex; `None` where it has none, or it cannot be read.
fn offset_index(file: &mut File, chunk: &ColumnChunkMetaData) -> Option<Vec<PageLocation>> {
    let (offset, length) = (chunk.offset_index_offset()?, chunk.offset_index_length()?);
    if offset <= 0 || length <= 0 {
        return None;
    }
    let index = read_thrift_struct(file, offset, length)?;
    index
        .field(1)?
        .items()
        .iter()
        .map(|location| {
            Some(PageLocation {
                offset: location.field(1)?.int()?,
                compressed_page_size: location.field(2)?.int()?,
                first_row_index: location.field(3)?.int()?,
            })
        })
        .collect()
}

/// `HasColumnIndex` and `HasOffsetIndex`.
fn has_page_index(chunk: &ColumnChunkMetaData) -> bool {
    let positive =
        |offset: Option<i64>, length: Option<i32>| offset.is_some_and(|o| o > 0) && length.is_some_and(|l| l > 0);
    positive(chunk.column_index_offset(), chunk.column_index_length())
        && positive(chunk.offset_index_offset(), chunk.offset_index_length())
}

/// `ParquetPageIndexUtils::OffsetIndexIsValid`: consecutive pages that start at the first row and lie inside the
/// byte range of their own column chunk.
fn offset_index_is_valid(locations: &[PageLocation], rows: u64, file_size: u64, chunk: &ColumnChunkMetaData) -> bool {
    if locations.first().is_none_or(|first| first.first_row_index != 0) {
        return false;
    }
    let mut begin = chunk.data_page_offset();
    if let Some(dictionary) = chunk.dictionary_page_offset().filter(|offset| *offset >= 4) {
        begin = begin.min(dictionary);
    }
    if begin < 0 || chunk.compressed_size() < 0 {
        return false;
    }
    let end = begin as u64 + chunk.compressed_size() as u64;
    let begin = begin as u64;
    for (at, location) in locations.iter().enumerate() {
        if location.offset <= 0 || location.compressed_page_size <= 0 || location.first_row_index < 0 {
            return false;
        }
        let (offset, size) = (location.offset as u64, location.compressed_page_size as u64);
        if offset + size > file_size || offset < begin || offset + size > end || location.first_row_index as u64 >= rows
        {
            return false;
        }
        if at > 0 {
            // pages are stored in row order, so both the rows and the bytes have to increase
            let previous = &locations[at - 1];
            if location.first_row_index <= previous.first_row_index
                || offset < previous.offset as u64 + previous.compressed_page_size as u64
            {
                return false;
            }
        }
    }
    true
}

/// `PageRowCount`.
fn page_rows(locations: &[PageLocation], page: usize, rows: u64) -> u64 {
    let begin = locations[page].first_row_index as u64;
    let end = locations.get(page + 1).map_or(rows, |next| next.first_row_index as u64);
    end - begin
}

/// `PagesToRowRanges`.
fn pages_to_row_ranges(locations: &[PageLocation], may_match: &[bool], rows: u64) -> Vec<RowRange> {
    let mut result: Vec<RowRange> = Vec::new();
    for (page, _) in may_match.iter().enumerate().filter(|(_, may)| **may) {
        let begin = locations[page].first_row_index as u64;
        let end = begin + page_rows(locations, page, rows);
        match result.last_mut() {
            // consecutive pages form one range
            Some(last) if last.end == begin => last.end = end,
            _ => result.push(RowRange { begin, end }),
        }
    }
    result
}

/// `IntersectRowRanges`.
fn intersect(left: &[RowRange], right: &[RowRange]) -> Vec<RowRange> {
    let (mut l, mut r) = (0, 0);
    let mut result = Vec::new();
    while l < left.len() && r < right.len() {
        let begin = left[l].begin.max(right[r].begin);
        let end = left[l].end.min(right[r].end);
        if begin < end {
            result.push(RowRange { begin, end });
        }
        if left[l].end < right[r].end {
            l += 1;
        } else {
            r += 1;
        }
    }
    result
}

/// The columns `expr` reads.
fn referenced(expr: &BoundExpr, columns: &mut Vec<usize>) {
    if let BoundExprKind::Reference(at) = expr.kind
        && !columns.contains(&at)
    {
        columns.push(at);
    }
    for child in expr.children() {
        referenced(child, columns);
    }
}

/// The conjuncts of `filter` that read one column each, by that column (`ParquetReaderScanState::scan_filters`).
pub(super) fn column_filters(filter: &BoundExpr) -> Vec<(usize, Vec<&BoundExpr>)> {
    fn conjuncts<'a>(expr: &'a BoundExpr, out: &mut Vec<&'a BoundExpr>) {
        match &expr.kind {
            BoundExprKind::Conjunction { and: true, children } => children.iter().for_each(|c| conjuncts(c, out)),
            _ => out.push(expr),
        }
    }
    let mut all = Vec::new();
    conjuncts(filter, &mut all);
    let mut result: Vec<(usize, Vec<&BoundExpr>)> = Vec::new();
    for conjunct in all {
        let mut columns = Vec::new();
        referenced(conjunct, &mut columns);
        let [column] = columns.as_slice() else {
            continue;
        };
        match result.iter_mut().find(|(c, _)| c == column) {
            Some((_, exprs)) => exprs.push(conjunct),
            None => result.push((*column, vec![conjunct])),
        }
    }
    result
}

/// The bytes a bound of a page has to have for its physical type; `None` for byte arrays.
fn fixed_width(descriptor: &parquet::schema::types::ColumnDescriptor) -> Option<usize> {
    use parquet::basic::Type as Physical;
    Some(match descriptor.physical_type() {
        Physical::BOOLEAN => 1,
        Physical::INT32 | Physical::FLOAT => 4,
        Physical::INT64 | Physical::DOUBLE => 8,
        Physical::INT96 => 12,
        Physical::FIXED_LEN_BYTE_ARRAY => usize::try_from(descriptor.type_length()).ok()?,
        Physical::BYTE_ARRAY => return None,
    })
}

/// What the ColumnIndex says of each page of a column chunk (`FilterPages`): `Err` where its bounds cannot be read
/// as the column's type, which makes the page index of the row group unusable.
fn page_zones(
    index: &ThriftValue,
    descriptor: &parquet::schema::types::ColumnDescriptor,
    ty: &LogicalType,
    can_have_nan: bool,
) -> std::result::Result<Vec<Option<ColumnZone>>, ()> {
    let null_pages = index.field(1).map(ThriftValue::items).unwrap_or(&[]);
    let mins = index.field(2).map(ThriftValue::items).unwrap_or(&[]);
    let maxes = index.field(3).map(ThriftValue::items).unwrap_or(&[]);
    let null_counts = index.field(5).map(ThriftValue::items);
    let width = fixed_width(descriptor);
    let float = matches!(ty.id(), LogicalTypeId::Float | LogicalTypeId::Double);
    let mut zones = Vec::with_capacity(null_pages.len());
    for page in 0..null_pages.len() {
        if matches!(null_pages[page], ThriftValue::Bool(true)) {
            // a page that holds only NULL values
            zones.push(Some(ColumnZone {
                has_null: true,
                ..ColumnZone::default()
            }));
            continue;
        }
        let (ThriftValue::Binary(min), ThriftValue::Binary(max)) = (&mins[page], &maxes[page]) else {
            return Err(());
        };
        if width.is_some_and(|width| min.len() != width || max.len() != width) {
            return Err(());
        }
        let bound = |bytes: &[u8]| -> Option<Value> {
            if ty.id() == LogicalTypeId::Varchar && std::str::from_utf8(bytes).is_err() {
                return None;
            }
            try_cast_value(&Value::varchar(statistic_text(descriptor, bytes)?), ty)
        };
        let nulls = null_counts
            .and_then(|counts| counts.get(page))
            .and_then(ThriftValue::int);
        let mut zone = ColumnZone {
            has_null: nulls.is_none_or(|n| n > 0),
            ..ColumnZone::default()
        };
        match (bound(min), bound(max)) {
            (Some(min), Some(max)) => {
                // NaN is left out of the bounds of floats, and is larger than any other value to DuckDB
                let max = if float && can_have_nan {
                    match try_cast_value(&Value::varchar("nan"), ty) {
                        Some(nan) => nan,
                        None => {
                            zones.push(None);
                            continue;
                        }
                    }
                } else {
                    max
                };
                zone.min = Some(min);
                zone.max = Some(max);
            }
            _ => zone.unknown = true,
        }
        zones.push(Some(zone));
    }
    Ok(zones)
}

/// The leaves of the file that `scan` reads its filtered columns from, which have a page index if any.
pub(super) struct PageIndexColumns {
    /// by filtered column of the scan: its leaf
    leaves: Vec<Option<usize>>,
}

impl PageIndexColumns {
    pub(super) fn new(scan: &ParquetScanData, metadata: &ParquetMetaData, path: &str, filter: &BoundExpr) -> Self {
        let descr = metadata.file_metadata().schema_descr();
        let mut leaves = vec![None; scan.names.len()];
        for (column, _) in column_filters(filter) {
            if column >= scan.names.len() || scan.types[column].is_nested() {
                continue;
            }
            leaves[column] = super::statistics_leaf(scan, metadata, column, path)
                .filter(|&leaf| descr.column(leaf).max_rep_level() == 0);
        }
        Self { leaves }
    }
}

/// `ParquetReader::PreparePageIndex` of row group `group`: the rows of it that may match `filter`, or `None` where
/// the page index rules no page out (or it is unusable). `file_size` is that of the file `file` at `path`.
#[allow(clippy::too_many_arguments)]
pub(super) fn row_ranges(
    scan: &ParquetScanData,
    columns: &PageIndexColumns,
    metadata: &ParquetMetaData,
    group: &RowGroupMetaData,
    filter: &BoundExpr,
    file: &mut File,
    path: &str,
    file_size: u64,
    read_leaves: &[usize],
) -> Result<Option<Vec<RowRange>>> {
    if !scan.page_pruning.use_page_index || group.num_rows() <= 0 {
        return Ok(None);
    }
    let rows = group.num_rows() as u64;
    let descr = metadata.file_metadata().schema_descr();
    let mut ranges: Option<Vec<RowRange>> = None;
    let mut any_pruned = false;
    for (column, exprs) in column_filters(filter) {
        let Some(leaf) = columns.leaves.get(column).copied().flatten() else {
            continue;
        };
        let chunk = group.column(leaf);
        if !has_page_index(chunk) {
            continue;
        }
        // a page index that cannot be read is ignored: the row group is read as if it had none
        let (Some(offset), Some(length)) = (chunk.column_index_offset(), chunk.column_index_length()) else {
            continue;
        };
        let Some(index) = read_thrift_struct(file, offset, length) else {
            return Ok(None);
        };
        let pages = index.field(1).map(ThriftValue::items).unwrap_or(&[]).len();
        let consistent = |id: i16| index.field(id).map(ThriftValue::items).unwrap_or(&[]).len() == pages;
        if !consistent(2) || !consistent(3) || index.field(5).is_some() && !consistent(5) {
            continue;
        }
        let ty = &scan.types[column];
        // `CanHaveNaN`: a nan_count of the column chunk says, else the scan's option does
        let can_have_nan = chunk
            .statistics()
            .and_then(parquet::file::statistics::Statistics::nan_count_opt)
            .map_or(scan.page_pruning.can_have_nan, |nans| nans != 0);
        let Ok(zones) = page_zones(&index, &descr.column(leaf), ty, can_have_nan) else {
            return Ok(None);
        };
        let may_match: Vec<bool> = zones
            .iter()
            .map(|zone| {
                let Some(zone) = zone else {
                    return true;
                };
                let zone_of = |at: usize| (at == column && !zone.unknown).then(|| zone.clone());
                !exprs
                    .iter()
                    .any(|expr| crate::execution::zone_map::never_true(&zone_of, expr))
            })
            .collect();
        any_pruned |= may_match.iter().any(|may| !may);
        let Some(locations) = offset_index(file, chunk) else {
            return Ok(None);
        };
        if !offset_index_is_valid(&locations, rows, file_size, chunk) || locations.len() != may_match.len() {
            continue;
        }
        let column_ranges = pages_to_row_ranges(&locations, &may_match, rows);
        ranges = Some(match ranges {
            None => column_ranges,
            Some(ranges) => intersect(&ranges, &column_ranges),
        });
    }
    let Some(ranges) = ranges.filter(|_| any_pruned) else {
        return Ok(None);
    };
    if ranges.iter().map(|r| r.end - r.begin).sum::<u64>() == rows {
        return Ok(None);
    }
    if !ranges.is_empty() {
        verify_pages(path, metadata, group, file, file_size, read_leaves, &ranges)?;
    }
    Ok(Some(ranges))
}

/// `ColumnReader::VerifyPageAgainstOffsetIndex` for the pages the scan reads of the columns it reads: a data page
/// holds one value for each row of it that the OffsetIndex promises.
fn verify_pages(
    path: &str,
    metadata: &ParquetMetaData,
    group: &RowGroupMetaData,
    file: &mut File,
    file_size: u64,
    read_leaves: &[usize],
    ranges: &[RowRange],
) -> Result<()> {
    let descr = metadata.file_metadata().schema_descr();
    for &leaf in read_leaves {
        let column = descr.column(leaf);
        if column.max_rep_level() != 0 {
            continue;
        }
        let chunk = group.column(leaf);
        let rows = chunk.num_values().max(0) as u64;
        let Some(locations) = offset_index(file, chunk) else {
            continue;
        };
        if !offset_index_is_valid(&locations, group.num_rows().max(0) as u64, file_size, chunk) {
            continue;
        }
        for (page, location) in locations.iter().enumerate() {
            let begin = location.first_row_index as u64;
            let expected = page_rows(&locations, page, rows);
            let overlaps = ranges.iter().any(|r| r.begin < begin + expected && begin < r.end);
            if !overlaps {
                continue;
            }
            let Some(values) = data_page_values(file, location.offset as u64) else {
                continue;
            };
            if values != expected {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!(
                        "Failed to read file \"{path}\": the OffsetIndex of column \"{}\" is corrupt. Page {page} \
                         should hold {expected} rows according to the index, but holds {values} values. The file can \
                         be read without its page index: SET parquet_use_page_index = false",
                        column.name()
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// The values of the data page whose header is at `offset`; `None` for other pages, or a header that cannot be
/// read.
fn data_page_values(file: &mut File, offset: u64) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    for size in [256usize, 4096, 1 << 16, 1 << 20] {
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut bytes = Vec::with_capacity(size);
        file.by_ref().take(size as u64).read_to_end(&mut bytes).ok()?;
        let truncated = bytes.len() < size;
        if let Some(header) = (ThriftReader { bytes: &bytes, at: 0 }).structure() {
            return match header.field(1)?.int()? {
                // DATA_PAGE and DATA_PAGE_V2
                0 => Some(header.field(5)?.field(1)?.int()?.max(0) as u64),
                3 => Some(header.field(8)?.field(1)?.int()?.max(0) as u64),
                _ => None,
            };
        }
        if truncated {
            return None;
        }
    }
    None
}
