#![allow(unsafe_code)]

use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};
use core::cmp;
use managed::ManagedSlice;
use crate::{Error, Result};
use crate::storage::Resettable;


static CNT: AtomicUsize = AtomicUsize::new(1);

pub fn new_socket_buffer_channel() -> (SocketBufferSender, SocketBufferReceiver) {
    let (tx, rx) = crossbeam::channel::unbounded();
    (
        SocketBufferSender {
            tx,
            id: CNT.fetch_add(1, Ordering::AcqRel),
        },
        SocketBufferReceiver {
            rx,
            id: CNT.fetch_add(1, Ordering::AcqRel),
        },
    )
}
#[derive(Debug)]
pub struct SocketBufferReceiver {
    rx: crossbeam::channel::Receiver<Vec<u8>>,
    pub id: usize,
}

#[derive(Debug)]
pub struct SocketBufferSender {
    tx: crossbeam::channel::Sender<Vec<u8>>,
    pub id: usize,
}
impl Drop for SocketBufferSender {
    fn drop(&mut self) {
        net_debug!("SocketBufferSender dropped {}\n{:?}", self.id, backtrace::Backtrace::new());
    }
}
impl Drop for SocketBufferReceiver {
    fn drop(&mut self) {
        net_debug!("SocketBufferReceiver dropped {}\n{:?}", self.id, backtrace::Backtrace::new());
    }
}
impl Deref for SocketBufferReceiver {
    type Target = crossbeam::channel::Receiver<Vec<u8>>;

    fn deref(&self) -> &Self::Target {
        &self.rx
    }
}

impl Deref for SocketBufferSender {
    type Target = crossbeam::channel::Sender<Vec<u8>>;

    fn deref(&self) -> &Self::Target {
        &self.tx
    }
}
/// A ring buffer.
///
/// This ring buffer implementation provides many ways to interact with it:
///
///   * Enqueueing or dequeueing one element from corresponding side of the buffer;
///   * Enqueueing or dequeueing a slice of elements from corresponding side of the buffer;
///   * Accessing allocated and unallocated areas directly.
///
/// It is also zero-copy; all methods provide references into the buffer's storage.
/// Note that all references are mutable; it is considered more important to allow
/// in-place processing than to protect from accidental mutation.
///
/// This implementation is suitable for both simple uses such as a FIFO queue
/// of UDP packets, and advanced ones such as a TCP reassembly buffer.
#[derive(Debug)]
pub struct RingBufferSync<'a, T: 'a> {
    storage: ManagedSlice<'a, T>,
    read_at: AtomicUsize,
    length: AtomicUsize,
}

impl<'a, T: 'a> RingBufferSync<'a, T> {
    /// Create a ring buffer with the given storage.
    ///
    /// During creation, every element in `storage` is reset.
    pub fn new<S>(storage: S) -> RingBufferSync<'a, T>
        where S: Into<ManagedSlice<'a, T>>,
    {
        RingBufferSync {
            storage: storage.into(),
            read_at: AtomicUsize::new(0),
            length: AtomicUsize::new(0),
        }
    }

    /// Clear the ring buffer.
    pub fn clear(&self) {
        self.read_at.store(0, Ordering::Release);
        self.length.store(0, Ordering::Release);
    }

    /// Return the maximum number of elements in the ring buffer.
    pub fn capacity(&self) -> usize {
        self.storage.len()
    }

    /// Clear the ring buffer, and reset every element.
    pub fn reset(&mut self)
        where T: Resettable {
        self.clear();
        for elem in self.storage.iter_mut() {
            elem.reset();
        }
    }

    /// Return the current number of elements in the ring buffer.
    pub fn len(&self) -> usize {
        self.length.load(Ordering::Acquire)
    }

    /// Return the number of elements that can be added to the ring buffer.
    pub fn window(&self) -> usize {
        self.capacity() - self.len()
    }

    /// Return the largest number of elements that can be added to the buffer
    /// without wrapping around (i.e. in a single `enqueue_many` call).
    pub fn contiguous_window(&self) -> usize {
        cmp::min(self.window(), self.capacity() - self.get_idx(self.length.load(Ordering::Acquire)))
    }

    /// Query whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Query whether the buffer is full.
    pub fn is_full(&self) -> bool {
        self.window() == 0
    }

    /// Shorthand for `(self.read + idx) % self.capacity()` with an
    /// additional check to ensure that the capacity is not zero.
    fn get_idx(&self, idx: usize) -> usize {
        let len = self.capacity();
        if len > 0 {
            (self.read_at.load(Ordering::Acquire) + idx) % len
        } else {
            0
        }
    }

    /// Shorthand for `(self.read + idx) % self.capacity()` with no
    /// additional checks to ensure the capacity is not zero.
    fn get_idx_unchecked(&self, idx: usize) -> usize {
        (self.read_at.load(Ordering::Acquire) + idx) % self.capacity()
    }
}

trait AsMutUnsafe {
    #[allow(unsafe_code)]
    fn as_mut_unsafe(&self) -> &mut Self {
        unsafe { &mut *(self as *const Self as *mut Self) }
    }
}

impl<T: ?Sized> AsMutUnsafe for T {}

/// This is the "discrete" ring buffer interface: it operates with single elements,
/// and boundary conditions (empty/full) are errors.
impl<'a, T: 'a> RingBufferSync<'a, T> {
    /// Call `f` with a single buffer element, and enqueue the element if `f`
    /// returns successfully, or return `Err(Error::Exhausted)` if the buffer is full.
    pub fn enqueue_one_with<'b, R, F>(&'b self, f: F) -> Result<R>
        where F: FnOnce(&'b mut T) -> Result<R> {
        if self.is_full() { return Err(Error::Exhausted); }

        let index = self.get_idx_unchecked(self.length.load(Ordering::Acquire));
        match f(self.storage[index].as_mut_unsafe()) {
            Ok(result) => {
                self.length.fetch_add(1, Ordering::AcqRel);
                Ok(result)
            }
            Err(error) => Err(error)
        }
    }

    /// Enqueue a single element into the buffer, and return a reference to it,
    /// or return `Err(Error::Exhausted)` if the buffer is full.
    ///
    /// This function is a shortcut for `ring_buf.enqueue_one_with(Ok)`.
    pub fn enqueue_one(&self) -> Result<&mut T> {
        self.enqueue_one_with(Ok)
    }

    /// Call `f` with a single buffer element, and dequeue the element if `f`
    /// returns successfully, or return `Err(Error::Exhausted)` if the buffer is empty.
    pub fn dequeue_one_with<'b, R, F>(&'b self, f: F) -> Result<R>
        where F: FnOnce(&'b T) -> Result<R> {
        if self.is_empty() { return Err(Error::Exhausted); }

        let next_at = self.get_idx_unchecked(1);
        match f(self.storage[self.read_at.load(Ordering::Acquire)].as_mut_unsafe()) {
            Ok(result) => {
                self.length.fetch_sub(1, Ordering::AcqRel);
                self.read_at.store(next_at, Ordering::Release);
                Ok(result)
            }
            Err(error) => Err(error)
        }
    }

    /// Dequeue an element from the buffer, and return a reference to it,
    /// or return `Err(Error::Exhausted)` if the buffer is empty.
    ///
    /// This function is a shortcut for `ring_buf.dequeue_one_with(Ok)`.
    pub fn dequeue_one(&self) -> Result<&T> {
        self.dequeue_one_with(Ok)
    }
}

/// This is the "continuous" ring buffer interface: it operates with element slices,
/// and boundary conditions (empty/full) simply result in empty slices.
impl<'a, T: 'a> RingBufferSync<'a, T> {
    /// Call `f` with the largest contiguous slice of unallocated buffer elements,
    /// and enqueue the amount of elements returned by `f`.
    ///
    /// # Panics
    /// This function panics if the amount of elements returned by `f` is larger
    /// than the size of the slice passed into it.
    pub fn enqueue_many_with<'b, R, F>(&'b self, f: F) -> (usize, R)
        where F: FnOnce(&'b mut [T]) -> (usize, R) {
        if self.length.load(Ordering::Acquire) == 0 {
            // Ring is currently empty. Reset `read_at` to optimize
            // for contiguous space.
            self.read_at.store(0, Ordering::Release);
        }

        let write_at = self.get_idx(self.length.load(Ordering::Acquire));
        let max_size = self.contiguous_window();
        let (size, result) = f(self.storage[write_at..write_at + max_size].as_mut_unsafe());
        assert!(size <= max_size);
        self.length.fetch_add(size, Ordering::AcqRel);
        (size, result)
    }

    /// Enqueue a slice of elements up to the given size into the buffer,
    /// and return a reference to them.
    ///
    /// This function may return a slice smaller than the given size
    /// if the free space in the buffer is not contiguous.
    // #[must_use]
    pub fn enqueue_many(&self, size: usize) -> &mut [T] {
        self.enqueue_many_with(|buf| {
            let size = cmp::min(size, buf.len());
            (size, &mut buf[..size])
        }).1
    }

    /// Enqueue as many elements from the given slice into the buffer as possible,
    /// and return the amount of elements that could fit.
    // #[must_use]
    pub fn enqueue_slice(&self, data: &[T]) -> usize
        where T: Copy {
        let (size_1, data) = self.enqueue_many_with(|buf| {
            let size = cmp::min(buf.len(), data.len());
            buf[..size].copy_from_slice(&data[..size]);
            (size, &data[size..])
        });
        let (size_2, ()) = self.enqueue_many_with(|buf| {
            let size = cmp::min(buf.len(), data.len());
            buf[..size].copy_from_slice(&data[..size]);
            (size, ())
        });
        size_1 + size_2
    }

    /// Call `f` with the largest contiguous slice of allocated buffer elements,
    /// and dequeue the amount of elements returned by `f`.
    ///
    /// # Panics
    /// This function panics if the amount of elements returned by `f` is larger
    /// than the size of the slice passed into it.
    pub fn dequeue_many_with<'b, R, F>(&'b self, f: F) -> (usize, R)
        where F: FnOnce(&'b [T]) -> (usize, R) {
        let capacity = self.capacity();
        let max_size = cmp::min(self.len(), capacity - self.read_at.load(Ordering::Acquire));
        let (size, result) = f(self.storage[self.read_at.load(Ordering::Acquire)..self.read_at.load(Ordering::Acquire) + max_size].as_mut_unsafe());
        assert!(size <= max_size);
        self.read_at.store(if capacity > 0 {
            (self.read_at.load(Ordering::Acquire) + size) % capacity
        } else {
            0
        }, Ordering::Release);
        self.length.fetch_sub(size, Ordering::AcqRel);
        (size, result)
    }

    /// Dequeue a slice of elements up to the given size from the buffer,
    /// and return a reference to them.
    ///
    /// This function may return a slice smaller than the given size
    /// if the allocated space in the buffer is not contiguous.
    // #[must_use]
    pub fn dequeue_many(&self, size: usize) -> &[T] {
        self.dequeue_many_with(|buf| {
            let size = cmp::min(size, buf.len());
            (size, &buf[..size])
        }).1
    }

    /// Dequeue as many elements from the buffer into the given slice as possible,
    /// and return the amount of elements that could fit.
    // #[must_use]
    pub fn dequeue_slice(&self, data: &mut [T]) -> usize
        where T: Copy {
        let (size_1, data) = self.dequeue_many_with(|buf| {
            let size = cmp::min(buf.len(), data.len());
            data[..size].copy_from_slice(&buf[..size]);
            (size, &mut data[size..])
        });
        let (size_2, ()) = self.dequeue_many_with(|buf| {
            let size = cmp::min(buf.len(), data.len());
            data[..size].copy_from_slice(&buf[..size]);
            (size, ())
        });
        size_1 + size_2
    }
}

/// This is the "random access" ring buffer interface: it operates with element slices,
/// and allows to access elements of the buffer that are not adjacent to its head or tail.
impl<'a, T: 'a> RingBufferSync<'a, T> {
    /// Return the largest contiguous slice of unallocated buffer elements starting
    /// at the given offset past the last allocated element, and up to the given size.
    // #[must_use]
    pub fn get_unallocated(&self, offset: usize, mut size: usize) -> &mut [T] {
        let start_at = self.get_idx(self.length.load(Ordering::Acquire) + offset);
        // We can't access past the end of unallocated data.
        if offset > self.window() { return &mut []; }
        // We can't enqueue more than there is free space.
        let clamped_window = self.window() - offset;
        if size > clamped_window { size = clamped_window }
        // We can't contiguously enqueue past the end of the storage.
        let until_end = self.capacity() - start_at;
        if size > until_end { size = until_end }

        self.storage[start_at..start_at + size].as_mut_unsafe()
    }

    /// Write as many elements from the given slice into unallocated buffer elements
    /// starting at the given offset past the last allocated element, and return
    /// the amount written.
    // #[must_use]
    pub fn write_unallocated(&self, offset: usize, data: &[T]) -> usize
        where T: Copy {
        net_debug!("write unallocated {} {}", offset, data.len());
        let (size_1, offset, data) = {
            let slice = self.get_unallocated(offset, data.len());
            let slice_len = slice.len();
            slice.copy_from_slice(&data[..slice_len]);
            (slice_len, offset + slice_len, &data[slice_len..])
        };
        let size_2 = {
            let slice = self.get_unallocated(offset, data.len());
            let slice_len = slice.len();
            slice.copy_from_slice(&data[..slice_len]);
            slice_len
        };
        size_1 + size_2
    }

    /// Enqueue the given number of unallocated buffer elements.
    ///
    /// # Panics
    /// Panics if the number of elements given exceeds the number of unallocated elements.
    pub fn enqueue_unallocated(&self, count: usize) {
        net_debug!("Enqueue unallocated {}", count);
        assert!(count <= self.window());
        self.length.fetch_add(count, Ordering::AcqRel);
    }

    /// Return the largest contiguous slice of allocated buffer elements starting
    /// at the given offset past the first allocated element, and up to the given size.
    // #[must_use]
    pub fn get_allocated(&self, offset: usize, mut size: usize) -> &[T] {
        let start_at = self.get_idx(offset);
        // We can't read past the end of the allocated data.
        if offset > self.length.load(Ordering::Acquire) { return &mut []; }
        // We can't read more than we have allocated.
        let clamped_length = self.length.load(Ordering::Acquire) - offset;
        if size > clamped_length { size = clamped_length }
        // We can't contiguously dequeue past the end of the storage.
        let until_end = self.capacity() - start_at;
        if size > until_end { size = until_end }

        &self.storage[start_at..start_at + size]
    }

    /// Read as many elements from allocated buffer elements into the given slice
    /// starting at the given offset past the first allocated element, and return
    /// the amount read.
    // #[must_use]
    pub fn read_allocated(&self, offset: usize, data: &mut [T]) -> usize
        where T: Copy {
        net_debug!("read allocated {} {}", offset, data.len());
        let (size_1, offset, data) = {
            let slice = self.get_allocated(offset, data.len());
            data[..slice.len()].copy_from_slice(slice);
            (slice.len(), offset + slice.len(), &mut data[slice.len()..])
        };
        let size_2 = {
            let slice = self.get_allocated(offset, data.len());
            data[..slice.len()].copy_from_slice(slice);
            slice.len()
        };
        size_1 + size_2
    }

    /// Dequeue the given number of allocated buffer elements.
    ///
    /// # Panics
    /// Panics if the number of elements given exceeds the number of allocated elements.
    pub fn dequeue_allocated(&self, count: usize) {
        net_debug!("Dequeue allocated {}", count);
        assert!(count <= self.len());
        self.length.fetch_sub(count, Ordering::AcqRel);
        self.read_at.store(self.get_idx(count), Ordering::Release);
    }
}

impl<'a, T: 'a> From<ManagedSlice<'a, T>> for RingBufferSync<'a, T> {
    fn from(slice: ManagedSlice<'a, T>) -> RingBufferSync<'a, T> {
        RingBufferSync::new(slice)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_buffer_length_changes() {
        let ring = RingBufferSync::new(vec![0; 2]);
        assert!(ring.is_empty());
        assert!(!ring.is_full());
        assert_eq!(ring.len(), 0);
        assert_eq!(ring.capacity(), 2);
        assert_eq!(ring.window(), 2);

        ring.length.store(1, Ordering::Release);
        assert!(!ring.is_empty());
        assert!(!ring.is_full());
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.capacity(), 2);
        assert_eq!(ring.window(), 1);

        ring.length.store(2, Ordering::Release);
        assert!(!ring.is_empty());
        assert!(ring.is_full());
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.capacity(), 2);
        assert_eq!(ring.window(), 0);
    }

    #[test]
    fn test_buffer_enqueue_dequeue_one_with() {
        let ring = RingBufferSync::new(vec![0; 5]);
        assert_eq!(ring.dequeue_one_with(|_| unreachable!()) as Result<()>,
                   Err(Error::Exhausted));

        ring.enqueue_one_with(Ok).unwrap();
        assert!(!ring.is_empty());
        assert!(!ring.is_full());

        for i in 1..5 {
            ring.enqueue_one_with(|e| Ok(*e = i)).unwrap();
            assert!(!ring.is_empty());
        }
        assert!(ring.is_full());
        assert_eq!(ring.enqueue_one_with(|_| unreachable!()) as Result<()>,
                   Err(Error::Exhausted));

        for i in 0..5 {
            assert_eq!(ring.dequeue_one_with(|e| Ok(*e)).unwrap(), i);
            assert!(!ring.is_full());
        }
        assert_eq!(ring.dequeue_one_with(|_| unreachable!()) as Result<()>,
                   Err(Error::Exhausted));
        assert!(ring.is_empty());
    }

    #[test]
    fn test_buffer_enqueue_dequeue_one() {
        let ring = RingBufferSync::new(vec![0; 5]);
        assert_eq!(ring.dequeue_one(), Err(Error::Exhausted));

        ring.enqueue_one().unwrap();
        assert!(!ring.is_empty());
        assert!(!ring.is_full());

        for i in 1..5 {
            *ring.enqueue_one().unwrap() = i;
            assert!(!ring.is_empty());
        }
        assert!(ring.is_full());
        assert_eq!(ring.enqueue_one(), Err(Error::Exhausted));

        for i in 0..5 {
            assert_eq!(*ring.dequeue_one().unwrap(), i);
            assert!(!ring.is_full());
        }
        assert_eq!(ring.dequeue_one(), Err(Error::Exhausted));
        assert!(ring.is_empty());
    }

    #[test]
    fn test_buffer_enqueue_many_with() {
        let ring = RingBufferSync::new(vec![b'.'; 12]);

        assert_eq!(ring.enqueue_many_with(|buf| {
            assert_eq!(buf.len(), 12);
            buf[0..2].copy_from_slice(b"ab");
            (2, true)
        }), (2, true));
        assert_eq!(ring.len(), 2);
        assert_eq!(&ring.storage[..], b"ab..........");

        ring.enqueue_many_with(|buf| {
            assert_eq!(buf.len(), 12 - 2);
            buf[0..4].copy_from_slice(b"cdXX");
            (2, ())
        });
        assert_eq!(ring.len(), 4);
        assert_eq!(&ring.storage[..], b"abcdXX......");

        ring.enqueue_many_with(|buf| {
            assert_eq!(buf.len(), 12 - 4);
            buf[0..4].copy_from_slice(b"efgh");
            (4, ())
        });
        assert_eq!(ring.len(), 8);
        assert_eq!(&ring.storage[..], b"abcdefgh....");

        for _ in 0..4 {
            let _ = ring.dequeue_one();
        }
        assert_eq!(ring.len(), 4);
        assert_eq!(&ring.storage[..], b"abcdefgh....");

        ring.enqueue_many_with(|buf| {
            assert_eq!(buf.len(), 12 - 8);
            buf[0..4].copy_from_slice(b"ijkl");
            (4, ())
        });
        assert_eq!(ring.len(), 8);
        assert_eq!(&ring.storage[..], b"abcdefghijkl");

        ring.enqueue_many_with(|buf| {
            assert_eq!(buf.len(), 4);
            buf[0..4].copy_from_slice(b"abcd");
            (4, ())
        });
        assert_eq!(ring.len(), 12);
        assert_eq!(&ring.storage[..], b"abcdefghijkl");

        for _ in 0..4 {
            let _ = ring.dequeue_one();
        }
        assert_eq!(ring.len(), 8);
        assert_eq!(&ring.storage[..], b"abcdefghijkl");
    }

    #[test]
    fn test_buffer_enqueue_many() {
        let ring = RingBufferSync::new(vec![b'.'; 12]);

        ring.enqueue_many(8).copy_from_slice(b"abcdefgh");
        assert_eq!(ring.len(), 8);
        assert_eq!(&ring.storage[..], b"abcdefgh....");

        ring.enqueue_many(8).copy_from_slice(b"ijkl");
        assert_eq!(ring.len(), 12);
        assert_eq!(&ring.storage[..], b"abcdefghijkl");
    }

    #[test]
    fn test_buffer_enqueue_slice() {
        let ring = RingBufferSync::new(vec![b'.'; 12]);

        assert_eq!(ring.enqueue_slice(b"abcdefgh"), 8);
        assert_eq!(ring.len(), 8);
        assert_eq!(&ring.storage[..], b"abcdefgh....");

        for _ in 0..4 {
            let _ = ring.dequeue_one();
        }

        assert_eq!(ring.enqueue_slice(b"ijklabcd"), 8);
        assert_eq!(ring.len(), 12);
        assert_eq!(&ring.storage[..], b"abcdefghijkl");
    }

    #[test]
    fn test_buffer_dequeue_slice() {
        let ring = RingBufferSync::new(vec![b'.'; 12]);

        assert_eq!(ring.enqueue_slice(b"abcdefghijkl"), 12);

        {
            let mut buf = [0; 8];
            assert_eq!(ring.dequeue_slice(&mut buf[..]), 8);
            assert_eq!(&buf[..], b"abcdefgh");
            assert_eq!(ring.len(), 4);
        }

        assert_eq!(ring.enqueue_slice(b"abcd"), 4);

        {
            let mut buf = [0; 8];
            assert_eq!(ring.dequeue_slice(&mut buf[..]), 8);
            assert_eq!(&buf[..], b"ijklabcd");
            assert_eq!(ring.len(), 0);
        }
    }

    #[test]
    fn test_buffer_with_no_capacity() {
        let no_capacity: RingBufferSync<u8> = RingBufferSync::new(vec![]);

        // Call all functions that calculate the remainder against rx_buffer.capacity()
        // with a backing storage with a length of 0.
        assert_eq!(no_capacity.get_unallocated(0, 0), &[]);
        assert_eq!(no_capacity.get_allocated(0, 0), &[]);
        no_capacity.dequeue_allocated(0);
        assert_eq!(no_capacity.enqueue_many(0), &[]);
        assert_eq!(no_capacity.enqueue_one(), Err(Error::Exhausted));
        assert_eq!(no_capacity.contiguous_window(), 0);
    }

    /// Use the buffer a bit. Then empty it and put in an item of
    /// maximum size. By detecting a length of 0, the implementation
    /// can reset the current buffer position.
    #[test]
    fn test_buffer_write_wholly() {
        let ring = RingBufferSync::new(vec![b'.'; 8]);
        ring.enqueue_many(2).copy_from_slice(b"xx");
        ring.enqueue_many(2).copy_from_slice(b"xx");
        assert_eq!(ring.len(), 4);
        ring.dequeue_many(4);
        assert_eq!(ring.len(), 0);

        let large = ring.enqueue_many(8);
        assert_eq!(large.len(), 8);
    }
}
