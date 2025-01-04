use ahash::AHashSet as HashSet;
use std::{
    cell::UnsafeCell,
    cmp::min,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    thread,
};

static BUCKET_SIZE: usize = 64;

pub struct LockFreeQueue<T> {
    capacity: usize,
    // read_for_write & data
    buffer: Vec<UnsafeCell<(AtomicBool, T)>>,
    head: AtomicUsize,
    tail: AtomicUsize,
}

unsafe impl<T> Sync for LockFreeQueue<T> {}

impl<T> LockFreeQueue<T>
where
    T: Default,
{
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.next_power_of_two();
        let buffer = (0..capacity)
            .map(|_| UnsafeCell::new((AtomicBool::new(true), Default::default())))
            .collect();
        Self { capacity, buffer, head: AtomicUsize::new(0), tail: AtomicUsize::new(0) }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.tail.load(Ordering::Acquire) - self.head.load(Ordering::Acquire)
    }

    pub fn push(&self, item: T) {
        loop {
            let head = self.head.load(Ordering::Acquire);
            let tail = self.tail.load(Ordering::Relaxed);
            if tail - head < self.capacity {
                let loc = unsafe { &mut (*self.buffer[tail % self.capacity].get()) };
                while !loc.0.load(Ordering::Relaxed) {
                    thread::yield_now();
                }
                loc.1 = item;
                loc.0.store(false, Ordering::Relaxed);
                self.tail.store(tail + 1, Ordering::Release);
                return;
            }
            thread::yield_now();
        }
    }

    pub fn multi_push(&self, item: T) {
        loop {
            let head = self.head.load(Ordering::Acquire);
            let tail = self.tail.load(Ordering::Relaxed);

            if tail - head < self.capacity &&
                self.tail
                    .compare_exchange(tail, tail + 1, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
            {
                let loc = unsafe { &mut (*self.buffer[tail % self.capacity].get()) };
                while !loc.0.load(Ordering::Relaxed) {
                    thread::yield_now();
                }
                loc.1 = item;
                loc.0.store(false, Ordering::Relaxed);
                return;
            }
            thread::yield_now();
        }
    }

    pub fn pop(&self) -> Option<T> {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);

        if head == tail {
            None
        } else {
            let loc = unsafe { &mut (*self.buffer[head % self.capacity].get()) };
            while loc.0.load(Ordering::Relaxed) {
                thread::yield_now();
            }
            let item = std::mem::take(&mut loc.1);
            loc.0.store(true, Ordering::Relaxed);
            self.head.store(head + 1, Ordering::Release);
            Some(item)
        }
    }

    pub fn multi_pop(&self) -> Option<T> {
        loop {
            let head = self.head.load(Ordering::Relaxed);
            let tail = self.tail.load(Ordering::Acquire);

            if head == tail {
                return None;
            }

            if self
                .head
                .compare_exchange(head, head + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                let loc = unsafe { &mut (*self.buffer[head % self.capacity].get()) };
                while loc.0.load(Ordering::Relaxed) {
                    thread::yield_now();
                }
                let item = std::mem::take(&mut loc.1);
                loc.0.store(true, Ordering::Relaxed);
                return Some(item);
            }
            thread::yield_now();
        }
    }
}

pub struct OrderedSet {
    capacity: usize,
    len: usize,
    min_index: usize,
    item_onboard: Vec<bool>,
    bucket_onboard: Vec<u8>,
}

impl OrderedSet {
    pub fn new(capacity: usize, all_onboard: bool) -> Self {
        let remainder = capacity % BUCKET_SIZE;
        let bucket_num =
            if remainder == 0 { capacity / BUCKET_SIZE } else { capacity / BUCKET_SIZE + 1 };
        let mut bucket_onboard = vec![if all_onboard { BUCKET_SIZE as u8 } else { 0 }; bucket_num];
        if all_onboard && remainder != 0 {
            bucket_onboard[bucket_num - 1] = remainder as u8;
        }
        Self {
            capacity,
            len: if all_onboard { capacity } else { 0 },
            min_index: 0,
            item_onboard: vec![all_onboard; capacity],
            bucket_onboard,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn to_set(&self) -> HashSet<usize> {
        let mut set = HashSet::new();
        for index in 0..self.capacity {
            if self.item_onboard[index] {
                set.insert(index);
            }
        }
        set
    }

    pub fn contains(&self, index: usize) -> bool {
        self.item_onboard[index]
    }

    pub fn first(&self) -> Option<usize> {
        if self.len > 0 {
            Some(self.min_index)
        } else {
            None
        }
    }

    pub fn insert(&mut self, index: usize) -> bool {
        if index < self.capacity {
            if !self.item_onboard[index] {
                self.item_onboard[index] = true;
                self.bucket_onboard[index / BUCKET_SIZE] += 1;
                if self.len == 0 {
                    self.min_index = index;
                } else if index < self.min_index {
                    self.min_index = index;
                }
                self.len += 1;
            }
            true
        } else {
            false
        }
    }

    pub fn remove(&mut self, index: usize) -> bool {
        if index < self.capacity {
            if self.item_onboard[index] {
                self.item_onboard[index] = false;
                self.bucket_onboard[index / BUCKET_SIZE] -= 1;
                self.len -= 1;
                if index == self.min_index {
                    if self.len == 0 {
                        self.min_index = 0
                    } else {
                        let mut find_min = false;
                        let mut next_min = index + 1;
                        let mut min_bucket = next_min / BUCKET_SIZE;
                        while min_bucket < self.bucket_onboard.len() {
                            if self.bucket_onboard[min_bucket] != 0 {
                                break;
                            }
                            min_bucket += 1;
                            next_min = min_bucket * BUCKET_SIZE;
                        }
                        let current_bucket_end = min((min_bucket + 1) * BUCKET_SIZE, self.capacity);
                        while next_min < current_bucket_end {
                            if self.item_onboard[next_min] {
                                self.min_index = next_min;
                                find_min = true;
                                break;
                            }
                            next_min += 1;
                        }
                        assert!(find_min)
                    }
                }
            }
            true
        } else {
            false
        }
    }

    pub fn pop_first(&mut self) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        assert!(self.item_onboard[self.min_index]);
        let first = self.min_index;
        self.remove(first);
        Some(first)
    }
}
