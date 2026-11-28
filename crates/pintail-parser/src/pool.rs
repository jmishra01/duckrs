//! Parser threads: parsing recurses deeply, so it runs on threads with a large stack. Starting
//! one per call cost about as much as parsing a statement, so idle threads are kept for reuse;
//! concurrent callers each take their own.

use pintail_common::{Error, ErrorKind, Result};
use std::sync::Mutex;
use std::sync::mpsc::{Sender, channel};

/// The stack of a parser thread.
pub(crate) const PARSER_STACK_SIZE: usize = 512 * 1024 * 1024;

/// How many idle threads are kept; more concurrent callers start threads that exit afterwards.
const MAX_IDLE: usize = 8;

type Job = Box<dyn FnOnce() + Send>;

static IDLE: Mutex<Vec<Sender<Job>>> = Mutex::new(Vec::new());

fn start_worker() -> Result<Sender<Job>> {
    let (sender, receiver) = channel::<Job>();
    std::thread::Builder::new()
        .name("pintail-parser".into())
        .stack_size(PARSER_STACK_SIZE)
        .spawn(move || {
            // runs jobs until its sender is dropped
            for job in receiver {
                job();
            }
        })
        .map_err(|e| Error::new(ErrorKind::Internal, format!("failed to start the parser thread: {e}")))?;
    Ok(sender)
}

/// Runs `f` on a parser thread and returns its result.
pub(crate) fn run<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    let idle = IDLE.lock().unwrap_or_else(|e| e.into_inner()).pop();
    let worker = match idle {
        Some(worker) => worker,
        None => start_worker()?,
    };
    let (result_sender, result) = channel();
    worker
        .send(Box::new(move || {
            let _ = result_sender.send(f());
        }))
        .map_err(|_| Error::new(ErrorKind::Internal, "the parser thread stopped"))?;
    // a panicking job ends its thread, which closes the result channel
    let value = result
        .recv()
        .map_err(|_| Error::new(ErrorKind::Internal, "the parser thread panicked"))?;
    let mut idle = IDLE.lock().unwrap_or_else(|e| e.into_inner());
    if idle.len() < MAX_IDLE {
        idle.push(worker);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    #[test]
    fn reuses_threads_and_survives_panics() {
        // that the same thread runs twice in a row can't be asserted: the pool is global and
        // the tests running next to this one take threads out of it
        super::run(|| std::thread::current().id()).unwrap();
        assert!(super::run(|| panic!("boom")).is_err());
        assert_eq!(super::run(|| 42).unwrap(), 42);
        let handles: Vec<_> = (0..4)
            .map(|i| std::thread::spawn(move || super::run(move || i * 2).unwrap()))
            .collect();
        let results: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results, vec![0, 2, 4, 6]);
    }
}
