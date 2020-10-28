use std::ptr;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64};
use std::sync::{Condvar, Mutex};

#[derive(Default)]
pub struct Cell {
    metadata: AtomicU64,
    wait_queue: AtomicPtr<WaitQueueEntry>,
    link: Option<u32>,
    partial_hash_array: [u32; 10],
}

/// ExclusiveLocker
pub struct ExclusiveLocker<'a> {
    cell: &'a Cell,
    metadata: u64,
}

struct WaitQueueEntry {
    mutex: Mutex<bool>,
    condvar: Condvar,
    completed: AtomicBool,
    next: *mut WaitQueueEntry,
}

impl Cell {
    const XLOCK: u64 = 1 << 32;
    fn new() -> Cell {
        Cell {
            metadata: AtomicU64::new(0),
            wait_queue: AtomicPtr::new(ptr::null_mut()),
            link: None,
            partial_hash_array: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        }
    }
}

impl<'a> ExclusiveLocker<'a> {
    /// Creates a new ExclusiveLocker instance.
    fn new(cell: &'a Cell) -> ExclusiveLocker<'a> {
        let mut current = cell.metadata.load(Relaxed);
        loop {
            match cell.metadata.compare_exchange(
                current & (!Cell::XLOCK),
                current | Cell::XLOCK,
                Acquire,
                Relaxed,
            ) {
                Ok(result) => {
                    current = result | Cell::XLOCK;
                    break;
                }
                Err(result) => current = result,
            }

            // locked: wait for a thread to release the lock
            if current & Cell::XLOCK == Cell::XLOCK {
                if Self::wait(&cell) {
                    current = cell.metadata.load(Relaxed);
                    break;
                }
                current = cell.metadata.load(Relaxed);
            }
        }
        assert!(current & Cell::XLOCK == Cell::XLOCK);
        ExclusiveLocker {
            cell: cell,
            metadata: current,
        }
    }

    fn wait(cell: &'a Cell) -> bool {
        let mut barrier = WaitQueueEntry::new(cell.wait_queue.load(Relaxed));
        let barrier_ptr: *mut WaitQueueEntry = &mut barrier;
        loop {
            if let Err(result) =
                cell.wait_queue
                    .compare_exchange(barrier.next, barrier_ptr, Release, Relaxed)
            {
                barrier.next = result;
                continue;
            }
            break;
        }

        // try-lock again once the barrier is inserted into the wait queue
        let mut current = cell.metadata.load(Relaxed);
        let mut locked = false;
        loop {
            match cell.metadata.compare_exchange(
                current & (!Cell::XLOCK),
                current | Cell::XLOCK,
                Acquire,
                Relaxed,
            ) {
                Ok(_) => {
                    locked = true;
                    break;
                }
                Err(result) => {
                    if result & Cell::XLOCK == 0 {
                        current = result;
                        continue;
                    }
                    break;
                }
            }
        }

        if locked {
            Self::wakeup(cell);
        }
        barrier.wait();
        locked
    }

    fn wakeup(cell: &'a Cell) {
        let mut barrier_ptr: *mut WaitQueueEntry = cell.wait_queue.load(Acquire);
        loop {
            if let Err(result) =
                cell.wait_queue
                    .compare_exchange(barrier_ptr, ptr::null_mut(), Acquire, Relaxed)
            {
                barrier_ptr = result;
                if barrier_ptr == ptr::null_mut() {
                    return;
                }
                continue;
            }
            break;
        }

        while barrier_ptr != ptr::null_mut() {
            let next_ptr = unsafe { (*barrier_ptr).next };
            unsafe {
                (*barrier_ptr).signal();
            };
            barrier_ptr = next_ptr;
        }
    }
}

impl WaitQueueEntry {
    fn new(wait_queue: *mut WaitQueueEntry) -> WaitQueueEntry {
        WaitQueueEntry {
            mutex: Mutex::new(false),
            condvar: Condvar::new(),
            completed: AtomicBool::new(false),
            next: wait_queue,
        }
    }

    fn wait(&self) {
        let mut completed = self.mutex.lock().unwrap();
        while !*completed {
            completed = self.condvar.wait(completed).unwrap();
        }
        while !self.completed.load(Relaxed) {}
    }

    fn signal(&self) {
        let mut completed = self.mutex.lock().unwrap();
        *completed = true;
        self.condvar.notify_one();
        drop(completed);
        self.completed.store(true, Relaxed);
    }
}

impl<'a> Drop for ExclusiveLocker<'a> {
    fn drop(&mut self) {
        if self.metadata & Cell::XLOCK == Cell::XLOCK {
            let mut current = self.metadata;
            loop {
                assert!(current & Cell::XLOCK == Cell::XLOCK);
                match self.cell.metadata.compare_exchange(
                    current,
                    current & (!Cell::XLOCK),
                    Release,
                    Relaxed,
                ) {
                    Err(result) => current = result,
                    Ok(_) => break,
                }
            }
            Self::wakeup(self.cell);
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn basic_assumptions() {
        assert_eq!(std::mem::size_of::<Cell>(), 64)
    }

    #[test]
    fn basic_exclusive_locker() {
        let threads = 12;
        let barrier = Arc::new(Barrier::new(threads));
        let cell: Arc<Cell> = Arc::new(Cell::new());
        let mut thread_handles = Vec::with_capacity(threads);
        for tid in 0..threads {
            let barrier_copied = barrier.clone();
            let cell_copied = cell.clone();
            let thread_id = tid;
            thread_handles.push(thread::spawn(move || {
                barrier_copied.wait();
                for i in 0..4096 {
                    let locker = ExclusiveLocker::new(&*cell_copied);
                    if i % 256 == 255 {
                        println!("locked {}:{}", thread_id, i);
                    }
                    drop(locker);
                }
            }));
        }
        for handle in thread_handles {
            handle.join().unwrap();
        }
    }
}