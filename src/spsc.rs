use std::cell::{Cell, UnsafeCell};
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize};
use std::sync::atomic::Ordering::{Acquire, Release};

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

        let buffer: Box<[Slot<T>]> = (0..=cap)
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

    fn get_slot_ptr(&self, pos: usize) -> *mut MaybeUninit<T> {
        unsafe { self.buffer.get_unchecked(pos).val.get() }
    }

    unsafe fn read_slot(&self, pos: usize) -> T {
        self.get_slot_ptr(pos).read().assume_init()
    }

    #[allow(dead_code)]
    fn is_empty(&self) -> bool {
        self.read_idx.load(Acquire) == self.write_idx.load(Acquire)
    }

    #[allow(dead_code)]
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

    #[allow(dead_code)]
    fn capacity(&self) -> usize {
        self.size - 1
    }
}

impl<T> Drop for RingBuffer<T> {
    fn drop(&mut self) {
        let mut head = self.read_idx.load(Acquire);
        let tail = self.write_idx.load(Acquire);
        while head != tail {
            unsafe { let _ = self.read_slot(head);}
            head = get_next_pos(head, self.size);
        }
    }
}

#[inline]
fn get_next_pos(pos: usize, size: usize) -> usize {
    if pos == size {
        0
    } else {
        pos + 1
    }
}

pub struct Sender<T> {
    buffer: Arc<RingBuffer<T>>,

    // accurate
    cached_head: Cell<usize>,

    // inaccurate
    cached_tail: Cell<usize>,

    size: usize,
}

impl<T> Sender<T> {
    pub fn send(&self, val: T) -> Result<(), T> {
        if let Some(pos) = self.write_pos() {
            unsafe { self.buffer.get_slot_ptr(pos).write(MaybeUninit::new(val)); }
            let next_pos = get_next_pos(pos, self.size);
            self.buffer.write_idx.store(next_pos, Release);
            self.cached_tail.set(next_pos);
            Ok(())
        } else {
            Err(val)
        }
    }

    fn write_pos(&self) -> Option<usize> {
        let tail = self.cached_tail.get();
        let next_tail = get_next_pos(tail, self.size);
        let mut head = self.cached_head.get();

        // may be full
        if next_tail == head {
            head = self.buffer.read_idx.load(Acquire);
            self.cached_head.set(head);
            if next_tail == head {
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

    size: usize,
}

impl<T> Receiver<T> {
    pub fn recv(&self) -> Result<T, SPSCError> {
        if let Some(pos) = self.read_pos() {
            let val = unsafe { self.buffer.read_slot(pos) };
            let next_read = get_next_pos(pos, self.size);
            self.buffer.read_idx.store(next_read, Release);
            self.cached_head.set(next_read);
            Ok(val)
        } else {
            Err(SPSCError::Full)
        }
    }

    pub fn read_pos(&self) -> Option<usize> {
        let tail = self.cached_tail.get();
        let head = self.cached_head.get();

        // may be empty
        if head == tail {
            let tail = self.buffer.write_idx.load(Acquire);
            self.cached_tail.set(tail);
            if head == tail {
                return None;
            }
        }
        Some(head)
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

unsafe impl<T: Send> Send for Receiver<T> {}
impl<T> !Sync for Receiver<T> {}

pub fn new<T>(cap: usize) -> (Sender<T>, Receiver<T>) {

    let size = cap + 1;

    let buffer = Arc::new(RingBuffer::new(size));
    let sender = Sender {
        buffer: buffer.clone(),
        cached_head: Cell::new(0),
        cached_tail: Cell::new(0),
        size,
    };
    let receiver = Receiver {
        buffer: buffer.clone(),
        cached_head: Cell::new(0),
        cached_tail: Cell::new(0),
        size,
    };
    (sender, receiver)
}

pub enum SPSCError {
    Full
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn basic() {
        let (sender, receiver) = new::<usize>(1000);
        let n = 100_000_000;
        let t1 = std::thread::spawn(move || {
            for i in 0..n {
                while sender.send(i).is_err() {

                };
            }
        });
        let t2 = std::thread::spawn(move || {
            let mut list = vec![];
            while list.len() < n {
                if let Ok(val) = receiver.recv() {
                    list.push(val);
                }
            }
            list
        });
        t1.join().unwrap();
        let v = t2.join().unwrap();
        for (i, value) in v.into_iter().enumerate() {
            assert_eq!(i, value);
        }
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