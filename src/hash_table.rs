pub mod cell;
pub mod cell_array;

use cell::{EntryIterator, Locker, Reader};
use cell_array::CellArray;

use crate::ebr::{Arc, AtomicArc, Barrier, Tag};

use std::borrow::Borrow;
use std::convert::TryInto;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};

/// `HashTable` define common functions for `HashIndex` and `HashMap`.
pub(super) trait HashTable<K, V, H, const CELL_SIZE: usize, const LOCK_FREE: bool>
where
    K: 'static + Eq + Hash + Sync,
    V: 'static + Sync,
    H: BuildHasher,
{
    /// Returns the hash value of the given key.
    #[inline]
    fn hash<Q>(&self, key: &Q) -> (u64, u8)
    where
        K: Borrow<Q>,
        Q: Hash + ?Sized,
    {
        // Generates a hash value.
        let mut h = self.hasher().build_hasher();
        key.hash(&mut h);
        let hash = h.finish();

        /*
            // Bitmix: https://mostlymangling.blogspot.com/2019/01/better-stronger-mixer-and-test-procedure.html
            hash = hash ^ (hash.rotate_right(25) ^ hash.rotate_right(50));
            hash = hash.overflowing_mul(0xA24BAED4963EE407u64).0;
            hash = hash ^ (hash.rotate_right(24) ^ hash.rotate_right(49));
            hash = hash.overflowing_mul(0x9FB21C651E98DF25u64).0;
            hash = hash ^ (hash >> 28);
        */

        (hash, (hash & ((1 << 8) - 1)).try_into().unwrap())
    }

    /// Returns a reference to its build hasher.
    fn hasher(&self) -> &H;

    /// Copying function.
    fn copier(key: &K, val: &V) -> Option<(K, V)>;

    /// Returns a reference to the `CellArray` pointer.
    fn cell_array(&self) -> &AtomicArc<CellArray<K, V, CELL_SIZE, LOCK_FREE>>;

    /// Returns the minimum allowed capacity.
    fn minimum_capacity(&self) -> usize;

    /// Returns a reference to the resizing mutex.
    fn resize_mutex_ref(&self) -> &AtomicU8;

    /// Returns the number of entries.
    fn num_entries(&self, barrier: &Barrier) -> usize {
        let current_array_ptr = self.cell_array().load(Acquire, barrier);
        let current_array_ref = current_array_ptr.as_ref().unwrap();
        let mut num_entries = 0;
        for i in 0..current_array_ref.num_cells() {
            num_entries += current_array_ref.cell(i).num_entries();
        }
        let old_array_ptr = current_array_ref.old_array(barrier);
        if let Some(old_array_ref) = old_array_ptr.as_ref() {
            for i in 0..old_array_ref.num_cells() {
                num_entries += old_array_ref.cell(i).num_entries();
            }
        }
        num_entries
    }

    /// Returns the number of slots.
    fn num_slots(&self, barrier: &Barrier) -> usize {
        let current_array_ptr = self.cell_array().load(Acquire, barrier);
        let current_array_ref = current_array_ptr.as_ref().unwrap();
        current_array_ref.num_entries()
    }

    /// Estimates the number of entries using the given number of cells.
    fn estimate(
        array_ref: &CellArray<K, V, CELL_SIZE, LOCK_FREE>,
        num_cells_to_sample: usize,
    ) -> usize {
        let mut num_entries = 0;
        for i in 0..num_cells_to_sample {
            num_entries += array_ref.cell(i).num_entries();
        }
        num_entries * (array_ref.num_cells() / num_cells_to_sample)
    }

    /// Inserts an entry into the [`HashTable`].
    #[inline]
    fn insert_entry(&self, key: K, val: V) -> Result<(), (K, V)> {
        let (hash, partial_hash) = self.hash(&key);
        let barrier = Barrier::new();
        let (_, locker, iterator) = self.acquire(&key, hash, partial_hash, &barrier);
        if iterator.is_some() {
            return Err((key, val));
        }
        locker.insert(key, val, partial_hash, &barrier);
        Ok(())
    }

    /// Reads an entry from the [`HashTable`].
    #[inline]
    fn read_entry<'b, Q, R, F: FnOnce(&'b K, &'b V) -> R>(
        &self,
        key_ref: &Q,
        reader: F,
        barrier: &'b Barrier,
    ) -> Option<R>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let (hash, partial_hash) = self.hash(key_ref);

        // An acquire fence is required to correctly load the contents of the array.
        let mut current_array_ptr = self.cell_array().load(Acquire, barrier);
        while let Some(current_array_ref) = current_array_ptr.as_ref() {
            if let Some(old_array_ref) = current_array_ref.old_array(barrier).as_ref() {
                if current_array_ref.partial_rehash(|key| self.hash(key), &Self::copier, barrier) {
                    current_array_ptr = self.cell_array().load(Acquire, barrier);
                    continue;
                }
                let cell_index = old_array_ref.calculate_cell_index(hash);
                if LOCK_FREE {
                    let cell_ref = old_array_ref.cell(cell_index);
                    if let Some(entry) = cell_ref.search(key_ref, partial_hash, barrier) {
                        return Some(reader(&entry.0, &entry.1));
                    }
                } else if let Some(locker) = Reader::lock(old_array_ref.cell(cell_index), barrier) {
                    if let Some((key, value)) =
                        locker.cell_ref().search(key_ref, partial_hash, barrier)
                    {
                        return Some(reader(key, value));
                    }
                }
            }
            let cell_index = current_array_ref.calculate_cell_index(hash);
            if LOCK_FREE {
                let cell_ref = current_array_ref.cell(cell_index);
                if let Some(entry) = cell_ref.search(key_ref, partial_hash, barrier) {
                    return Some(reader(&entry.0, &entry.1));
                }
            } else if let Some(locker) = Reader::lock(current_array_ref.cell(cell_index), barrier) {
                if let Some((key, value)) = locker.cell_ref().search(key_ref, partial_hash, barrier)
                {
                    return Some(reader(key, value));
                }
            }
            let new_current_array_ptr = self.cell_array().load(Acquire, barrier);
            if new_current_array_ptr == current_array_ptr {
                break;
            }
            // The pointer value has changed.
            current_array_ptr = new_current_array_ptr;
        }
        None
    }

    /// Acquires a [`Locker`] and [`EntryIterator`].
    ///
    /// In case it successfully found the key, it returns a [`EntryIterator`]. Not returning a
    /// [`EntryIterator`] means that the key does not exist.
    #[inline]
    fn acquire<'h, 'b, Q>(
        &'h self,
        key_ref: &Q,
        hash: u64,
        partial_hash: u8,
        barrier: &'b Barrier,
    ) -> (
        usize,
        Locker<'b, K, V, CELL_SIZE, LOCK_FREE>,
        Option<EntryIterator<'b, K, V, CELL_SIZE, LOCK_FREE>>,
    )
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let mut check_resize = true;

        // It is guaranteed that the thread reads a consistent snapshot of the current and old
        // array pair by a release memory barrier in the resize function, hence the following
        // procedure is correct.
        //  - The thread reads `self.array`, and it kills the target cell in the old array if
        //    there is one attached to it, and inserts the key into array.
        // There are two cases.
        //  1. The thread reads an old version of `self.array`.
        //    If there is another thread having read the latest version of `self.array`,
        //    trying to insert the same key, it will try to kill the Cell in the old version
        //    of `self.array`, thus competing with each other.
        //  2. The thread reads the latest version of `self.array`.
        //    If the array is deprecated while inserting the key, it falls into case 1.
        loop {
            // An acquire fence is required to correctly load the contents of the array.
            let current_array_ptr = self.cell_array().load(Acquire, barrier);
            let current_array_ref = current_array_ptr.as_ref().unwrap();
            if let Some(old_array_ref) = current_array_ref.old_array(barrier).as_ref() {
                if current_array_ref.partial_rehash(|key| self.hash(key), &Self::copier, barrier) {
                    continue;
                }
                check_resize = false;
                let cell_index = old_array_ref.calculate_cell_index(hash);
                if let Some(mut locker) = Locker::lock(old_array_ref.cell(cell_index), barrier) {
                    if let Some(iterator) = locker.cell_ref().get(key_ref, partial_hash, barrier) {
                        return (cell_index, locker, Some(iterator));
                    }
                    // Kills the Cell.
                    current_array_ref.kill_cell(
                        &mut locker,
                        old_array_ref,
                        cell_index,
                        &|key| self.hash(key),
                        &Self::copier,
                        barrier,
                    );
                }
            }
            let cell_index = current_array_ref.calculate_cell_index(hash);

            // Tries to resize the array.
            let num_entries = current_array_ref.cell(cell_index).num_entries();
            if check_resize && num_entries >= CELL_SIZE {
                // Triggers resize if the estimated load factor is greater than 7/8.
                check_resize = false;
                self.try_enlarge(current_array_ref, cell_index, num_entries, barrier);
                continue;
            }

            if let Some(locker) = Locker::lock(current_array_ref.cell(cell_index), barrier) {
                if let Some(iterator) = locker.cell_ref().get(key_ref, partial_hash, barrier) {
                    return (cell_index, locker, Some(iterator));
                }
                return (cell_index, locker, None);
            }

            // Reaching here means that `self.array` is updated.
        }
    }

    /// Tries to enlarge the array.
    fn try_enlarge(
        &self,
        array_ref: &CellArray<K, V, CELL_SIZE, LOCK_FREE>,
        cell_index: usize,
        mut num_entries: usize,
        barrier: &Barrier,
    ) {
        let sample_size = array_ref.sample_size();
        let array_size = array_ref.num_cells();
        let threshold = sample_size * (CELL_SIZE / 8) * 7;
        if num_entries > threshold
            || (1..sample_size).any(|i| {
                num_entries += array_ref.cell((cell_index + i) % array_size).num_entries();
                num_entries > threshold
            })
        {
            self.resize(barrier);
        }
    }

    /// Tries to shrink the array.
    fn try_shrink(
        &self,
        array_ref: &CellArray<K, V, CELL_SIZE, LOCK_FREE>,
        cell_index: usize,
        barrier: &Barrier,
    ) {
        if array_ref.num_entries() > self.minimum_capacity() {
            let sample_size = array_ref.sample_size();
            let array_size = array_ref.num_cells();
            let threshold = sample_size * CELL_SIZE / 16;
            let mut num_entries = 0;
            if !(1..sample_size).any(|i| {
                num_entries += array_ref.cell((cell_index + i) % array_size).num_entries();
                num_entries >= threshold
            }) {
                self.resize(barrier);
            }
        }
    }

    /// Resizes the array.
    fn resize(&self, barrier: &Barrier) {
        let mut mutex_state = self.resize_mutex_ref().load(Acquire);
        loop {
            if mutex_state == 2_u8 {
                // Another thread is resizing the table, and will retry.
                return;
            }
            let new_state = if mutex_state == 1_u8 {
                // Another thread is resizing the table, and needs to retry.
                2_u8
            } else {
                // This thread will acquire the mutex.
                1_u8
            };
            match self
                .resize_mutex_ref()
                .compare_exchange(mutex_state, new_state, Acquire, Acquire)
            {
                Ok(_) => {
                    if new_state == 2_u8 {
                        // Retry requested.
                        return;
                    }
                    // Lock acquired.
                    break;
                }
                Err(actual) => mutex_state = actual,
            }
        }

        let mut resize = true;
        while resize {
            let _mutex_guard = scopeguard::guard(&mut resize, |resize| {
                *resize = self.resize_mutex_ref().fetch_sub(1, Release) == 2_u8;
            });

            let current_array_ptr = self.cell_array().load(Acquire, barrier);
            let current_array_ref = current_array_ptr.as_ref().unwrap();
            let old_array = current_array_ref.old_array(barrier);
            if !old_array.is_null() {
                // With a deprecated array present, it cannot be resized.
                continue;
            }

            // The resizing policies are as follows.
            //  - The load factor reaches 7/8, then the array grows up to 64x.
            //  - The load factor reaches 1/16, then the array shrinks to fit.
            let capacity = current_array_ref.num_entries();
            let num_cells = current_array_ref.num_cells();
            let num_cells_to_sample = (num_cells / 8).max(2).min(4096);
            let estimated_num_entries = Self::estimate(current_array_ref, num_cells_to_sample);
            let new_capacity = if estimated_num_entries >= (capacity / 8) * 7 {
                let max_capacity = 1_usize << (std::mem::size_of::<usize>() * 8 - 1);
                if capacity == max_capacity {
                    // Do not resize if the capacity cannot be increased.
                    capacity
                } else {
                    let mut new_capacity = capacity;
                    while new_capacity < (estimated_num_entries / 8) * 15 {
                        // Doubles the new capacity until it can accommodate the estimated number of entries * 15/8.
                        if new_capacity == max_capacity {
                            break;
                        }
                        if new_capacity / capacity >= 32 {
                            break;
                        }
                        new_capacity *= 2;
                    }
                    new_capacity
                }
            } else if estimated_num_entries <= capacity / 16 {
                // Shrinks to fit.
                estimated_num_entries
                    .next_power_of_two()
                    .max(self.minimum_capacity())
            } else {
                capacity
            };

            // Array::new may not be able to allocate the requested number of cells.
            if new_capacity != capacity {
                self.cell_array().swap(
                    (
                        Some(Arc::new(CellArray::<K, V, CELL_SIZE, LOCK_FREE>::new(
                            new_capacity,
                            self.cell_array().clone(Relaxed, barrier),
                        ))),
                        Tag::None,
                    ),
                    Release,
                );
            }
        }
    }
}