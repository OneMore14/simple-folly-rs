use std::cell::{Cell, UnsafeCell};
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize};
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};

use crossbeam_utils::CachePadded;

#[repr(transparent)]
struct Slot<T> {
    val: UnsafeCell<MaybeUninit<T>>
}

impl<T> Slot<T> {

    fn uninit() -> Self {
        Slot {
            val: UnsafeCell::new(MaybeUninit::uninit())
        }
    }
}

struct RingBuffer<T> {
    size: usize,
    read_idx: CachePadded<AtomicUsize>,
    write_idx: CachePadded<AtomicUsize>,
    buffer: Box<[Slot<T>]>,
}

impl<T> RingBuffer<T> {

    fn new(cap: usize) -> Self {
        assert!(cap >= 1, "cap can't be zero");
        let cap = cap + 1;
        let buffer: Box<[Slot<T>]> = (0..cap)
            .map(|_| {
                Slot::uninit()
            })
            .collect();
        RingBuffer {
            size: cap,
            read_idx: CachePadded::new(AtomicUsize::new(0)),
            write_idx: CachePadded::new(AtomicUsize::new(0)),
            buffer,
        }
    }

    fn recv(&self) -> Result<T, ()> {
        let current_read = self.read_idx.load(Relaxed);
        if current_read == self.write_idx.load(Acquire) {
            return Err(());
        }
        let mut next_record = current_read + 1;
        if next_record == self.size {
            next_record = 0;
        }
        let slot = unsafe { self.buffer.get_unchecked(current_read) };
        let val = unsafe { slot.val.get().read().assume_init() };
        self.read_idx.store(next_record,Release);
        Ok(val)
    }

    fn send(&self, val: T) -> Result<(), T> {
        let current_write = self.write_idx.load(Relaxed);
        let mut next_record = current_write + 1;
        if next_record == self.size {
            next_record = 0;
        }
        if next_record != self.read_idx.load(Acquire) {
            let slot = unsafe { self.buffer.get_unchecked(current_write) };
            unsafe {slot.val.get().write(MaybeUninit::new(val));}
            self.write_idx.store(next_record, Release);
            return Ok(());
        }
        Err(val)
    }

    fn is_empty(&self) -> bool {
        self.read_idx.load(Acquire) == self.write_idx.load(Acquire)
    }

    fn is_full(&self) -> bool {
        let mut next_record = self.write_idx.load(Acquire) + 1;
        if next_record == self.size {
            next_record = 0;
        }
        if next_record != self.read_idx.load(Acquire) {
            return false;
        }
        true
    }

    fn capacity(&self) -> usize {
        self.size - 1
    }
}

impl<T> Drop for RingBuffer<T> {
    fn drop(&mut self) {
        while !self.is_empty() {
            self.recv().unwrap();
        }
    }
}

pub struct Sender<T> {
    buffer: Arc<RingBuffer<T>>,

    // accurate
    cached_head: Cell<usize>,

    // inaccurate
    cached_tail: Cell<usize>,
}

impl<T> Sender<T> {
    pub fn send(&self, val: T) -> Result<(), T> {
        self.buffer.send(val)
    }

    fn write_pos(&self) -> Option<usize> {
        let mut tail = self.cached_tail.get();
        let head = self.cached_head.get() + 1;
        if tail == self.buffer.size {
            tail = 0;
        }
        // may be full
        if tail == head {
            tail = self.buffer.write_idx.load(Acquire);
            if tail == self.buffer.size {
                tail = 0;
            }
            if tail == head {
                return None;
            }
        }
        Some(tail)
    }
}

unsafe impl<T: Send> Send for Sender<T> {}
impl<T> !Sync for Sender<T> {}

pub struct Receiver<T> {
    buffer: Arc<RingBuffer<T>>,

    // inaccurate
    cached_head: Cell<usize>,

    // accurate
    cached_tail: Cell<usize>,
}

impl<T> Receiver<T> {
    pub fn recv(&self) -> Result<T, ()> {
        self.buffer.recv()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

unsafe impl<T: Send> Send for Receiver<T> {}
impl<T> !Sync for Receiver<T> {}

pub fn new<T>(cap: usize) -> (Sender<T>, Receiver<T>) {

    let buffer = Arc::new(RingBuffer::new(cap));
    let sender = Sender {
        buffer: buffer.clone(),
        cached_head: Cell::new(0),
        cached_tail: Cell::new(0),
    };
    let receiver = Receiver {
        buffer: buffer.clone(),
        cached_head: Cell::new(0),
        cached_tail: Cell::new(0),
    };
    (sender, receiver)
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn basic() {
        let (sender, receiver) = new::<i32>(10);
        let t1 = std::thread::spawn(move || {
            for i in 1..=5 {
                sender.send(i).unwrap();
            }
        });
        let t2 = std::thread::spawn(move || {
            let mut list = vec![];
            while list.len() < 5 {
                if let Ok(val) = receiver.recv() {
                    list.push(val);
                }
            }
            assert_eq!(list, vec![1, 2, 3, 4, 5]);
        });
        t1.join().unwrap();
        t2.join().unwrap();
    }

    #[test]
    fn test_memory_leak() {
        let (sender, receiver) = new::<String>(10);
        let t1 = std::thread::spawn(move || {
            for _ in 1..=5 {
                sender.send("hello".into()).unwrap();
            }
        });
        let t2 = std::thread::spawn(move || {

        });
        t1.join().unwrap();
        t2.join().unwrap();
    }
}