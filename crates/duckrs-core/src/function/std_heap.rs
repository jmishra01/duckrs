//! `std::push_heap`, `std::pop_heap` and `std::sort_heap` as libc++ (LLVM >= 15, the Apple
//! toolchain) implements them. DuckDB's top-N aggregates keep their rows in such a heap, so the
//! order in which equal keys come out of `min(x, n)` or `arg_max(x, by, n)` is the heap's.
//! `less(a, b)` is the heap's `Compare`: the element for which no other is `less` sits at the top.

/// libc++ `__sift_up` for the element at `heap.len() - 1`.
fn sift_up<T: Copy>(heap: &mut [T], less: &impl Fn(&T, &T) -> bool) {
    let len = heap.len();
    if len > 1 {
        let mut last = len - 1;
        let mut parent = (len - 2) / 2;
        if less(&heap[parent], &heap[last]) {
            let t = heap[last];
            loop {
                heap[last] = heap[parent];
                last = parent;
                if parent == 0 {
                    break;
                }
                parent = (parent - 1) / 2;
                if !less(&heap[parent], &t) {
                    break;
                }
            }
            heap[last] = t;
        }
    }
}

/// `std::push_heap(heap.begin(), heap.end())`: the last element joins the heap before it.
pub(crate) fn push_heap<T: Copy>(heap: &mut [T], less: &impl Fn(&T, &T) -> bool) {
    sift_up(heap, less);
}

/// `std::pop_heap(heap.begin(), heap.end())`: the top moves to the end (Floyd's sift-down,
/// which prefers the left child on ties).
pub(crate) fn pop_heap<T: Copy>(heap: &mut [T], less: &impl Fn(&T, &T) -> bool) {
    let len = heap.len();
    if len > 1 {
        let top = heap[0];
        let mut hole = 0usize;
        loop {
            let mut child = 2 * hole + 1;
            if child + 1 < len && less(&heap[child], &heap[child + 1]) {
                child += 1;
            }
            heap[hole] = heap[child];
            hole = child;
            if child > (len - 2) / 2 {
                break;
            }
        }
        let last = len - 1;
        if hole == last {
            heap[hole] = top;
        } else {
            heap[hole] = heap[last];
            heap[last] = top;
            sift_up(&mut heap[..=hole], less);
        }
    }
}

/// `std::sort_heap(heap.begin(), heap.end())`: ascending by `less`.
pub(crate) fn sort_heap<T: Copy>(heap: &mut [T], less: &impl Fn(&T, &T) -> bool) {
    let mut len = heap.len();
    while len > 1 {
        pop_heap(&mut heap[..len], less);
        len -= 1;
    }
}

/// DuckDB's `UnaryAggregateHeap` / `BinaryAggregateHeap` over the indexes `0..count`: the
/// indexes of the `capacity` greatest by `less` (each new one replaces the top only when
/// `less(new, top)`), in the order `SortedSlots` reads them.
pub(crate) fn top_n(count: usize, capacity: usize, less: impl Fn(&usize, &usize) -> bool) -> Vec<usize> {
    let mut heap: Vec<usize> = Vec::with_capacity(capacity.min(count));
    for at in 0..count {
        if heap.len() < capacity {
            heap.push(at);
            push_heap(&mut heap, &less);
        } else if less(&at, &heap[0]) {
            pop_heap(&mut heap, &less);
            let last = heap.len() - 1;
            heap[last] = at;
            push_heap(&mut heap, &less);
        }
    }
    sort_heap(&mut heap, &less);
    heap
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ties_follow_the_heap() {
        // arg_max(arg, val, 2) over three equal keys keeps the first two, the second first.
        assert_eq!(top_n(3, 2, |_, _| false), vec![1, 0]);
        let keys = [1, 4, 1, 3];
        // max(v, 2): the comparator is "greater than", the result descends.
        assert_eq!(top_n(4, 2, |a, b| keys[*a] > keys[*b]), vec![1, 3]);
    }
}
