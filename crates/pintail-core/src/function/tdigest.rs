// Ported from duckdb/third_party/tdigest/t_digest.hpp @ rust-port-baseline
//
//! Merging t-digest (Derrick R. Burns' implementation, as vendored by DuckDB), used by
//! `approx_quantile`.
//!
//! The arithmetic is kept in the same order and in `f64` throughout so results match the C++
//! bit for bit. The C++ capacity management (`Grow`, `reserveForProcess`, the arena allocator)
//! only affects allocation, never results, and is not ported.

use std::cmp::Ordering;

/// `kHighWater`: total centroid count at which a multi-digest merge flushes a batch.
const HIGH_WATER: usize = 40000;
/// `pi` in the C++ header (`3.14159265358979323846`), which rounds to exactly this double.
const PI: f64 = std::f64::consts::PI;

/// `std::min(a, b)`: returns `b` only if `b < a`.
#[inline]
fn std_min(a: f64, b: f64) -> f64 {
    if b < a { b } else { a }
}

/// `std::max(a, b)`: returns `b` only if `a < b`.
#[inline]
fn std_max(a: f64, b: f64) -> f64 {
    if a < b { b } else { a }
}

/// A weighted mean. Port of `duckdb_tdigest::Centroid`.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Centroid {
    mean: f64,
    weight: f64,
}

impl Centroid {
    /// `Centroid(Value mean, Weight weight)`.
    pub fn new(mean: f64, weight: f64) -> Self {
        Centroid { mean, weight }
    }

    /// `Centroid::mean`.
    #[inline]
    pub fn mean(&self) -> f64 {
        self.mean
    }

    /// `Centroid::weight`.
    #[inline]
    pub fn weight(&self) -> f64 {
        self.weight
    }

    /// `Centroid::add`: folds `c` into this centroid.
    #[inline]
    pub fn add(&mut self, c: &Centroid) {
        if self.weight != 0.0 {
            self.weight += c.weight;
            self.mean += c.weight * (c.mean - self.mean) / self.weight;
        } else {
            self.weight = c.weight;
            self.mean = c.mean;
        }
    }
}

/// `CentroidComparator` as a total order for sorting/merging (`a.mean() < b.mean()`); means are
/// never NaN (`add` rejects NaN), so this is consistent.
#[inline]
fn cmp_mean(a: &Centroid, b: &Centroid) -> Ordering {
    if a.mean < b.mean {
        Ordering::Less
    } else if b.mean < a.mean {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}

/// Emulation of `std::priority_queue` (`std::push_heap` / `std::pop_heap`) as implemented by
/// libc++ (LLVM >= 15, the Apple toolchain): sift-up on push, Floyd sift-down on pop that prefers
/// the left child on ties. The order in which equal-priority elements leave the queue is what
/// makes this matter: it decides how equal-mean centroids of different weight are interleaved.
/// libstdc++ breaks child ties towards the right child, so merges of three or more equal-mean
/// lists can differ between C++ standard libraries; with at most two entries (DuckDB's only use:
/// merging one digest into another) both libraries behave identically.
struct StdPriorityQueue<T, F: Fn(&T, &T) -> bool> {
    heap: Vec<T>,
    /// The `Compare` of the priority queue: `less(a, b)` means `a` has lower priority.
    less: F,
}

impl<T: Copy, F: Fn(&T, &T) -> bool> StdPriorityQueue<T, F> {
    fn new(less: F) -> Self {
        StdPriorityQueue { heap: Vec::new(), less }
    }

    fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    fn top(&self) -> T {
        self.heap[0]
    }

    /// libc++ `__sift_up` over `heap[..len]`, for the element at `len - 1`.
    fn sift_up(&mut self, len: usize) {
        if len > 1 {
            let mut last = len - 1;
            let mut parent = (len - 2) / 2;
            if (self.less)(&self.heap[parent], &self.heap[last]) {
                let t = self.heap[last];
                loop {
                    self.heap[last] = self.heap[parent];
                    last = parent;
                    if parent == 0 {
                        break;
                    }
                    parent = (parent - 1) / 2;
                    if !(self.less)(&self.heap[parent], &t) {
                        break;
                    }
                }
                self.heap[last] = t;
            }
        }
    }

    /// `priority_queue::push` = `push_back` + `std::push_heap`.
    fn push(&mut self, value: T) {
        self.heap.push(value);
        let len = self.heap.len();
        self.sift_up(len);
    }

    /// `priority_queue::pop` = `std::pop_heap` + `pop_back` (libc++ Floyd variant).
    fn pop(&mut self) {
        let len = self.heap.len();
        if len > 1 {
            let top = self.heap[0];
            // __floyd_sift_down
            let mut hole = 0usize;
            loop {
                let mut child = 2 * hole + 1;
                if child + 1 < len && (self.less)(&self.heap[child], &self.heap[child + 1]) {
                    child += 1;
                }
                self.heap[hole] = self.heap[child];
                hole = child;
                if child > (len - 2) / 2 {
                    break;
                }
            }
            let last = len - 1;
            if hole == last {
                self.heap[hole] = top;
            } else {
                self.heap[hole] = self.heap[last];
                self.heap[last] = top;
                self.sift_up(hole + 1);
            }
        }
        self.heap.pop();
    }
}

/// Port of `duckdb_tdigest::TDigest`.
#[derive(Clone, Debug)]
pub struct TDigest {
    compression: f64,
    /// Initialized to `std::numeric_limits<double>::max()`.
    min: f64,
    /// Initialized to `std::numeric_limits<double>::min()`, i.e. the smallest *positive* normal
    /// double (`f64::MIN_POSITIVE`), not `-f64::MAX`. Kept for fidelity: `max` is only ever
    /// raised by `std::max`, so a digest of only negative values reports `max() ==
    /// f64::MIN_POSITIVE` unless overridden by `set_min_max`.
    max: f64,
    max_processed: usize,
    max_unprocessed: usize,
    processed_weight: f64,
    unprocessed_weight: f64,
    processed: Vec<Centroid>,
    unprocessed: Vec<Centroid>,
    cumulative: Vec<f64>,
}

impl Default for TDigest {
    /// `TDigest(allocator)`: the default compression is 1000.
    fn default() -> Self {
        TDigest::new(1000.0)
    }
}

#[allow(
    dead_code,
    reason = "the whole API of C++'s `TDigest`, of which the quantile functions use a part"
)]
impl TDigest {
    /// `TDigest(allocator, compression)` (DuckDB's `approx_quantile` uses compression 100).
    pub fn new(compression: f64) -> Self {
        TDigest::with_sizes(compression, 0, 0)
    }

    /// `TDigest(allocator, compression, unmergedSize, mergedSize)`; a size of 0 picks the default
    /// (`processedSize` / `unprocessedSize`).
    pub fn with_sizes(compression: f64, unmerged_size: usize, merged_size: usize) -> Self {
        TDigest {
            compression,
            min: f64::MAX,
            max: f64::MIN_POSITIVE,
            max_processed: Self::processed_size(merged_size, compression),
            max_unprocessed: Self::unprocessed_size(unmerged_size, compression),
            processed_weight: 0.0,
            unprocessed_weight: 0.0,
            processed: Vec::new(),
            unprocessed: Vec::new(),
            cumulative: Vec::new(),
        }
    }

    /// `TDigest(processed, unprocessed, compression, unmergedSize, mergedSize)`: rebuilds a digest
    /// from centroid lists. `processed` must be sorted by mean. Weights are recomputed, `min`/`max`
    /// are taken from the first/last processed centroid (unprocessed centroids do not contribute).
    pub fn from_centroids(
        processed: Vec<Centroid>,
        unprocessed: Vec<Centroid>,
        compression: f64,
        unmerged_size: usize,
        merged_size: usize,
    ) -> Self {
        let mut td = TDigest::with_sizes(compression, unmerged_size, merged_size);
        td.processed = processed;
        td.unprocessed = unprocessed;
        td.processed_weight = Self::weight_of(&td.processed);
        td.unprocessed_weight = Self::weight_of(&td.unprocessed);
        if let (Some(first), Some(last)) = (td.processed.first(), td.processed.last()) {
            td.min = std_min(td.min, first.mean);
            td.max = std_max(td.max, last.mean);
        }
        td.update_cumulative();
        td
    }

    /// Import path used by `ApproxQuantileImportState`: `TDigest(std::move(centroids), {},
    /// compression, 0, 0)` followed by `setMinMax(min, max)`.
    pub fn from_parts(compression: f64, min: f64, max: f64, centroids: Vec<Centroid>) -> Self {
        let mut td = TDigest::from_centroids(centroids, Vec::new(), compression, 0, 0);
        td.set_min_max(min, max);
        td
    }

    /// `TDigest::weight(centroids)`: sequential sum of the weights.
    pub fn weight_of(centroids: &[Centroid]) -> f64 {
        let mut w = 0.0;
        for c in centroids {
            w += c.weight;
        }
        w
    }

    /// `TDigest::processedSize`.
    #[inline]
    pub fn processed_size(size: usize, compression: f64) -> usize {
        if size == 0 {
            (2.0 * compression.ceil()) as usize
        } else {
            size
        }
    }

    /// `TDigest::unprocessedSize`.
    #[inline]
    pub fn unprocessed_size(size: usize, compression: f64) -> usize {
        if size == 0 {
            (8.0 * compression.ceil()) as usize
        } else {
            size
        }
    }

    /// `TDigest::merge(const TDigest *other)`.
    pub fn merge(&mut self, other: &TDigest) {
        self.merge_many(&[other]);
    }

    /// `TDigest::add(std::vector<const TDigest *>)`: merges several digests, smallest
    /// `total_size()` first, in batches flushed at `kHighWater` centroids.
    pub fn merge_many(&mut self, digests: &[&TDigest]) {
        if digests.is_empty() {
            return;
        }
        // TDigestComparator: left->totalSize() > right->totalSize(), so the top is the smallest.
        let mut pq = StdPriorityQueue::new(|l: &&TDigest, r: &&TDigest| l.total_size() > r.total_size());
        for &td in digests {
            pq.push(td);
        }
        let mut batch: Vec<&TDigest> = Vec::with_capacity(digests.len());
        let mut total_size = 0usize;
        while !pq.is_empty() {
            let td = pq.top();
            batch.push(td);
            pq.pop();
            total_size += td.total_size();
            if total_size >= HIGH_WATER || pq.is_empty() {
                self.merge_processed(&batch);
                self.merge_unprocessed(&batch);
                self.process_if_necessary();
                batch.clear();
                total_size = 0;
            }
        }
        self.update_cumulative();
    }

    /// `TDigest::processed`.
    pub fn processed(&self) -> &[Centroid] {
        &self.processed
    }

    /// The processed centroids (sorted by mean). Call [`compress`](Self::compress) first to fold
    /// in unprocessed values, as `ApproxQuantileExportState` does.
    pub fn centroids(&self) -> &[Centroid] {
        &self.processed
    }

    /// `TDigest::unprocessed`.
    pub fn unprocessed(&self) -> &[Centroid] {
        &self.unprocessed
    }

    /// `TDigest::maxUnprocessed`.
    pub fn max_unprocessed(&self) -> usize {
        self.max_unprocessed
    }

    /// `TDigest::maxProcessed`.
    pub fn max_processed(&self) -> usize {
        self.max_processed
    }

    /// `TDigest::min`.
    pub fn min(&self) -> f64 {
        self.min
    }

    /// `TDigest::max`.
    pub fn max(&self) -> f64 {
        self.max
    }

    /// `TDigest::setMinMax`: overrides the tracked extremes (used when importing a state).
    pub fn set_min_max(&mut self, min: f64, max: f64) {
        self.min = min;
        self.max = max;
    }

    /// `TDigest::processedWeight`.
    pub fn processed_weight(&self) -> f64 {
        self.processed_weight
    }

    /// `TDigest::unprocessedWeight`.
    pub fn unprocessed_weight(&self) -> f64 {
        self.unprocessed_weight
    }

    /// `TDigest::haveUnprocessed`.
    pub fn have_unprocessed(&self) -> bool {
        !self.unprocessed.is_empty()
    }

    /// `TDigest::totalSize`: number of processed plus unprocessed centroids.
    pub fn total_size(&self) -> usize {
        self.processed.len() + self.unprocessed.len()
    }

    /// Total weight as a double (`processedWeight_ + unprocessedWeight_`).
    pub fn total_weight(&self) -> f64 {
        self.processed_weight + self.unprocessed_weight
    }

    /// `TDigest::totalWeight` exactly as C++ returns it: truncated to `long`.
    pub fn total_weight_truncated(&self) -> i64 {
        (self.processed_weight + self.unprocessed_weight) as i64
    }

    /// `TDigest::cdf`: processes pending values if needed, then [`cdf_processed`](Self::cdf_processed).
    pub fn cdf(&mut self, x: f64) -> f64 {
        if self.have_unprocessed() || self.is_dirty() {
            self.process();
        }
        self.cdf_processed(x)
    }

    /// `TDigest::isDirty`.
    pub fn is_dirty(&self) -> bool {
        self.processed.len() > self.max_processed || self.unprocessed.len() > self.max_unprocessed
    }

    /// `TDigest::cdfProcessed`: the cdf over the processed centroids only.
    pub fn cdf_processed(&self, x: f64) -> f64 {
        let min = self.min;
        let max = self.max;
        if self.processed.is_empty() {
            0.0
        } else if self.processed.len() == 1 {
            let width = max - min;
            if x < min {
                0.0
            } else if x > max {
                1.0
            } else if x - min <= width {
                0.5
            } else {
                (x - min) / (max - min)
            }
        } else {
            let n = self.processed.len();
            if x <= min {
                return 0.0;
            }
            if x >= max {
                return 1.0;
            }
            if x <= self.mean(0) {
                if self.mean(0) - min > 0.0 {
                    return (x - min) / (self.mean(0) - min) * self.weight(0) / self.processed_weight / 2.0;
                } else {
                    return 0.0;
                }
            }
            if x >= self.mean(n - 1) {
                if max - self.mean(n - 1) > 0.0 {
                    return 1.0
                        - (max - x) / (max - self.mean(n - 1)) * self.weight(n - 1) / self.processed_weight / 2.0;
                } else {
                    return 1.0;
                }
            }
            // std::upper_bound with CentroidComparator: first centroid with x < mean.
            #[allow(clippy::neg_cmp_op_on_partial_ord)]
            let i = self.processed.partition_point(|c| !(x < c.mean));
            let z1 = x - self.processed[i - 1].mean;
            let z2 = self.processed[i].mean - x;
            Self::weighted_average(self.cumulative[i - 1], z2, self.cumulative[i], z1) / self.processed_weight
        }
    }

    /// `TDigest::quantile`: processes pending values if needed, then
    /// [`quantile_processed`](Self::quantile_processed).
    pub fn quantile(&mut self, q: f64) -> f64 {
        if self.have_unprocessed() || self.is_dirty() {
            self.process();
        }
        self.quantile_processed(q)
    }

    /// `TDigest::quantileProcessed`: a quantile over the processed centroids only. NaN for
    /// `q` outside `[0, 1]` or an empty digest.
    pub fn quantile_processed(&self, q: f64) -> f64 {
        // Not `!(0.0..=1.0).contains(&q)`: a NaN `q` must fall through, as in C++.
        #[allow(clippy::manual_range_contains)]
        let out_of_range = q < 0.0 || q > 1.0;
        if out_of_range {
            return f64::NAN;
        }
        if self.processed.is_empty() {
            return f64::NAN;
        } else if self.processed.len() == 1 {
            return self.mean(0);
        }
        let n = self.processed.len();
        let index = q * self.processed_weight;
        if index <= self.weight(0) / 2.0 {
            return self.min + 2.0 * index / self.weight(0) * (self.mean(0) - self.min);
        }
        // std::lower_bound: first cumulative entry not less than index; it is >= 1 here since
        // cumulative[0] == weight(0) / 2 < index.
        let i = self.cumulative.partition_point(|&c| c < index);
        // C++ tests `iter + 1 != cumulative_.cend()`, i.e. i != n (cumulative has n + 1 entries).
        // i == n + 1 (index above every cumulative entry through rounding) would read out of bounds
        // in C++; it is routed to the tail branch here.
        if i < n {
            let z1 = index - self.cumulative[i - 1];
            let z2 = self.cumulative[i] - index;
            return Self::weighted_average(self.mean(i - 1), z2, self.mean(i), z1);
        }
        let z1 = index - self.processed_weight - self.weight(n - 1) / 2.0;
        let z2 = self.weight(n - 1) / 2.0 - z1;
        Self::weighted_average(self.mean(n - 1), z1, self.max, z2)
    }

    /// `TDigest::compression`.
    pub fn compression(&self) -> f64 {
        self.compression
    }

    /// `TDigest::compress`: folds all unprocessed values into the processed centroids.
    pub fn compress(&mut self) {
        self.process();
    }

    /// `TDigest::add(Value x)`: adds `x` with weight 1.
    pub fn add_value(&mut self, x: f64) {
        self.add(x, 1.0);
    }

    /// `TDigest::add(Value x, Weight w)`: buffers a centroid, processing when the buffers overflow.
    /// Returns `false` (and ignores the value) for NaN. Note that `min`/`max` are only updated
    /// when the buffer is processed.
    pub fn add(&mut self, x: f64, w: f64) -> bool {
        if x.is_nan() {
            return false;
        }
        self.unprocessed.push(Centroid::new(x, w));
        self.unprocessed_weight += w;
        self.process_if_necessary();
        true
    }

    /// `TDigest::add(iter, end)` over a centroid range: appends centroids to the unprocessed
    /// buffer, processing each time it fills up.
    ///
    /// Faithful to the C++, this does **not** add the centroids' weight to the unprocessed weight,
    /// so the processed weight ends up too small. DuckDB does not call it.
    pub fn add_centroids(&mut self, centroids: &[Centroid]) {
        let mut iter = 0usize;
        let end = centroids.len();
        while iter != end {
            let diff = end - iter;
            // size_t arithmetic: wraps (to a huge room) if the buffer is already over capacity.
            let room = self.max_unprocessed.wrapping_sub(self.unprocessed.len());
            let mid = iter + diff.min(room);
            self.unprocessed.extend_from_slice(&centroids[iter..mid]);
            iter = mid;
            if self.unprocessed.len() >= self.max_unprocessed {
                self.process();
            }
        }
    }

    /// `TDigest::mean(i)`.
    #[inline]
    fn mean(&self, i: usize) -> f64 {
        self.processed[i].mean
    }

    /// `TDigest::weight(i)`.
    #[inline]
    fn weight(&self, i: usize) -> f64 {
        self.processed[i].weight
    }

    /// `TDigest::mergeUnprocessed`: appends every digest's unprocessed centroids, in batch order.
    fn merge_unprocessed(&mut self, tdigests: &[&TDigest]) {
        for td in tdigests {
            self.unprocessed.extend_from_slice(&td.unprocessed);
            self.unprocessed_weight += td.unprocessed_weight;
        }
    }

    /// `TDigest::mergeProcessed`: k-way merge of the batch's processed centroids and our own
    /// through a `std::priority_queue` (see [`StdPriorityQueue`] for the tie order).
    fn merge_processed(&mut self, tdigests: &[&TDigest]) {
        if tdigests.is_empty() {
            return;
        }
        let own = std::mem::take(&mut self.processed);
        let mut lists: Vec<&[Centroid]> = Vec::with_capacity(tdigests.len() + 1);
        let mut total = 0usize;
        for td in tdigests {
            let sorted = &td.processed;
            if !sorted.is_empty() {
                lists.push(sorted);
                total += sorted.len();
                self.processed_weight += td.processed_weight;
            }
        }
        if total == 0 {
            self.processed = own;
            return;
        }
        if !own.is_empty() {
            lists.push(&own);
            total += own.len();
        }
        // CentroidList = (list, position); CentroidListComparator: left.mean > right.mean.
        let mean_at = |e: &(usize, usize)| lists[e.0][e.1].mean;
        let mut pq = StdPriorityQueue::new(|l: &(usize, usize), r: &(usize, usize)| mean_at(l) > mean_at(r));
        for (li, _) in lists.iter().enumerate() {
            pq.push((li, 0usize));
        }
        let mut sorted = Vec::with_capacity(total);
        while !pq.is_empty() {
            let best = pq.top();
            pq.pop();
            sorted.push(lists[best.0][best.1]);
            let next = (best.0, best.1 + 1);
            if next.1 != lists[best.0].len() {
                pq.push(next);
            }
        }
        drop(pq);
        self.processed = sorted;
        if let (Some(first), Some(last)) = (self.processed.first(), self.processed.last()) {
            self.min = std_min(self.min, first.mean);
            self.max = std_max(self.max, last.mean);
        }
    }

    /// `TDigest::processIfNecessary`.
    #[inline]
    fn process_if_necessary(&mut self) {
        if self.is_dirty() {
            self.process();
        }
    }

    /// `TDigest::updateCumulative`: midpoint-cumulative weights, `n + 1` entries.
    fn update_cumulative(&mut self) {
        self.cumulative.clear();
        let mut previous = 0.0;
        for c in &self.processed {
            let current = c.weight;
            let half_current = current / 2.0;
            self.cumulative.push(previous + half_current);
            previous += current;
        }
        self.cumulative.push(previous);
    }

    /// `TDigest::process`: merges the unprocessed and processed centroids and re-compresses them.
    ///
    /// Sorting: C++ uses `std::sort` (unstable) on the unprocessed buffer, then `std::inplace_merge`
    /// (stable; on ties the sorted unprocessed centroids precede the processed ones). A stable sort
    /// is used here. Elements that compare equal have the same mean; unprocessed centroids added by
    /// DuckDB all have weight 1, so equal-mean elements are indistinguishable and any sort gives
    /// the same result. The one exception is `-0.0` vs `0.0` (equal under `<`), whose relative
    /// order after `std::sort` is unspecified; this port keeps insertion order.
    ///
    /// On an empty digest C++ reads `unprocessed_[0]` out of bounds; here it is a no-op.
    fn process(&mut self) {
        if self.unprocessed.is_empty() && self.processed.is_empty() {
            return;
        }
        self.unprocessed.sort_by(cmp_mean);
        // std::inplace_merge(sorted unprocessed, processed): take from the processed run only when
        // it is strictly less.
        let mut merged = Vec::with_capacity(self.unprocessed.len() + self.processed.len());
        {
            let (a, b) = (&self.unprocessed, &self.processed);
            let (mut i, mut j) = (0usize, 0usize);
            while i < a.len() && j < b.len() {
                if b[j].mean < a[i].mean {
                    merged.push(b[j]);
                    j += 1;
                } else {
                    merged.push(a[i]);
                    i += 1;
                }
            }
            merged.extend_from_slice(&a[i..]);
            merged.extend_from_slice(&b[j..]);
        }

        self.processed_weight += self.unprocessed_weight;
        self.unprocessed_weight = 0.0;
        self.processed.clear();

        self.processed.push(merged[0]);
        let mut w_so_far = merged[0].weight;
        let mut w_limit = self.processed_weight * self.integrated_q(1.0);

        for centroid in &merged[1..] {
            let projected_w = w_so_far + centroid.weight;
            if projected_w <= w_limit {
                w_so_far = projected_w;
                self.processed.last_mut().expect("non-empty").add(centroid);
            } else {
                let k1 = self.integrated_location(w_so_far / self.processed_weight);
                w_limit = self.processed_weight * self.integrated_q(k1 + 1.0);
                w_so_far += centroid.weight;
                self.processed.push(*centroid);
            }
        }
        self.unprocessed.clear();
        self.min = std_min(self.min, self.processed[0].mean);
        self.max = std_max(self.max, self.processed[self.processed.len() - 1].mean);
        self.update_cumulative();
    }

    /// `TDigest::integratedLocation`: maps a quantile to the centroid scale k.
    #[inline]
    fn integrated_location(&self, q: f64) -> f64 {
        self.compression * ((2.0 * q - 1.0).asin() + PI / 2.0) / PI
    }

    /// `TDigest::integratedQ`: inverse of `integrated_location`, clamped at the compression.
    #[inline]
    fn integrated_q(&self, k: f64) -> f64 {
        ((std_min(k, self.compression) * PI / self.compression - PI / 2.0).sin() + 1.0) / 2.0
    }

    /// `TDigest::weightedAverage`.
    #[inline]
    fn weighted_average(x1: f64, w1: f64, x2: f64, w2: f64) -> f64 {
        if x1 <= x2 {
            Self::weighted_average_sorted(x1, w1, x2, w2)
        } else {
            Self::weighted_average_sorted(x2, w2, x1, w1)
        }
    }

    /// `TDigest::weightedAverageSorted`.
    #[inline]
    fn weighted_average_sorted(x1: f64, w1: f64, x2: f64, w2: f64) -> f64 {
        let x = (x1 * w1 + x2 * w2) / (w1 + w2);
        std_max(x1, std_min(x, x2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(td: &TDigest) -> Vec<(u64, u64)> {
        td.processed()
            .iter()
            .map(|c| (c.mean().to_bits(), c.weight().to_bits()))
            .collect()
    }

    #[test]
    fn quantiles_of_1_to_1000() {
        let mut td = TDigest::new(100.0);
        for i in 1..=1000 {
            td.add_value(i as f64);
        }
        let median = td.quantile(0.5);
        let p99 = td.quantile(0.99);
        assert!((median - 500.5).abs() < 2.0, "median {median}");
        assert!((p99 - 990.5).abs() < 2.0, "p99 {p99}");
        assert_eq!(td.min(), 1.0);
        assert_eq!(td.max(), 1000.0);
        assert_eq!(td.total_weight(), 1000.0);
        assert!(td.processed().len() <= td.max_processed());
        assert!(td.processed().windows(2).all(|w| w[0].mean() <= w[1].mean()));
    }

    #[test]
    fn single_value() {
        let mut td = TDigest::new(100.0);
        td.add_value(42.0);
        assert_eq!(td.quantile(0.0), 42.0);
        assert_eq!(td.quantile(0.5), 42.0);
        assert_eq!(td.quantile(1.0), 42.0);
        assert!(td.quantile(1.5).is_nan());
        assert_eq!(td.min(), 42.0);
        assert_eq!(td.max(), 42.0);
        assert_eq!(td.cdf(42.0), 0.5);
    }

    #[test]
    fn empty_and_nan() {
        let mut td = TDigest::new(100.0);
        assert!(!td.add(f64::NAN, 1.0));
        td.compress();
        assert!(td.quantile(0.5).is_nan());
        assert_eq!(td.max(), f64::MIN_POSITIVE);
    }

    #[test]
    fn merge_small_is_exact() {
        // Below the unprocessed capacity nothing is processed before the merge, so merging only
        // concatenates the buffers in the same order as adding everything to one digest.
        let values: Vec<f64> = (0..700).map(|i| ((i * 7919) % 1000) as f64 * 0.5).collect();
        let (mut a, mut b, mut all) = (TDigest::new(100.0), TDigest::new(100.0), TDigest::new(100.0));
        for (i, &v) in values.iter().enumerate() {
            if i < 300 {
                a.add_value(v);
            } else {
                b.add_value(v);
            }
            all.add_value(v);
        }
        a.merge(&b);
        a.compress();
        all.compress();
        assert_eq!(bits(&a), bits(&all));
        for q in [0.0, 0.1, 0.5, 0.9, 0.99, 1.0] {
            assert_eq!(a.quantile(q).to_bits(), all.quantile(q).to_bits());
        }
    }

    #[test]
    fn merge_large_is_close() {
        let (mut a, mut b, mut all) = (TDigest::new(100.0), TDigest::new(100.0), TDigest::new(100.0));
        for i in 1..=10000 {
            let v = i as f64;
            if i % 2 == 0 {
                a.add_value(v);
            } else {
                b.add_value(v);
            }
            all.add_value(v);
        }
        a.merge(&b);
        assert_eq!(a.total_weight(), 10000.0);
        for q in [0.01, 0.25, 0.5, 0.75, 0.99] {
            let (x, y) = (a.quantile(q), all.quantile(q));
            assert!((x - y).abs() < 20.0, "q={q}: {x} vs {y}");
        }
        assert_eq!(a.min(), 1.0);
        assert_eq!(a.max(), 10000.0);
    }

    #[test]
    fn export_import_roundtrip() {
        let mut td = TDigest::new(100.0);
        for i in 0..5000 {
            td.add_value(((i * 37) % 5000) as f64);
        }
        td.compress();
        let imported = TDigest::from_parts(100.0, td.min(), td.max(), td.centroids().to_vec());
        let mut imported2 = imported.clone();
        for q in [0.0, 0.3, 0.5, 0.97, 1.0] {
            assert_eq!(td.quantile(q).to_bits(), imported2.quantile(q).to_bits());
        }
        // Merging two imported states goes through the two-list priority-queue merge.
        let mut m = imported.clone();
        m.merge(&imported);
        assert_eq!(m.total_weight(), 10000.0);
        assert!((m.quantile(0.5) - td.quantile(0.5)).abs() < 1e-9 * 5000.0 + 1.0);
    }

    #[test]
    fn heap_matches_std_semantics() {
        // Distinct priorities must come out in order regardless of tie handling.
        let mut pq = StdPriorityQueue::new(|a: &i32, b: &i32| a > b);
        for v in [5, 3, 9, 1, 7, 2, 8, 6, 4, 0] {
            pq.push(v);
        }
        let mut out = Vec::new();
        while !pq.is_empty() {
            out.push(pq.top());
            pq.pop();
        }
        assert_eq!(out, (0..10).collect::<Vec<_>>());
    }
}
