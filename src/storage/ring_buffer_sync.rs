#![allow(unsafe_code)]

use std::sync::atomic::{Ordering, AtomicU32, AtomicU64, AtomicUsize};
use core::cmp;
use managed::ManagedSlice;
use crate::{Result, Error};
use std::sync::Arc;
use crate::wire::{IpEndpoint};
use crate::socket::SocketMeta;
use std::fmt::{Debug, Formatter};

#[derive(Debug)]
pub struct SocketBufferReceiver<'a> {
    pub(crate) rx_buffer: Arc<RingBufferSync<'a, u8>>,
    pub(crate) remote_seq_no: Arc<AtomicUsize>,
    pub(crate) local_endpoint: IpEndpoint,
    pub(crate) remote_endpoint: IpEndpoint,
    pub(crate) meta: SocketMeta,
}

// impl<'a> Drop for SocketBufferReceiver<'a> {
//     fn drop(&mut self) {
//         net_debug!("SocketBufferReceiver dropped\n{:?}", backtrace::Backtrace::new());
//     }
// }

impl<'a> SocketBufferReceiver<'a> {
    fn recv_impl<'b, F, R>(&'b mut self, f: F) -> Result<R>
        where F: FnOnce(&'b RingBufferSync<'a, u8>) -> (usize, R) {
        // self.recv_error_check()?;

        let _old_length = self.rx_buffer.len();
        let (size, result) = f(&self.rx_buffer);
        self.remote_seq_no.fetch_add(size, Ordering::AcqRel);
        if size > 0 {
            #[cfg(any(test, feature = "verbose"))]
            net_trace!("{}:{}:{}: rx buffer: dequeueing {} octets (now {})",
                       self.meta.handle, self.local_endpoint, self.remote_endpoint,
                       size, _old_length as isize - size as isize);
        }
        Ok(result)
    }
    /// Call `f` with the largest contiguous slice of octets in the receive buffer,
    /// and dequeue the amount of elements returned by `f`.
    ///
    /// This function errors if the receive half of the connection is not open.
    ///
    /// If the receive half has been gracefully closed (with a FIN packet), `Err(Error::Finished)`
    /// is returned. In this case, the previously received data is guaranteed to be complete.
    ///
    /// In all other cases, `Err(Error::Illegal)` is returned and previously received data (if any)
    /// may be incomplete (truncated).
    pub fn recv<'b, F, R>(&'b mut self, f: F) -> Result<R>
        where F: FnOnce(&'b mut [u8]) -> (usize, R) {
        self.recv_impl(|rx_buffer| {
            rx_buffer.dequeue_many_with(f)
        })
    }

    /// Dequeue a sequence of received octets, and fill a slice from it.
    ///
    /// This function returns the amount of octets actually dequeued, which is limited
    /// by the amount of occupied space in the receive buffer; down to zero.
    ///
    /// See also [recv](#method.recv).
    pub fn recv_slice(&mut self, data: &mut [u8]) -> Result<usize> {
        self.recv_impl(|rx_buffer| {
            let size = rx_buffer.dequeue_slice(data);
            (size, size)
        })
    }

    /// Peek at a sequence of received octets without removing them from
    /// the receive buffer, and return a pointer to it.
    ///
    /// This function otherwise behaves identically to [recv](#method.recv).
    pub fn peek(&mut self, size: usize) -> Result<&[u8]> {
        // self.recv_error_check()?;

        let buffer = self.rx_buffer.get_allocated(0, size);
        if !buffer.is_empty() {
            #[cfg(any(test, feature = "verbose"))]
            net_trace!("{}:{}:{}: rx buffer: peeking at {} octets",
                       self.meta.handle, self.local_endpoint, self.remote_endpoint,
                       buffer.len());
        }
        Ok(buffer)
    }

    /// Peek at a sequence of received octets without removing them from
    /// the receive buffer, and fill a slice from it.
    ///
    /// This function otherwise behaves identically to [recv_slice](#method.recv_slice).
    pub fn peek_slice(&mut self, data: &mut [u8]) -> Result<usize> {
        let buffer = self.peek(data.len())?;
        let data = &mut data[..buffer.len()];
        data.copy_from_slice(buffer);
        Ok(buffer.len())
    }

    /// Return the amount of octets queued in the receive buffer. This value can be larger than
    /// the slice read by the next `recv` or `peek` call because it includes all queued octets,
    /// and not only the octets that may be returned as a contiguous slice.
    ///
    /// Note that the Berkeley sockets interface does not have an equivalent of this API.
    pub fn recv_queue(&self) -> usize {
        self.rx_buffer.len()
    }
}

#[repr(C)]
struct BundledAtomic {
    pub read_at: AtomicU32,
    pub write_at: AtomicU32,
}

impl BundledAtomic {
    fn as_atomic_usize(&self) -> &AtomicU64 {
        unsafe { std::mem::transmute(self) }
    }
    fn load_pair(&self, order: Ordering) -> (usize, usize) {
        let (read_at, write_at): (u32, u32) = unsafe {
            std::mem::transmute(self.as_atomic_usize().load(order))
        };
        (read_at as usize, write_at as usize)
    }
}

pub struct RingBufferSync<'a, T: 'a> {
    storage: ManagedSlice<'a, T>,
    rw: BundledAtomic,
}

impl<'a, T: 'a> Debug for RingBufferSync<'a, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferSync").finish_non_exhaustive()
    }
}

impl<'a, T: 'a> RingBufferSync<'a, T> {
    /// Create a ring buffer with the given storage.
    ///
    /// During creation, every element in `storage` is reset.
    pub fn new<S>(storage: S) -> RingBufferSync<'a, T>
        where S: Into<ManagedSlice<'a, T>>,
    {
        let storage = storage.into();
        assert_ne!(storage.len(), 0, "Should not be 0 capacity");
        RingBufferSync {
            storage,
            rw: BundledAtomic {
                read_at: AtomicU32::new(0),
                write_at: AtomicU32::new(0),
            },
        }
    }

    /// Clear the ring buffer.
    pub fn clear(&self) {
        self.rw.read_at.store(0, Ordering::Release);
        self.rw.write_at.store(0, Ordering::Release);
    }

    /// Return the maximum number of elements in the ring buffer.
    pub fn capacity(&self) -> usize {
        self.storage.len()
    }

    /// Return the current number of elements in the ring buffer.
    pub fn len(&self) -> usize {
        let (read_at, write_at) = self.rw.load_pair(Ordering::Acquire);
        (write_at - read_at) as usize
    }

    /// Return the number of elements that can be added to the ring buffer.
    pub fn window(&self) -> usize {
        self.capacity() - self.len()
    }

    /// Return the largest number of elements that can be added to the buffer
    /// without wrapping around (i.e. in a single `enqueue_many` call).
    pub fn contiguous_window(&self) -> usize {
        self.window()
    }

    /// Query whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Query whether the buffer is full.
    pub fn is_full(&self) -> bool {
        self.window() == 0
    }

    fn get_abs_idx(&self, idx: usize) -> usize {
        let len = self.capacity();
        idx % len
    }

    fn max_write_slice(&self) -> &mut [T] {
        let cap = self.storage.len();
        let (read_at, write_at) = self.rw.load_pair(Ordering::Acquire);

        let begin = write_at % cap;
        let len = write_at - read_at;
        let end = std::cmp::min(begin + cap - len, cap);
        self.storage[begin..end].as_mut_unsafe()
    }
    fn max_read_slice(&self) -> &mut [T] {
        let cap = self.storage.len();
        let (read_at, write_at) = self.rw.load_pair(Ordering::Acquire);
        let begin = read_at % cap;
        let len = write_at - read_at;
        let end = std::cmp::min(begin + len, cap);
        self.storage[begin..end].as_mut_unsafe()
    }
}

trait AsMutUnsafe {
    #[allow(unsafe_code)]
    fn as_mut_unsafe(&self) -> &mut Self {
        unsafe { &mut *(self as *const Self as *mut Self) }
    }
}

impl<T: ?Sized> AsMutUnsafe for T {}


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
        let max_write_slice = self.max_write_slice();
        let old_len = max_write_slice.len();
        let (size, result) = f(max_write_slice);
        assert!(size <= old_len);
        self.rw.write_at.fetch_add(size as _, Ordering::AcqRel);
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
        where F: FnOnce(&'b mut [T]) -> (usize, R) {
        let max_read_slice = self.max_read_slice();
        let max_size = max_read_slice.len();
        let (size, result) = f(max_read_slice);
        assert!(size <= max_size);
        self.rw.read_at.fetch_add(size as _, Ordering::AcqRel);
        (size, result)
    }

    /// Dequeue a slice of elements up to the given size from the buffer,
    /// and return a reference to them.
    ///
    /// This function may return a slice smaller than the given size
    /// if the allocated space in the buffer is not contiguous.
    // #[must_use]
    pub fn dequeue_many(&self, size: usize) -> &mut [T] {
        self.dequeue_many_with(|buf| {
            let size = cmp::min(size, buf.len());
            (size, &mut buf[..size])
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

/// This is the "discrete" ring buffer interface: it operates with single elements,
/// and boundary conditions (empty/full) are errors.
impl<'a, T: 'a> RingBufferSync<'a, T> {
    /// Call `f` with a single buffer element, and enqueue the element if `f`
    /// returns successfully, or return `Err(Error::Exhausted)` if the buffer is full.
    pub fn enqueue_one_with<'b, R, F>(&'b self, f: F) -> Result<R>
        where F: FnOnce(&'b mut T) -> Result<R> {
        let (read_at, write_at) = self.rw.load_pair(Ordering::Relaxed);
        if write_at == read_at + self.storage.len() { return Err(Error::Exhausted) }

        let index = self.get_abs_idx(write_at);
        match f(self.storage[index].as_mut_unsafe()) {
            Ok(result) => {
                self.rw.write_at.fetch_add(1, Ordering::AcqRel);
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
        where F: FnOnce(&'b mut T) -> Result<R> {
        let (read_at, write_at) = self.rw.load_pair(Ordering::Relaxed);
        if read_at == write_at { return Err(Error::Exhausted) }

        match f(self.storage[read_at].as_mut_unsafe()) {
            Ok(result) => {
                self.rw.read_at.fetch_add(1, Ordering::AcqRel);
                Ok(result)
            }
            Err(error) => Err(error)
        }
    }

    /// Dequeue an element from the buffer, and return a reference to it,
    /// or return `Err(Error::Exhausted)` if the buffer is empty.
    ///
    /// This function is a shortcut for `ring_buf.dequeue_one_with(Ok)`.
    pub fn dequeue_one(&self) -> Result<&mut T> {
        self.dequeue_one_with(Ok)
    }
}


/// This is the "random access" ring buffer interface: it operates with element slices,
/// and allows to access elements of the buffer that are not adjacent to its head or tail.
impl<'a, T: 'a> RingBufferSync<'a, T> {
    /// Return the largest contiguous slice of unallocated buffer elements starting
    /// at the given offset past the last allocated element, and up to the given size.
    // #[must_use]
    pub fn get_unallocated(&self, offset: usize, mut size: usize) -> &mut [T] {
        let (read_at, write_at) = self.rw.load_pair(Ordering::Acquire);
        let length = write_at - read_at;
        let window = self.storage.len() - length;
        let start_at = self.get_abs_idx(read_at + length + offset);
        // We can't access past the end of unallocated data.
        if offset > window { return &mut [] }
        // We can't enqueue more than there is free space.
        let clamped_window = window - offset;
        if size > clamped_window { size = clamped_window }
        // We can't contiguously enqueue past the end of the storage.
        let until_end = self.storage.len() - start_at;
        if size > until_end { size = until_end }

        self.storage[start_at..start_at + size].as_mut_unsafe()
    }

    /// Write as many elements from the given slice into unallocated buffer elements
    /// starting at the given offset past the last allocated element, and return
    /// the amount written.
    // #[must_use]
    pub fn write_unallocated(&self, offset: usize, data: &[T]) -> usize
        where T: Copy {
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
        assert!(count <= self.window());
        self.rw.write_at.fetch_add(count as _, Ordering::AcqRel);
    }

    /// Return the largest contiguous slice of allocated buffer elements starting
    /// at the given offset past the first allocated element, and up to the given size.
    // #[must_use]
    pub fn get_allocated(&self, offset: usize, mut size: usize) -> &[T] {
        let (read_at, write_at) = self.rw.load_pair(Ordering::Acquire);
        let start_at = self.get_abs_idx(read_at + offset);
        let length = write_at - read_at;
        // We can't read past the end of the allocated data.
        if offset > length { return &mut [] }
        // We can't read more than we have allocated.
        let clamped_length = length - offset;
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
        self.rw.read_at.fetch_add(count as _, Ordering::Release);
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

        ring.rw.write_at.store(1, Ordering::Relaxed);
        assert!(!ring.is_empty());
        assert!(!ring.is_full());
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.capacity(), 2);
        assert_eq!(ring.window(), 1);

        ring.rw.write_at.store(2, Ordering::Relaxed);
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
            *ring.dequeue_one().unwrap() = b'.';
        }
        assert_eq!(ring.len(), 4);
        assert_eq!(&ring.storage[..], b"....efgh....");

        ring.enqueue_many_with(|buf| {
            assert_eq!(buf.len(), 12 - 8);
            buf[0..4].copy_from_slice(b"ijkl");
            (4, ())
        });
        assert_eq!(ring.len(), 8);
        assert_eq!(&ring.storage[..], b"....efghijkl");

        ring.enqueue_many_with(|buf| {
            assert_eq!(buf.len(), 4);
            buf[0..4].copy_from_slice(b"abcd");
            (4, ())
        });
        assert_eq!(ring.len(), 12);
        assert_eq!(&ring.storage[..], b"abcdefghijkl");

        for _ in 0..4 {
            *ring.dequeue_one().unwrap() = b'.';
        }
        assert_eq!(ring.len(), 8);
        assert_eq!(&ring.storage[..], b"abcd....ijkl");
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
            *ring.dequeue_one().unwrap() = b'.';
        }
        assert_eq!(ring.len(), 4);
        assert_eq!(&ring.storage[..], b"....efgh....");

        assert_eq!(ring.enqueue_slice(b"ijklabcd"), 8);
        assert_eq!(ring.len(), 12);
        assert_eq!(&ring.storage[..], b"abcdefghijkl");
    }

    #[test]
    fn test_buffer_dequeue_many_with() {
        let ring = RingBufferSync::new(vec![b'.'; 12]);

        assert_eq!(ring.enqueue_slice(b"abcdefghijkl"), 12);

        assert_eq!(ring.dequeue_many_with(|buf| {
            assert_eq!(buf.len(), 12);
            assert_eq!(buf, b"abcdefghijkl");
            buf[..4].copy_from_slice(b"....");
            (4, true)
        }), (4, true));
        assert_eq!(ring.len(), 8);
        assert_eq!(&ring.storage[..], b"....efghijkl");

        ring.dequeue_many_with(|buf| {
            assert_eq!(buf, b"efghijkl");
            buf[..4].copy_from_slice(b"....");
            (4, ())
        });
        assert_eq!(ring.len(), 4);
        assert_eq!(&ring.storage[..], b"........ijkl");

        assert_eq!(ring.enqueue_slice(b"abcd"), 4);
        assert_eq!(ring.len(), 8);

        ring.dequeue_many_with(|buf| {
            assert_eq!(buf, b"ijkl");
            buf[..4].copy_from_slice(b"....");
            (4, ())
        });
        ring.dequeue_many_with(|buf| {
            assert_eq!(buf, b"abcd");
            buf[..4].copy_from_slice(b"....");
            (4, ())
        });
        assert_eq!(ring.len(), 0);
        assert_eq!(&ring.storage[..], b"............");
    }

    #[test]
    fn test_buffer_dequeue_many() {
        let ring = RingBufferSync::new(vec![b'.'; 12]);

        assert_eq!(ring.enqueue_slice(b"abcdefghijkl"), 12);

        {
            let buf = ring.dequeue_many(8);
            assert_eq!(buf, b"abcdefgh");
            buf.copy_from_slice(b"........");
        }
        assert_eq!(ring.len(), 4);
        assert_eq!(&ring.storage[..], b"........ijkl");

        {
            let buf = ring.dequeue_many(8);
            assert_eq!(buf, b"ijkl");
            buf.copy_from_slice(b"....");
        }
        assert_eq!(ring.len(), 0);
        assert_eq!(&ring.storage[..], b"............");
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
    fn test_buffer_get_unallocated() {
        let ring = RingBufferSync::new(vec![b'.'; 12]);

        assert_eq!(ring.get_unallocated(16, 4), b"");

        {
            let buf = ring.get_unallocated(0, 4);
            buf.copy_from_slice(b"abcd");
        }
        assert_eq!(&ring.storage[..], b"abcd........");

        ring.enqueue_many(4);
        assert_eq!(ring.len(), 4);

        {
            let buf = ring.get_unallocated(4, 8);
            buf.copy_from_slice(b"ijkl");
        }
        assert_eq!(&ring.storage[..], b"abcd....ijkl");

        ring.enqueue_many(8).copy_from_slice(b"EFGHIJKL");
        ring.dequeue_many(4).copy_from_slice(b"abcd");
        assert_eq!(ring.len(), 8);
        assert_eq!(&ring.storage[..], b"abcdEFGHIJKL");

        {
            let buf = ring.get_unallocated(0, 8);
            buf.copy_from_slice(b"ABCD");
        }
        assert_eq!(&ring.storage[..], b"ABCDEFGHIJKL");
    }

    #[test]
    fn test_buffer_write_unallocated() {
        let ring = RingBufferSync::new(vec![b'.'; 12]);
        ring.enqueue_many(6).copy_from_slice(b"abcdef");
        ring.dequeue_many(6).copy_from_slice(b"ABCDEF");

        assert_eq!(ring.write_unallocated(0, b"ghi"), 3);
        assert_eq!(ring.get_unallocated(0, 3), b"ghi");

        assert_eq!(ring.write_unallocated(3, b"jklmno"), 6);
        assert_eq!(ring.get_unallocated(3, 3), b"jkl");

        assert_eq!(ring.write_unallocated(9, b"pqrstu"), 3);
        assert_eq!(ring.get_unallocated(9, 3), b"pqr");
    }

    #[test]
    fn test_buffer_get_allocated() {
        let ring = RingBufferSync::new(vec![b'.'; 12]);

        assert_eq!(ring.get_allocated(16, 4), b"");
        assert_eq!(ring.get_allocated(0, 4),  b"");

        ring.enqueue_slice(b"abcd");
        assert_eq!(ring.get_allocated(0, 8), b"abcd");

        ring.enqueue_slice(b"efghijkl");
        ring.dequeue_many(4).copy_from_slice(b"....");
        assert_eq!(ring.get_allocated(4, 8), b"ijkl");

        ring.enqueue_slice(b"abcd");
        assert_eq!(ring.get_allocated(4, 8), b"ijkl");
    }

    #[test]
    fn test_buffer_read_allocated() {
        let ring = RingBufferSync::new(vec![b'.'; 12]);
        ring.enqueue_many(12).copy_from_slice(b"abcdefghijkl");

        let mut data = [0; 6];
        assert_eq!(ring.read_allocated(0, &mut data[..]), 6);
        assert_eq!(&data[..], b"abcdef");

        ring.dequeue_many(6).copy_from_slice(b"ABCDEF");
        ring.enqueue_many(3).copy_from_slice(b"mno");

        let mut data = [0; 6];
        assert_eq!(ring.read_allocated(3, &mut data[..]), 6);
        assert_eq!(&data[..], b"jklmno");

        let mut data = [0; 6];
        assert_eq!(ring.read_allocated(6, &mut data[..]), 3);
        assert_eq!(&data[..], b"mno\x00\x00\x00");

    }

    #[test]
    #[should_panic]
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
        assert_eq!(large.len(), 4);
    }

    #[test]
    fn read_write_async() {
        let segment = b"abcdef";
        let count = 200;
        let writer = Arc::new(RingBufferSync::new(vec![0u8; segment.len() * count]));
        let reader = Arc::clone(&writer);
        let t1 = std::thread::spawn(move || {
            for _i in 0..count {
                writer.enqueue_slice(segment);
            }
        });
        let t2 = std::thread::spawn(move || {
            let mut i = 0;
            while i < count {
                reader.dequeue_many_with(|x| {
                    if x.len() >= segment.len() {
                        let mut result = [0u8; 6];
                        result.copy_from_slice(&x[..segment.len()]);
                        // println!("{:?}", &x[..segment.len()]);
                        assert_eq!(&result, segment);
                        i += 1;
                        (segment.len(), ())
                    } else {
                        (0, ())
                    }
                });
            }
        });
        t1.join().unwrap();
        t2.join().unwrap();
    }
}
