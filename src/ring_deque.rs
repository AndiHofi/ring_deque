use crate::ring_deque::Error::BufferFull;
use nix::libc::off_t;
use nix::sys::memfd::{memfd_create, MemFdCreateFlag};
use nix::sys::mman::{MapFlags, ProtFlags};
use nix::unistd::ftruncate;
use std::borrow::Borrow;
use std::ffi::{c_void, CString};
use std::mem::{align_of, size_of};
use std::num::NonZeroUsize;
use std::ops::{Index, Range, RangeFrom, RangeFull, RangeTo};
use std::ptr;
use std::ptr::NonNull;

#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub enum Error {
    BufferFull,
}

/// Always contiguous double-ended queue
///
/// ```
/// # use std::mem::size_of;
/// # use ring_deque::RingDeque;
/// # fn main() {
/// // always allocates full memory pages - typically 4kiB
/// let mut deque: RingDeque<u64> = RingDeque::allocate(100);
///
/// assert_eq!(deque.capacity(), 4096 / size_of::<u64>());
///
/// // fill the buffer
/// deque.push_back(5).unwrap();
/// deque.push_front(2).unwrap(); // VecDeque would wrap around with this
/// deque.push_front(1).unwrap();
/// for v in 8..12 {
///     deque.push_back(v).unwrap();
/// }
///
/// // remove elements
/// assert_eq!(deque.pop_back(), Some(11));
/// assert_eq!(deque.pop_front(), Some(1));
///
/// // at all times it is possible to have a single slice containing
/// // all values of the deque. Therefore, it is contiguous.
/// assert_eq!(&deque[..], &[2, 5, 8, 9, 10]);
/// # }
/// ```

pub struct RingDeque<T: Sized> {
    base_addr: NonNull<T>,
    end_addr: NonNull<T>,
    virt_size: NonZeroUsize,
    capacity: NonZeroUsize,
    start: NonNull<T>,
    end: NonNull<T>,
}

impl<T> RingDeque<T>
where
    T: Sized,
{
    pub fn allocate(count: usize) -> RingDeque<T> {
        let alloc = unsafe { allocate_ring_buffer::<T>(count) };

        let base_addr = NonNull::new(alloc.base).unwrap();
        let end_addr = unsafe { alloc.base.offset(alloc.virt_capacity.get() as isize) };
        RingDeque {
            base_addr,
            end_addr: NonNull::new(end_addr).unwrap(),
            virt_size: alloc.virt_capacity,
            capacity: alloc.capacity,
            start: base_addr,
            end: base_addr,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity.get()
    }

    pub fn virtual_capacity(&self) -> usize {
        self.virt_size.get()
    }

    pub fn len(&self) -> usize {
        let count = unsafe { self.end.as_ptr().offset_from(self.start.as_ptr()) };
        debug_assert!(count >= 0);
        count as usize
    }

    #[inline]
    pub fn as_slice(&self) -> &[T] {
        unsafe {
            std::slice::from_raw_parts(
                self.start.as_ptr(),
                self.end.as_ptr().offset_from(self.start.as_ptr()) as usize,
            )
        }
    }

    pub fn push_front(&mut self, v: T) -> Result<(), Error> {
        if self.len() == self.capacity() {
            return Err(BufferFull);
        }
        unsafe {
            self.cond_move_address_back();
            let new_start = self.start.as_ptr().offset(-1);
            new_start.write(v);
            self.start = NonNull::new(new_start).unwrap();
            Ok(())
        }
    }

    pub fn push_back(&mut self, v: T) -> Result<(), Error> {
        if self.len() == self.capacity() {
            return Err(BufferFull);
        }
        let next_end = self.new_end(self.end);
        unsafe { ptr::write(self.end.as_ptr(), v) }
        self.end = next_end;
        Ok(())
    }

    pub fn push_back_rotate(&mut self, v: T) {
        if self.len() == self.capacity() {
            drop(self.pop_front().unwrap())
        }
        self.push_back(v).unwrap()
    }

    pub fn pop_front(&mut self) -> Option<T> {
        if self.start == self.end {
            None
        } else {
            unsafe {
                let value = ptr::read(self.start.as_ptr());
                let next_start = self.start.as_ptr().offset(1);
                self.start = NonNull::new(next_start).unwrap();
                self.cond_move_address_front();

                Some(value)
            }
        }
    }

    pub fn pop(&mut self) -> Option<T> {
        self.pop_back()
    }

    pub fn pop_back(&mut self) -> Option<T> {
        if self.end == self.start {
            None
        } else {
            unsafe {
                self.end = NonNull::new(self.end.as_ptr().offset(-1)).unwrap();
                let value = self.end.as_ptr().read();
                Some(value)
            }
        }
    }

    pub fn extend_back_from_slice(&mut self, slice: &[T]) -> Result<(), Error>
    where
        T: Copy,
    {
        if self.len() + slice.len() > self.capacity() {
            return Err(BufferFull);
        }

        for v in slice {
            self.push_back(*v)?
        }

        Ok(())
    }

    pub fn extend_front_from_slice(&mut self, slice: &[T]) -> Result<(), Error>
    where
        T: Copy,
    {
        if self.len() + slice.len() > self.capacity() {
            return Err(BufferFull);
        }

        let mut slice_iter = slice.into_iter();
        while let Some(v) = slice_iter.next_back() {
            self.push_front(*v)?;
        }

        Ok(())
    }

    pub fn try_extend_back(&mut self, data: impl IntoIterator<Item = T>) -> Result<(), Error> {
        for e in data.into_iter() {
            self.push_back(e)?;
        }
        Ok(())
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Reference to the first element
    ///
    /// ```
    /// # use ring_deque::RingDeque;
    /// # fn test() {
    /// let mut deque = RingDeque::allocate(5);
    /// deque.extend_front_from_slice(&[1,2,3]).unwrap();
    ///
    /// assert_eq!(deque.front(), Some(&1));
    /// deque.pop_front();
    /// assert_eq!(deque.front(), Some(&2));
    /// # }
    /// ```
    pub fn front(&self) -> Option<&T> {
        if self.is_empty() {
            None
        } else {
            Some(unsafe { self.start.as_ref() })
        }
    }

    pub fn back(&self) -> Option<&T> {
        if self.is_empty() {
            None
        } else {
            unsafe { self.end.as_ptr().offset(-1).as_ref() }
        }
    }

    fn new_end(&self, addr: NonNull<T>) -> NonNull<T> {
        unsafe {
            let addr: *mut T = addr.as_ptr().offset(1);
            if addr > self.end_addr.as_ptr() {
                panic!("Reached end of virtual memory region")
            } else {
                NonNull::new(addr).unwrap()
            }
        }
    }

    unsafe fn cond_move_address_back(&mut self) {
        if self.start == self.base_addr {
            // move to back end of the virtual memory region
            let offset = self.capacity.get() as isize;
            self.start = NonNull::new(self.start.as_ptr().offset(offset)).unwrap();
            self.end = NonNull::new(self.end.as_ptr().offset(offset)).unwrap();
        }
    }

    unsafe fn cond_move_address_front(&mut self) {
        let offset = self.capacity.get() as isize;
        if self.start.as_ptr() > self.base_addr.as_ptr().offset(offset) {
            // move to the front end of the virtual memory region
            // only need to move start and end back by the capacity
            self.start = NonNull::new(self.start.as_ptr().offset(-offset)).unwrap();
            self.end = NonNull::new(self.end.as_ptr().offset(-offset)).unwrap();
            assert!(self.start <= self.end);
        }
    }
}

impl<T> Borrow<[T]> for RingDeque<T>
where
    T: Eq + Ord,
{
    fn borrow(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T> AsRef<[T]> for RingDeque<T> {
    #[inline]
    fn as_ref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T> AsMut<[T]> for RingDeque<T> {
    #[inline]
    fn as_mut(&mut self) -> &mut [T] {
        unsafe { std::slice::from_raw_parts_mut(self.start.as_ptr(), self.len()) }
    }
}

impl<T> Index<RangeFull> for RingDeque<T> {
    type Output = [T];
    #[inline]
    fn index(&self, _index: RangeFull) -> &Self::Output {
        self.as_slice()
    }
}

impl<T> Index<usize> for RingDeque<T> {
    type Output = T;
    fn index(&self, index: usize) -> &Self::Output {
        let all = self.as_slice();
        &all[index]
    }
}

impl<T> Index<Range<usize>> for RingDeque<T> {
    type Output = [T];
    fn index(&self, index: Range<usize>) -> &Self::Output {
        let all = self.as_slice();
        &all[index]
    }
}

impl<T> Index<RangeFrom<usize>> for RingDeque<T> {
    type Output = [T];
    fn index(&self, index: RangeFrom<usize>) -> &Self::Output {
        let all = self.as_slice();
        &all[index]
    }
}

impl<T> Index<RangeTo<usize>> for RingDeque<T> {
    type Output = [T];
    fn index(&self, index: RangeTo<usize>) -> &Self::Output {
        let all = self.as_slice();
        &all[index]
    }
}

impl<T> Drop for RingDeque<T> {
    fn drop(&mut self) {
        unsafe {
            let content = ptr::slice_from_raw_parts_mut(
                self.start.as_ptr(),
                self.end.as_ptr().offset_from(self.start.as_ptr()) as usize,
            );
            ptr::drop_in_place(content);
            let to_deallocate = RingAlloc {
                base: self.base_addr.as_ptr(),
                capacity: self.capacity,
                virt_capacity: NonZeroUsize::new(self.capacity.get() * 2).unwrap(),
            };
            deallocate_ring_buffer(to_deallocate);
            // make self invalid, probably not needed
            self.base_addr = NonNull::dangling();
            self.end_addr = NonNull::dangling();
            self.start = NonNull::dangling();
            self.end = NonNull::dangling();
        }
    }
}

struct RingAlloc<T> {
    pub base: *mut T,
    pub capacity: NonZeroUsize,
    pub virt_capacity: NonZeroUsize,
}

unsafe fn deallocate_ring_buffer<T>(alloc: RingAlloc<T>) {
    let byte_ptr = NonNull::new(alloc.base.cast::<c_void>()).unwrap();
    let byte_capacity = alloc.capacity.get() * size_of::<T>();
    nix::sys::mman::munmap(byte_ptr.offset(byte_capacity as isize), byte_capacity).unwrap();
    nix::sys::mman::munmap(byte_ptr, byte_capacity).unwrap();
}

unsafe fn allocate_ring_buffer<T>(min_capacity: usize) -> RingAlloc<T> {
    let byte_size = size_of::<T>() * min_capacity;
    unsafe {
        let pagesize = get_page_size();
        let page_count = (byte_size + pagesize - 1) / pagesize;
        let byte_count = page_count * pagesize;
        assert_eq!(byte_count % align_of::<T>(), 0, "Unaligned buffer");
        assert_eq!(byte_count % size_of::<T>(), 0, "Not a multiple of size");
        let capacity = byte_count / size_of::<T>();
        let name = CString::new("mem").unwrap();
        let fd = memfd_create(&name, MemFdCreateFlag::MFD_CLOEXEC).unwrap();
        ftruncate(&fd, byte_count as off_t).unwrap();

        // find contiguous virtual memory space
        let base = nix::sys::mman::mmap_anonymous(
            None,
            NonZeroUsize::new(2 * byte_count).unwrap(),
            ProtFlags::PROT_NONE,
            MapFlags::MAP_ANONYMOUS | MapFlags::MAP_PRIVATE,
        )
        .unwrap();

        let byte_count = NonZeroUsize::new(byte_count).unwrap();
        nix::sys::mman::mmap(
            Some(base.addr()),
            byte_count,
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_FIXED | MapFlags::MAP_SHARED,
            &fd,
            0,
        )
        .unwrap();

        nix::sys::mman::mmap(
            base.addr().checked_add(byte_count.get()),
            byte_count,
            ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
            MapFlags::MAP_FIXED | MapFlags::MAP_SHARED,
            &fd,
            0,
        )
        .unwrap();
        drop(fd);
        RingAlloc {
            base: base.as_ptr().cast(),
            capacity: NonZeroUsize::new(capacity).unwrap(),
            virt_capacity: NonZeroUsize::new(capacity * 2).unwrap(),
        }
    }
}

fn get_page_size() -> usize {
    unsafe { nix::libc::sysconf(nix::libc::_SC_PAGESIZE) as usize }
}

#[cfg(test)]
mod test {
    use crate::ring_deque::Error::BufferFull;
    use crate::ring_deque::{get_page_size, RingDeque};
    use rand::Rng;
    use std::mem::size_of;

    #[test]
    fn creates_empty_ring_buffer() {
        type Payload = u64;
        let mut ring = RingDeque::<Payload>::allocate(1);
        assert_eq!(ring.capacity(), get_page_size() / size_of::<Payload>());
        assert_eq!(ring.len(), 0);
        assert!(ring.virtual_capacity() >= ring.capacity() * 2);
        assert!(ring.as_slice().is_empty());
        assert_eq!(ring.pop_front(), None);
        assert_eq!(ring.pop_back(), None);
        drop(ring)
    }

    #[test]
    fn keeps_element_order_push_back() {
        let mut ring = RingDeque::allocate(2000);

        for i in 0..ring.capacity() {
            ring.push_back(i as isize).unwrap();
        }
        let mut cur_max = ring.capacity() as isize;

        assert_sorted(&ring[..]);
        for _ in 0..100 {
            for _ in 0..1337 {
                ring.pop_front().unwrap();
                if ring.len() > 100 {
                    ring.pop_front().unwrap();
                }
                ring.push_back(cur_max).unwrap();
                cur_max += 1;
            }
            assert_sorted(&ring[..]);
        }
    }

    #[test]
    fn keeps_element_order_push_front() {
        let mut ring = RingDeque::allocate(2000);

        let mut cur_min = 0;
        for _ in 0..ring.capacity() {
            ring.push_front(cur_min).unwrap();
            cur_min -= 1;
        }

        for _ in 0..100 {
            assert_sorted(&ring[..]);
            for _ in 0..1337 {
                ring.pop_back().unwrap();
                ring.push_front(cur_min).unwrap();
                cur_min -= 1;
            }
            assert_sorted(&ring[..]);
        }
    }

    fn assert_sorted(ring: &[isize]) {
        let mut iter = ring.iter();
        let mut last = *iter.next().unwrap();
        while let Some(cur) = iter.next() {
            assert!(*cur > last);
            last = *cur;
        }
    }

    #[test]
    fn full_ring_buffer() {
        type Payload = u64;
        let mut ring = RingDeque::<Payload>::allocate(1000);

        let mut rng = rand::thread_rng();
        let mut checksum = 0u128;
        for _ in 0..ring.capacity() {
            let value = rng.gen();
            ring.push_back(value).unwrap();
            checksum += value as u128;
        }

        assert_eq!(ring.push_back(0), Err(BufferFull));
        assert_eq!(ring.push_front(214), Err(BufferFull));
        assert_eq!(ring.len(), ring.capacity());

        validate_checksum(&ring[..], checksum);

        // constantly rotate backwards
        // by taking a value from the back and pushing another value to the front
        for _ in 0..ring.capacity() * 2 {
            let val = ring.pop_back().unwrap();
            assert_eq!(ring.len(), ring.capacity() - 1);
            checksum -= val as u128;
            validate_checksum(&ring[..], checksum);
            let val = rng.gen();
            ring.push_front(val).unwrap();
            assert_eq!(ring.len(), ring.capacity());
            checksum += val as u128;
            validate_checksum(ring.as_ref(), checksum);
        }

        // constantly rotate forwards
        // by taking a value from the font and pushing another value to the back
        for _ in 0..ring.capacity() * 2 {
            let val = ring.pop_front().unwrap();
            checksum -= val as u128;
            assert_eq!(ring.len(), ring.capacity() - 1);
            validate_checksum(ring.as_ref(), checksum);
            let val = rng.gen();
            ring.push_back(val).unwrap();
            assert_eq!(ring.len(), ring.capacity());
            checksum += val as u128;
            validate_checksum(ring.as_ref(), checksum);
        }
    }

    fn validate_checksum(ring: &[u64], checksum: u128) {
        let sum: u128 = ring.into_iter().map(|e| *e as u128).sum();
        assert_eq!(sum, checksum);
    }
}
