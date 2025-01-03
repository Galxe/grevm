use std::{
    cell::UnsafeCell,
    sync::atomic::{AtomicUsize, Ordering},
    thread,
};

pub struct LockFreeQueue<T> {
    capacity: usize,
    buffer: Vec<UnsafeCell<T>>,
    head: AtomicUsize,
    tail: AtomicUsize,
}

unsafe impl<T> Sync for LockFreeQueue<T> {}

impl<T> LockFreeQueue<T>
where
    T: Default + Clone,
{
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.next_power_of_two();
        let buffer = (0..capacity).map(|_| UnsafeCell::default()).collect();
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
                unsafe {
                    *self.buffer[tail % self.capacity].get() = item;
                }
                self.tail.store(tail + 1, Ordering::Release);
                return;
            }
            thread::yield_now();
        }
    }

    pub fn pop(&self) -> Option<T> {
        loop {
            let head = self.head.load(Ordering::Relaxed);
            let tail = self.tail.load(Ordering::Acquire);

            if head == tail {
                return None;
            }
            let item = unsafe { (*self.buffer[head % self.capacity].get()).clone() };

            if self
                .head
                .compare_exchange(head, head + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(item);
            }
            thread::yield_now();
        }
    }
}
