use alloc::boxed::Box;
use alloc::vec::Vec;
use core::alloc::Allocator;
use core::marker::PhantomData;
use core::ops::DerefMut;

pub struct DmaGuard<T,C> {
    inner: core::mem::ManuallyDrop<C>,

    _phantom: PhantomData<T>,
    lock: Option<alloc::sync::Arc<core::sync::atomic::AtomicBool>>,
}

impl<T,C> Drop for DmaGuard<T,C> {
    fn drop(&mut self) {
        if !self.lock.take().is_some_and(|v| v.load(atomic::Ordering::Acquire)) {
            // SAFETY: Well, we definitely aren't using this anymore
            unsafe { core::mem::ManuallyDrop::drop(&mut self.inner) }
        }
    }
}

impl<T,C> DmaGuard<T,C> {
    pub fn unwrap(mut self) -> C {
        if self.lock.take().is_some_and(|v| v.load(atomic::Ordering::Acquire)) {
            panic!("DmaGuard::unwrap(): Called while data was locked");
        }
        // SAFETY: `self` is forgotten immediately after this
        let t = unsafe { core::mem::ManuallyDrop::take(&mut self.inner) };
        core::mem::forget(self);
        t
    }
}

unsafe impl<T: 'static, A: Allocator + 'static> DmaTarget for DmaGuard<T,Vec<T, A>> {
    fn as_mut(&mut self) -> *mut [u8] {
        let ptr = self.inner.as_mut_ptr();
        let elem_size = size_of::<T>();
        unsafe { core::slice::from_raw_parts_mut(ptr as *mut _, elem_size * self.inner.len()) }
    }
}

unsafe impl<T: 'static, A: Allocator + 'static> DmaTarget for DmaGuard<T,Box<T,A>> {
    fn as_mut(&mut self) -> *mut [u8] {
        let ptr = self.inner.as_mut() as *mut T as *mut u8;
        let elem_size = size_of::<T>();
        unsafe { core::slice::from_raw_parts_mut(ptr, elem_size) }
    }
}

impl<'a, T> DmaGuard<T, &'a mut T> {
    /// Constructs self from a raw pointer.
    /// This can be used to allow stack allocated buffers or buffers that are otherwise unsafe to use.
    ///
    /// # Safety
    ///
    /// The caller must ensure that DMA operations are completed before accessing the owner of `data`.
    pub unsafe fn from_raw(data: &'a mut T) -> DmaGuard<T, &'a mut T> {
        Self {
            inner: core::mem::ManuallyDrop::new(data),
            _phantom: Default::default(),
            lock: None,
        }
    }
}

unsafe impl<'a, T> DmaTarget for DmaGuard<T, &'a mut T> {

    fn as_mut(&mut self) -> *mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.inner.deref_mut() as *mut _ as *mut u8, size_of_val(&*self.inner)) }
    }
}

unsafe impl<T,C> DmaClaimable for DmaGuard<T, C> where Self: DmaTarget {
    fn claim<'a,'b>(&'a mut self) -> Option<Box<dyn DmaTarget + 'b>> {

        // Lazily constructed, because this may not actually be used.
        if let Some(lock) = self.lock.as_ref() {
            lock.compare_exchange(false,true, atomic::Ordering::Acquire, atomic::Ordering::Relaxed).ok()?;
        } else {
            self.lock = Some(alloc::sync::Arc::new(core::sync::atomic::AtomicBool::new(true)));
        }

        Some(Box::new(BorrowedDmaGuard {
            data: self.as_mut(),
            lock: self.lock.as_ref().unwrap().clone(), // Guaranteed to be some
            _phantom: PhantomData,
        }))
    }

    fn query_owned(&self) -> bool {
        self.lock.as_ref().is_some_and(|v| v.load(atomic::Ordering::Acquire))
    }
}

struct BorrowedDmaGuard<'a> {
    data: *mut [u8],
    lock: alloc::sync::Arc<core::sync::atomic::AtomicBool>,
    _phantom: PhantomData<&'a mut [u8]>,
}

unsafe impl DmaTarget for BorrowedDmaGuard<'_> {
    fn as_mut(&mut self) -> *mut [u8] {
        self.data
    }
}

impl Drop for BorrowedDmaGuard<'_> {
    fn drop(&mut self) {
        self.lock.store(false, atomic::Ordering::Release);
    }
}

impl<T, C: DmaPointer<T>> From<C> for DmaGuard<T, C> {
    fn from(inner: C) -> Self {
        DmaGuard { inner: core::mem::ManuallyDrop::new(inner), _phantom: PhantomData, lock: None }
    }
}


mod sealed {
    pub trait Sealed {}
}

trait DmaPointer<T>: sealed::Sealed {}


impl<T,A:Allocator> sealed::Sealed for Vec<T,A> {}
impl<T,A:Allocator> DmaPointer<T> for Vec<T,A> {}

impl<T,A:Allocator> sealed::Sealed for Box<T,A> {}
impl<T,A:Allocator> DmaPointer<T> for Box<T,A> {}

pub struct PhysicalRegionDescriber<'a> {
    data: *mut [u8],
    next: usize,

    phantom: PhantomData<&'a ()>,
}

impl PhysicalRegionDescriber<'_> {
    fn next_chunk(&mut self, index: usize) -> Option<u64> {
        // SAFETY: I think this is unsound
        let data = unsafe { &*self.data };
        crate::mem::mem_map::translate_ptr(data.get(index)?)
    }
}

impl Iterator for PhysicalRegionDescriber<'_> {
    type Item = PhysicalRegionDescription;

    fn next(&mut self) -> Option<Self::Item> {
        let base = self.next_chunk(self.next)?;
        // SAFETY: I think this is unsound
        let data = unsafe { & *self.data };

        let mut diff = super::PAGE_SIZE - (base as usize & (super::PAGE_SIZE-1)).min(data.len()); // diff between next index and base

        loop {
            match self.next_chunk(diff + self.next) {
                // Ok(_) ensures that this is offset is valid
                // match guard checks that addr is contiguous
                Some(addr) if addr - base == diff as u64 => {
                    diff += super::PAGE_SIZE;
                    diff = diff.min(data.len()); // make sure we dont overflow
                }
                // When either of the above checks fail we have reached the end of the region
                _ => break,
            }
            if diff == data.len() {
                break;
            }
        }

        self.next += diff;

        Some(PhysicalRegionDescription {
            addr: base,
            size: diff,
        })
    }
}

/// Describes a contiguous region of physical memory.
///
/// This is used for building Scatter-Gather tables.
#[derive(Debug)]
pub struct PhysicalRegionDescription {
    /// Starting physical address of the region.
    pub addr: u64,
    /// Length in bytes.
    pub size: usize,
}

/// A type that implements DmaTarget can be used for DMA operations.
///
/// `async` DMA operations *must* use an implementor of DmaTarget to safely operate. The argument *must* be
/// taken by value and not by reference, the future should return ownership of the DmaTarget when it completes.
/// See [Embedonomicon](https://docs.rust-embedded.org/embedonomicon/dma.html) for details.
///
/// # Safety
///
/// An implementor must ensure that the DMA region returned by [Self::as_mut] is owned by `self` is treated as volatile.
pub unsafe trait DmaTarget {
    fn as_mut(&mut self) -> *mut [u8];

    /// Returns a Physical region describer.
    ///
    /// This takes `self` as `&mut` but does not actually mutate `self` this is to prevent all
    /// accesses to `self` while the PRD is alive.
    fn prd(&mut self) -> PhysicalRegionDescriber {
        PhysicalRegionDescriber {
            data: self.as_mut(),
            next: 0,
            phantom: Default::default(),
        }
    }
}

/// Claimable is intended to solve a problem in [DmaGuard] where a user may want to wrap a
/// `Vec<u64>` read a [crate::fs::file::Read] into it and unwrap back into a `Vec<u64>`.
/// This may only be done by downcasting through [core::any::Any], this is inconvenient,
/// because it requires declaring a type, erasing the type data then trying to re-determine our type data.
///
/// The intention of this trait is to provide a RAII guard similar to a mutex.
/// `self` may not drop or access its wrapped buffer until the return value of [DmaClaimable::claim] is dropped.
///
/// If `self` is dropped while the data is borrowed then the data must be leaked.
pub unsafe trait DmaClaimable: DmaTarget {

    /// This fn returns a [DmaTarget] using the same target buffer as `self`.
    ///
    /// When this fn completes successfully then the returned type (`'b`) "owns" the target data of self (`'a`),
    /// when the returned `'b` is dropped it must return ownership of the target buffer to `'a`.
    /// If `'a` is dropped before `'b` then `'a` must not drop the inner data.
    ///
    /// The value of [Self::query_owned] indicates whether this function will succeed.
    ///
    /// This is intended for use with futures where the target buffer must use dynamic dispatch.
    /// This allows a borrow to occur while passing ownership of the target data without erasing the
    /// type of `self` thus skipping a downcast back into `Self`
    ///
    /// The lifetimes should be treated as `fn('a) -> 'a` by the caller but `fn('a) -> 'b` must be safe.
    fn claim<'a, 'b>(&'a mut self) -> Option<Box<dyn DmaTarget + 'b>>;

    /// Returns `true` if self currently owned the buffer.
    fn query_owned(&self) -> bool;
}


#[test_case]
#[cfg(test)]
fn test_dmaguard() {
    use crate::{alloc_interface,mem};
    let mut b = alloc::vec::Vec::new_in(alloc_interface::DmaAlloc::new(mem::MemRegion::Mem64,4096));
    b.resize(0x4000, 0u8);
    let mut g = mem::dma::DmaGuard::from(b);

    let g_prd = g.prd();
    let mut prd_cmp = Vec::new();
    for i in g_prd {
        prd_cmp.push(alloc::format!("{:x?}", i));
    }

    let mut t = g.claim().unwrap();
    assert!(g.claim().is_none());
    for (p,c) in t.prd().zip(prd_cmp) {
        assert_eq!(c,alloc::format!("{:x?}",p))
    }

    drop(t);
    g.unwrap();

    let mut b = alloc::vec::Vec::new_in(alloc_interface::DmaAlloc::new(mem::MemRegion::Mem64,4096));
    b.resize(0x4000, 0u8);
    let mut g = mem::dma::DmaGuard::from(b);
    let t = g.claim();
    let helper = g.as_mut();

    drop(g);

    x86_64::instructions::nop();


}