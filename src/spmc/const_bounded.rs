//! This module provides a single-producer multi-consumer queue.
//!
//! It is implemented as a const bounded ring buffer.
//! It is optimized for the work-stealing model.
#![allow(clippy::cast_possible_truncation, reason = "LongNumber should be synonymous to usize")]
use crate::hints::unlikely;
use crate::light_arc::LightArc;
use crate::number_types::{
    CachePaddedLongAtomic, LongAtomic, LongNumber, NotCachePaddedLongAtomic,
};
use crate::spmc::{Consumer, Producer};
use crate::sync_batch_receiver::SyncBatchReceiver;
use std::marker::PhantomData;
use std::mem::{MaybeUninit, needs_drop};
use std::ops::Deref;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::{ptr, slice};

// Don't care about ABA because we can count that 16-bit and 32-bit processors never
// insert + read (2 ^ 16) - 1 or (2 ^ 32) - 1 values while some consumer are preempted.
// For 32-bit:
// If we guess, it always put and get 10 values by time and do it in
// 20 nanoseconds (it becomes slower by adding new threads), then the thread needs to be preempted
// for 8.5 seconds while others thread only work with this queue.
// For 16-bit:
// We guess, it never has so much concurrency.
// For 64-bit it is unrealistic to have the ABA problem.

// Reads from the head, writes to the tail.

/// The single-producer, multi-consumer ring-based _const bounded_ queue.
///
/// It is safe to use when and only when only one thread is writing to the queue at the same time.
///
/// You can call `producer_` methods for the producer and `consumer_` methods for the consumers.
///
/// It accepts the atomic wrapper as a generic parameter.
/// It allows using cache-padded atomics or not.
/// You should create types aliases not to write this large type name.
///
/// # Using directly the [`SPMCBoundedQueue`] vs. using [`new_bounded`] or [`new_cache_padded_bounded`].
///
/// Functions [`new_bounded`] and [`new_cache_padded_bounded`] allocate the
/// [`SPMCUnboundedQueue`] on the heap in [`LightArc`] and provide separate producer and consumer.
/// It hurts the performance if you don't need to allocate the queue separately, but improve
/// the readability when you need to separate producer and consumer logic and share them.
///
/// It doesn't implement the [`Producer`] and [`Consumer`] traits because all producer methods
/// are unsafe (can be called only by one thread).
#[repr(C)]
pub struct SPMCBoundedQueue<
    T,
    const CAPACITY: usize,
    AtomicWrapper: Deref<Target = LongAtomic> + Default = NotCachePaddedLongAtomic,
> {
    tail: AtomicWrapper,
    head: AtomicWrapper,
    buffer: *mut [MaybeUninit<T>; CAPACITY],
}

impl<T, const CAPACITY: usize, AtomicWrapper: Deref<Target = LongAtomic> + Default>
    SPMCBoundedQueue<T, CAPACITY, AtomicWrapper>
{
    /// Indicates how many elements we are taking from the local queue.
    ///
    /// This is one less than the number of values pushed to the global
    /// queue (or any other `SyncBatchReceiver`) as we are also inserting the `value` argument.
    const NUM_VALUES_TAKEN: LongNumber = CAPACITY as LongNumber / 2;

    /// Creates a new [`SPMCBoundedQueue`].
    pub fn new() -> Self {
        debug_assert!(size_of::<MaybeUninit<T>>() == size_of::<T>()); // Assume that we can just cast it

        Self {
            buffer: Box::into_raw(Box::new([const { MaybeUninit::uninit() }; CAPACITY])),
            tail: AtomicWrapper::default(),
            head: AtomicWrapper::default(),
        }
    }

    /// Returns the capacity of the queue.
    #[inline]
    pub fn capacity(&self) -> usize {
        CAPACITY
    }

    /// Returns a pointer to the buffer.
    fn buffer_thin_ptr(&self) -> *const MaybeUninit<T> {
        unsafe { &*self.buffer }.as_ptr()
    }

    /// Returns a mutable pointer to the buffer.
    fn buffer_mut_thin_ptr(&self) -> *mut MaybeUninit<T> {
        unsafe { &mut *self.buffer }.as_mut_ptr()
    }

    /// Returns the number of elements in the queue.
    #[inline]
    fn len(head: LongNumber, tail: LongNumber) -> usize {
        tail.wrapping_sub(head) as usize
    }
}

// Producer
impl<T, const CAPACITY: usize, AtomicWrapper: Deref<Target = LongAtomic> + Default>
    SPMCBoundedQueue<T, CAPACITY, AtomicWrapper>
{
    /// Pushes a slice into the queue. Returns a new tail (not index).
    fn copy_slice(buffer_ptr: *mut T, start_tail: LongNumber, slice: &[T]) -> LongNumber {
        let tail_idx = start_tail as usize % CAPACITY;

        if tail_idx + slice.len() <= CAPACITY {
            unsafe {
                ptr::copy_nonoverlapping(slice.as_ptr(), buffer_ptr.add(tail_idx), slice.len());
            };
        } else {
            let right = CAPACITY - tail_idx;

            unsafe {
                ptr::copy_nonoverlapping(slice.as_ptr(), buffer_ptr.add(tail_idx), right);
                ptr::copy_nonoverlapping(
                    slice.as_ptr().add(right),
                    buffer_ptr,
                    slice.len() - right,
                );
            }
        }

        start_tail.wrapping_add(slice.len() as LongNumber)
    }

    /// Return the number of elements in the queue.
    ///
    /// # Safety
    ///
    /// The called should be the only producer.
    #[inline]
    pub unsafe fn producer_len(&self) -> usize {
        let head = self.head.load(Relaxed);
        let tail = unsafe { self.tail.unsync_load() }; // only producer can change tail

        Self::len(head, tail)
    }

    /// Pops a value from the queue.
    /// Returns `None` if the queue is empty.
    ///
    /// # Safety
    ///
    /// The called should be the only producer.
    #[inline]
    pub unsafe fn producer_pop(&self) -> Option<T> {
        let mut head = self.head.load(Acquire);
        let tail = unsafe { self.tail.unsync_load() }; // only producer can change tail

        loop {
            if unlikely(head == tail) {
                return None;
            }

            match self
                .head
                .compare_exchange_weak(head, head.wrapping_add(1), Release, Acquire)
            {
                Ok(_) => {
                    // We are the only producer,
                    // so we can don't worry about someone overwriting the value before we read it
                    return Some(unsafe {
                        self.buffer_thin_ptr()
                            .add(head as usize % CAPACITY)
                            .read()
                            .assume_init()
                    });
                }
                Err(new_head) => {
                    head = new_head;
                }
            }
        }
    }

    /// Pops many values from the queue.
    /// Returns the number of values popped.
    ///
    /// # Safety
    ///
    /// The called should be the only producer.
    #[inline]
    pub unsafe fn producer_pop_many(&self, dst: &mut [MaybeUninit<T>]) -> usize {
        let mut head = self.head.load(Acquire);
        let tail = unsafe { self.tail.unsync_load() }; // only producer can change tail

        loop {
            let available = Self::len(head, tail);
            let n = dst.len().min(available);

            if n == 0 {
                return 0;
            }

            debug_assert!(n <= CAPACITY, "Bug occurred, please report it.");

            match self.head.compare_exchange_weak(
                head,
                head.wrapping_add(n as LongNumber),
                Release,
                Acquire,
            ) {
                Ok(_) => {
                    // We are the only producer,
                    // so we can don't worry about someone overwriting the value before we read it.

                    let dst_ptr = dst.as_mut_ptr();
                    let head_idx = head as usize % CAPACITY;
                    let right = CAPACITY - head_idx;

                    if n <= right {
                        // No wraparound, copy in one shot
                        unsafe {
                            ptr::copy_nonoverlapping(
                                self.buffer_thin_ptr().add(head_idx),
                                dst_ptr,
                                n,
                            );
                        }
                    } else {
                        unsafe {
                            // Wraparound: copy right half then left half
                            ptr::copy_nonoverlapping(
                                self.buffer_thin_ptr().add(head_idx),
                                dst_ptr,
                                right,
                            );
                            ptr::copy_nonoverlapping(
                                self.buffer_thin_ptr(),
                                dst_ptr.add(right),
                                n - right,
                            );
                        }
                    }

                    return n;
                }
                Err(new_head) => {
                    head = new_head;
                }
            }
        }
    }

    /// Pushes a value to the queue.
    ///
    /// # Safety
    ///
    /// The called should be the only producer and the queue should not be full.
    #[inline(always)]
    pub unsafe fn push_unchecked(&self, value: T, tail: LongNumber) {
        unsafe {
            self.buffer_mut_thin_ptr()
                .add(tail as usize % CAPACITY)
                .write(MaybeUninit::new(value));
        }

        self.tail.store(tail.wrapping_add(1), Release);
    }

    /// Likely moves a half of the queue and one value to the [`SyncBatchReceiver`].
    #[inline(never)]
    #[cold]
    fn handle_overflow_one<SBR: SyncBatchReceiver<T>>(
        &self,
        tail: LongNumber,
        mut head: LongNumber,
        sbr: &SBR,
        value: T,
    ) {
        debug_assert!(tail == head.wrapping_add(CAPACITY as LongNumber) && tail > head);

        loop {
            let head_idx = head as usize % CAPACITY;
            let values_slice = unsafe { &*(self.buffer.cast::<[T; CAPACITY]>()) };

            let (right, left): (&[T], &[T]) = if head_idx < Self::NUM_VALUES_TAKEN as usize {
                // we can return only the right half of the queue
                (
                    &values_slice[head_idx..head_idx + Self::NUM_VALUES_TAKEN as usize],
                    &[],
                )
            } else {
                let left_part_len = head_idx - Self::NUM_VALUES_TAKEN as usize;

                (&values_slice[head_idx..], &values_slice[..left_part_len])
            };

            // We haven't read the value yet, so we can use `compare_exchange_weak`.
            //If it fails, we calculate two slices and try again, it is not a performance issue.
            let res = self.head.compare_exchange_weak(
                head,
                head.wrapping_add(Self::NUM_VALUES_TAKEN),
                Release,
                Acquire,
            );

            match res {
                Ok(_) => {}
                Err(new_head) => {
                    head = new_head;

                    if Self::len(head, tail) < Self::NUM_VALUES_TAKEN as usize {
                        // Another thread concurrently
                        // stole from the queue.
                        // Because we are the one producer,
                        // we can just insert the value (it can't become full before we return).

                        unsafe { self.push_unchecked(value, tail) };

                        return;
                    }

                    continue;
                }
            }

            sbr.push_many_and_one(left, right, value);

            return;
        }
    }

    /// Likely moves a half of the queue and many values to the [`SyncBatchReceiver`].
    #[inline(never)]
    #[cold]
    fn handle_overflow_many<SBR: SyncBatchReceiver<T>>(
        &self,
        tail: LongNumber,
        mut head: LongNumber,
        sbr: &SBR,
        slice: &[T],
    ) {
        debug_assert!(tail == head.wrapping_add(CAPACITY as LongNumber) && tail > head);

        loop {
            let head_idx = head as usize % CAPACITY;
            let values_slice = unsafe { &*(self.buffer.cast::<[T; CAPACITY]>()) };

            let (right, left): (&[T], &[T]) = if head_idx < Self::NUM_VALUES_TAKEN as usize {
                // we can return only the right half of the queue
                (
                    &values_slice[head_idx..head_idx + Self::NUM_VALUES_TAKEN as usize],
                    &[],
                )
            } else {
                let left_part_len = head_idx - Self::NUM_VALUES_TAKEN as usize;

                (&values_slice[head_idx..], &values_slice[..left_part_len])
            };

            // We haven't read the value yet, so we can use `compare_exchange_weak`.
            //If it fails, we calculate two slices and try again, it is not a performance issue.
            let res = self.head.compare_exchange_weak(
                head,
                head.wrapping_add(Self::NUM_VALUES_TAKEN),
                Release,
                Acquire,
            );

            match res {
                Ok(_) => {}
                Err(new_head) => {
                    head = new_head;

                    let len = Self::len(head, tail);

                    if (len < Self::NUM_VALUES_TAKEN as usize) && len + slice.len() <= CAPACITY {
                        // Another thread concurrently
                        // stole from the queue.
                        // Because we are the one producer,
                        // we can just insert the slice (it can't become full before we return).

                        let new_tail =
                            Self::copy_slice(self.buffer_mut_thin_ptr().cast(), tail, slice);
                        self.tail.store(new_tail, Release);

                        return;
                    }

                    continue;
                }
            }

            sbr.push_many_and_slice(left, right, slice);

            return;
        }
    }

    /// Pushes a value to the queue or to the [`SyncBatchReceiver`].
    ///
    /// # Safety
    ///
    /// The called should be the only producer.
    #[inline]
    pub unsafe fn producer_push<SBR: SyncBatchReceiver<T>>(
        &self,
        value: T,
        sync_batch_receiver: &SBR,
    ) {
        let head = self.head.load(Acquire);
        let tail = unsafe { self.tail.unsync_load() }; // only producer can change tail

        if unlikely(Self::len(head, tail) == CAPACITY) {
            self.handle_overflow_one(tail, head, sync_batch_receiver, value);

            return;
        }

        unsafe { self.push_unchecked(value, tail) };
    }

    /// Pushes a value to the queue or returns an error.
    ///
    /// # Safety
    ///
    /// The called should be the only producer.
    #[inline]
    pub unsafe fn producer_maybe_push(&self, value: T) -> Result<(), T> {
        let head = self.head.load(Acquire);
        let tail = unsafe { self.tail.unsync_load() }; // only producer can change tail

        if unlikely(Self::len(head, tail) == CAPACITY) {
            return Err(value);
        }

        debug_assert!(Self::len(head, tail) < CAPACITY);

        unsafe { self.push_unchecked(value, tail) };

        Ok(())
    }

    /// Pushes many values to the queue.
    /// It accepts two slices to allow using ring-based src.
    ///
    /// # Safety
    ///
    /// The called should be the only producer and the space is enough.
    #[inline]
    pub unsafe fn producer_push_many_unchecked(&self, first: &[T], last: &[T]) {
        if cfg!(debug_assertions) {
            let head = self.head.load(Acquire);
            let tail = unsafe { self.tail.unsync_load() }; // only producer can change tail

            debug_assert!(Self::len(head, tail) + first.len() + last.len() <= CAPACITY);
        }

        // It is SPMC, and it is expected that the capacity is enough.

        let mut tail = unsafe { self.tail.unsync_load() }; // only producer can change tail

        tail = Self::copy_slice(self.buffer_mut_thin_ptr().cast(), tail, first);
        tail = Self::copy_slice(self.buffer_mut_thin_ptr().cast(), tail, last);

        self.tail.store(tail, Release);
    }

    /// Pushes many values to the queue or to the [`SyncBatchReceiver`].
    ///
    /// # Safety
    ///
    /// The called should be the only producer.
    #[inline]
    pub unsafe fn producer_push_many<SBR: SyncBatchReceiver<T>>(
        &self,
        slice: &[T],
        sync_batch_receiver: &SBR,
    ) {
        let head = self.head.load(Acquire);
        let mut tail = unsafe { self.tail.unsync_load() }; // only producer can change tail

        if unlikely(Self::len(head, tail) + slice.len() > CAPACITY) {
            self.handle_overflow_many(tail, head, sync_batch_receiver, slice);

            return;
        }

        tail = Self::copy_slice(self.buffer_mut_thin_ptr().cast(), tail, slice);

        self.tail.store(tail, Release);
    }

    /// Pushes many values to the queue or returns an error.
    ///
    /// # Safety
    ///
    /// The called should be the only producer.
    #[inline]
    pub unsafe fn producer_maybe_push_many(&self, slice: &[T]) -> Result<(), ()> {
        let head = self.head.load(Acquire);
        let mut tail = unsafe { self.tail.unsync_load() }; // only producer can change tail

        if unlikely(Self::len(head, tail) + slice.len() > CAPACITY) {
            return Err(()); // full
        }

        debug_assert!(Self::len(head, tail) + slice.len() <= CAPACITY);

        tail = Self::copy_slice(self.buffer_mut_thin_ptr().cast(), tail, slice);

        self.tail.store(tail, Release);

        Ok(())
    }
}

// Consumers
impl<T, const CAPACITY: usize, AtomicWrapper: Deref<Target = LongAtomic> + Default>
    SPMCBoundedQueue<T, CAPACITY, AtomicWrapper>
{
    /// Returns the number of values in the queue.
    #[inline]
    pub fn consumer_len(&self) -> usize {
        loop {
            let head = self.head.load(Relaxed);
            let tail = self.tail.load(Relaxed);
            let len = Self::len(head, tail);

            if unlikely(len > CAPACITY) {
                // Inconsistent state (this thread has been preempted
                // after we have loaded `head`,
                // and before we have loaded `tail`),
                // try again
                continue;
            }

            return len;
        }
    }

    /// Pops many values from the queue to the `dst`.
    /// Returns the number of values popped.
    #[inline]
    pub fn consumer_pop_many(&self, dst: &mut [MaybeUninit<T>]) -> usize {
        let mut head = self.head.load(Acquire);
        let mut tail = self.tail.load(Acquire);

        'top: loop {
            let available = Self::len(head, tail);
            let n = dst.len().min(available);

            if n == 0 {
                return 0;
            }

            if unlikely(n > CAPACITY) {
                // Inconsistent state (this thread has been preempted
                // after we have loaded `head`,
                // and before we have loaded `tail`),
                // try again

                head = self.head.load(Acquire);

                continue;
            }

            let dst_ptr = dst.as_mut_ptr();
            let head_idx = head as usize % CAPACITY;
            let right = CAPACITY - head_idx;

            // We optimistically copy the values from the buffer into the dst.
            // On CAS failure, we forget the copied values and try again.
            // It is safe because we can concurrently read from the head.

            if n <= right {
                // No wraparound, copy in one shot
                unsafe {
                    ptr::copy_nonoverlapping(self.buffer_thin_ptr().add(head_idx), dst_ptr, n);
                }
            } else {
                unsafe {
                    // Wraparound: copy right half then left half
                    ptr::copy_nonoverlapping(self.buffer_thin_ptr().add(head_idx), dst_ptr, right);
                    ptr::copy_nonoverlapping(self.buffer_thin_ptr(), dst_ptr.add(right), n - right);
                }
            }

            'weak_cas_loop: loop {
                // Now claim ownership
                match self.head.compare_exchange_weak(
                    head,
                    head.wrapping_add(n as LongNumber),
                    Release,
                    Acquire,
                ) {
                    Ok(_) => return n,
                    Err(actual_head) => {
                        if unlikely(actual_head == head) {
                            // we can just retry, it is a false positive
                            continue 'weak_cas_loop;
                        }

                        // CAS failed, forget read values (they're MaybeUninit, so it's fine)
                        // But don't try to drop, just retry
                        head = actual_head;

                        tail = self.tail.load(Acquire);

                        continue 'top;
                    }
                }
            }
        }
    }

    /// Steals many values from the consumer to the `dst`.
    /// Returns the number of values stolen.
    /// 
    /// # Panics
    /// 
    /// If `dst` is not empty.
    pub fn steal_into(&self, dst: &Self) -> usize {
        let mut src_head = self.head.load(Acquire);
        let dst_tail = unsafe { dst.tail.unsync_load() }; // only producer can change tail

        if cfg!(debug_assertions) {
            let dst_head = dst.head.load(Relaxed);

            assert_eq!(
                dst_head, dst_tail,
                "steal_into should not be called when dst is not empty"
            );
        }

        'top: loop {
            let src_tail = self.tail.load(Acquire);
            let n = Self::len(src_head, src_tail) / 2;

            if n > CAPACITY / 2 {
                // Inconsistent state (this thread has been preempted
                // after we have loaded `src_head`,
                // and before we have loaded `src_tail`),
                // try again

                src_head = self.head.load(Acquire);

                continue;
            }

            if !cfg!(feature = "always_steal") && n < 4 || n == 0 {
                // we don't steal less than 4 by default
                // because else we may lose more because of cache locality and NUMA awareness
                return 0;
            }

            let src_head_idx = src_head as usize % CAPACITY;

            let (src_right, src_left): (&[T], &[T]) = unsafe {
                let right_occupied = CAPACITY - src_head_idx;
                if n <= right_occupied {
                    (
                        slice::from_raw_parts(self.buffer_thin_ptr().add(src_head_idx).cast(), n),
                        &[],
                    )
                } else {
                    (
                        slice::from_raw_parts(
                            self.buffer_thin_ptr().add(src_head_idx).cast(),
                            right_occupied,
                        ),
                        slice::from_raw_parts(self.buffer_thin_ptr().cast(), n - right_occupied),
                    )
                }
            };

            // We optimistically copy the values from the buffer into the dst.
            // On CAS failure, we forget the copied values and try again.
            // It is safe because we can concurrently read from the head.
            Self::copy_slice(
                dst.buffer_mut_thin_ptr().cast::<T>(),
                dst_tail % CAPACITY as LongNumber,
                src_right,
            );
            Self::copy_slice(
                dst.buffer_mut_thin_ptr().cast::<T>(),
                (dst_tail.wrapping_add(src_right.len() as LongNumber)) % CAPACITY as LongNumber,
                src_left,
            );

            let res = self.head.compare_exchange(
                src_head,
                src_head.wrapping_add(n as LongNumber),
                Release,
                Acquire,
            );

            match res {
                Ok(_) => {
                    // Success, we can move dst tail and return
                    dst.tail
                        .store(dst_tail.wrapping_add(n as LongNumber), Release);

                    return n;
                }
                Err(current_head) => {
                    // another thread has read the same values, full retry
                    src_head = current_head;

                    continue 'top;
                }
            }
        }
    }
}

impl<T, const CAPACITY: usize, AtomicWrapper: Deref<Target = LongAtomic> + Default> Default for SPMCBoundedQueue<T, CAPACITY, AtomicWrapper> {
    fn default() -> Self {
        Self::new()
    }
}

unsafe impl<T, const CAPACITY: usize, AtomicWrapper> Sync
    for SPMCBoundedQueue<T, CAPACITY, AtomicWrapper>
where
    AtomicWrapper: Deref<Target = LongAtomic> + Default,
{
}
unsafe impl<T, const CAPACITY: usize, AtomicWrapper> Send
    for SPMCBoundedQueue<T, CAPACITY, AtomicWrapper>
where
    AtomicWrapper: Deref<Target = LongAtomic> + Default,
{
}

impl<T, const CAPACITY: usize, AtomicWrapper> Drop for SPMCBoundedQueue<T, CAPACITY, AtomicWrapper>
where
    AtomicWrapper: Deref<Target = LongAtomic> + Default,
{
    fn drop(&mut self) {
        // While dropping there is no concurrency

        if needs_drop::<T>() {
            let mut head = unsafe { self.head.unsync_load() };
            let tail = unsafe { self.tail.unsync_load() };

            while head != tail {
                unsafe {
                    ptr::drop_in_place(
                        self.buffer_thin_ptr()
                            .add(head as usize % CAPACITY)
                            .cast::<T>()
                            .cast_mut(),
                    );
                }

                head = head.wrapping_add(1);
            }
        }

        unsafe { drop(Box::from_raw(self.buffer)) };
    }
}

/// Generates SPMC producer and consumer.
macro_rules! generate_spmc_producer_and_consumer {
    ($producer_name:ident, $consumer_name:ident, $atomic_wrapper:ty) => {
        /// The producer of the [`SPMCBoundedQueue`].
        pub struct $producer_name<T, const CAPACITY: usize> {
            inner: LightArc<SPMCBoundedQueue<T, CAPACITY, $atomic_wrapper>>,
        }

        impl<T: Send, const CAPACITY: usize> Producer<T> for $producer_name<T, CAPACITY> {
            #[inline]
            fn capacity(&self) -> usize {
                CAPACITY as usize
            }

            #[inline]
            fn len(&mut self) -> usize {
                unsafe { self.inner.producer_len() }
            }

            #[inline]
            fn push<SBR: SyncBatchReceiver<T>>(&mut self, value: T, sync_batch_receiver: &SBR) {
                unsafe { self.inner.producer_push(value, sync_batch_receiver) };
            }

            #[inline]
            fn maybe_push(&mut self, value: T) -> Result<(), T> {
                unsafe { self.inner.producer_maybe_push(value) }
            }

            #[inline]
            fn pop(&mut self) -> Option<T> {
                unsafe { self.inner.producer_pop() }
            }

            #[inline]
            fn pop_many(&mut self, dst: &mut [MaybeUninit<T>]) -> usize {
                unsafe { self.inner.producer_pop_many(dst) }
            }

            #[inline]
            unsafe fn push_many_unchecked(&mut self, first: &[T], last: &[T]) {
                unsafe { self.inner.producer_push_many_unchecked(first, last) };
            }

            #[inline]
            fn maybe_push_many(&mut self, slice: &[T]) -> Result<(), ()> {
                unsafe { self.inner.producer_maybe_push_many(slice) }
            }

            #[inline]
            fn push_many<SBR: SyncBatchReceiver<T>>(
                &mut self,
                slice: &[T],
                sync_batch_receiver: &SBR,
            ) {
                unsafe { self.inner.producer_push_many(slice, sync_batch_receiver) };
            }
        }

        unsafe impl<T: Send, const CAPACITY: usize> Sync for $producer_name<T, CAPACITY> {}
        unsafe impl<T: Send, const CAPACITY: usize> Send for $producer_name<T, CAPACITY> {}

        /// The consumer of the [`SPMCBoundedQueue`].
        pub struct $consumer_name<T, const CAPACITY: usize> {
            inner: LightArc<SPMCBoundedQueue<T, CAPACITY, $atomic_wrapper>>,
            _non_sync: PhantomData<*const ()>,
        }

        impl<T: Send, const CAPACITY: usize> Consumer<T> for $consumer_name<T, CAPACITY> {
            type AssociatedProducer = $producer_name<T, CAPACITY>;

            #[inline]
            fn capacity(&mut self) -> usize {
                CAPACITY as usize
            }

            #[inline]
            fn len(&mut self) -> usize {
                self.inner.consumer_len()
            }

            #[inline]
            fn pop_many(&mut self, dst: &mut [MaybeUninit<T>]) -> usize {
                self.inner.consumer_pop_many(dst)
            }

            #[inline]
            fn steal_into(&mut self, dst: &mut Self::AssociatedProducer) -> usize {
                self.inner.steal_into(&*dst.inner)
            }
        }

        impl<T, const CAPACITY: usize> Clone for $consumer_name<T, CAPACITY> {
            fn clone(&self) -> Self {
                Self {
                    inner: self.inner.clone(),
                    _non_sync: PhantomData,
                }
            }
        }

        unsafe impl<T: Send, const CAPACITY: usize> Send for $consumer_name<T, CAPACITY> {}
    };

    ($producer_name:ident, $consumer_name:ident) => {
        generate_spmc_producer_and_consumer!(
            $producer_name,
            $consumer_name,
            NotCachePaddedLongAtomic
        );
    };
}

generate_spmc_producer_and_consumer!(SPMCProducer, SPMCConsumer);

/// Creates a new single-producer, multi-consumer queue with the given capacity.
/// Returns [`producer`](SPMCProducer) and [`consumer`](SPMCConsumer).
///
/// It accepts the capacity as a const generic parameter.
/// We recommend using a power of two.
///
/// The producer __should__ be only one while consumers can be cloned.
/// If you want to use more than one producer, don't use this queue.
///
/// If you want to use only one consumer, look at the single-producer, single-consumer queue.
///
/// # Cache padding
///
/// Cache padding can improve the performance of the queue many times, but it also requires
/// much more memory (likely 128 or 256 more bytes for the queue).
/// If you can sacrifice some memory for the performance, use [`new_cache_padded_bounded`].
///
/// # Examples
///
/// ```
/// use parcoll::spmc::{new_bounded, Producer, Consumer};
/// use std::sync::Arc;
///
/// let (mut producer, mut consumer) = new_bounded::<_, 256>();
/// let consumer2 = consumer.clone(); // You can clone the consumer
///
/// producer.maybe_push(1).unwrap();
/// producer.maybe_push(2).unwrap();
///
/// let mut slice = [std::mem::MaybeUninit::uninit(); 3];
/// let popped = consumer.pop_many(&mut slice);
///
/// assert_eq!(popped, 2);
/// assert_eq!(unsafe { slice[0].assume_init() }, 1);
/// assert_eq!(unsafe { slice[1].assume_init() }, 2);
/// ```
pub fn new_bounded<T, const CAPACITY: usize>()
-> (SPMCProducer<T, CAPACITY>, SPMCConsumer<T, CAPACITY>) {
    let queue = LightArc::new(SPMCBoundedQueue::new());

    (
        SPMCProducer {
            inner: queue.clone(),
        },
        SPMCConsumer {
            inner: queue,
            _non_sync: PhantomData,
        },
    )
}

generate_spmc_producer_and_consumer!(
    CachePaddedSPMCProducer,
    CachePaddedSPMCConsumer,
    CachePaddedLongAtomic
);

/// Creates a new single-producer, multi-consumer queue with the given capacity.
/// Returns [`producer`](CachePaddedSPMCProducer) and [`consumer`](CachePaddedSPMCConsumer).
///
/// It accepts the capacity as a const generic parameter.
/// We recommend using a power of two.
///
/// The producer __should__ be only one while consumers can be cloned.
/// If you want to use more than one producer, don't use this queue.
///
/// If you want to use only one consumer, look at the single-producer, single-consumer queue.
///
/// # Cache padding
///
/// Cache padding can improve the performance of the queue many times, but it also requires
/// much more memory (likely 128 or 256 more bytes for the queue).
/// If you can't sacrifice some memory for the performance, use [`new_bounded`].
///
/// # Examples
///
/// ```
/// use parcoll::spmc::{new_bounded, Producer, Consumer};
/// use std::sync::Arc;
///
/// let (mut producer, mut consumer) = new_bounded::<_, 256>();
/// let consumer2 = consumer.clone(); // You can clone the consumer
///
/// producer.maybe_push(1).unwrap();
/// producer.maybe_push(2).unwrap();
///
/// let mut slice = [std::mem::MaybeUninit::uninit(); 3];
/// let popped = consumer.pop_many(&mut slice);
///
/// assert_eq!(popped, 2);
/// assert_eq!(unsafe { slice[0].assume_init() }, 1);
/// assert_eq!(unsafe { slice[1].assume_init() }, 2);
/// ```
pub fn new_cache_padded_bounded<T, const CAPACITY: usize>() -> (
    CachePaddedSPMCProducer<T, CAPACITY>,
    CachePaddedSPMCConsumer<T, CAPACITY>,
) {
    let queue = LightArc::new(SPMCBoundedQueue::new());

    (
        CachePaddedSPMCProducer {
            inner: queue.clone(),
        },
        CachePaddedSPMCConsumer {
            inner: queue,
            _non_sync: PhantomData,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutex_vec_queue::MutexVecQueue;
    use std::collections::VecDeque;

    const CAPACITY: usize = 256;

    #[test]
    fn test_spmc_bounded_size() {
        let queue = SPMCBoundedQueue::<(), CAPACITY>::new();

        assert_eq!(
            size_of_val(&queue),
            size_of::<usize>() + size_of::<LongAtomic>() * 2
        );

        let cache_padded_queue = SPMCBoundedQueue::<(), CAPACITY, CachePaddedLongAtomic>::new();

        assert_eq!(
            size_of_val(&cache_padded_queue),
            size_of::<CachePaddedLongAtomic>() * 2 + size_of::<usize>()
        );
    }

    #[test]
    fn test_spmc_bounded_seq_insertions() {
        let global_queue = MutexVecQueue::new();
        let (mut producer, _) = new_bounded::<_, CAPACITY>();

        for i in 0..CAPACITY * 100 {
            producer.push(i, &global_queue);
        }

        let (mut new_producer, _) = new_bounded::<_, CAPACITY>();

        global_queue
            .move_batch_to_producer(&mut new_producer, producer.capacity() - producer.len());

        assert_eq!(
            producer.len() + new_producer.len() + global_queue.len(),
            CAPACITY * 100
        );

        for _ in 0..producer.len() {
            assert!(producer.pop().is_some());
        }

        for _ in 0..new_producer.len() {
            assert!(new_producer.pop().is_some());
        }
    }

    #[test]
    fn test_spmc_bounded_stealing() {
        const TRIES: usize = 10;

        let global_queue = MutexVecQueue::new();
        let (mut producer1, mut consumer) = new_bounded::<_, CAPACITY>();
        let (mut producer2, _) = new_bounded::<_, CAPACITY>();

        let mut stolen = VecDeque::new();

        for _ in 0..TRIES * 2 {
            for i in 0..CAPACITY / 2 {
                producer1.push(i, &global_queue);
            }

            consumer.steal_into(&mut producer2);

            while let Some(task) = producer2.pop() {
                stolen.push_back(task);
            }

            assert!(global_queue.is_empty());
        }

        assert!(producer2.is_empty());

        let mut count = 0;

        while let Some(_) = producer1.pop() {
            count += 1;
        }

        assert_eq!(count + stolen.len() + global_queue.len(), CAPACITY * TRIES);
    }

    #[test]
    fn test_spmc_bounded_many() {
        const BATCH_SIZE: usize = 30;
        const N: usize = BATCH_SIZE * 100;

        let global_queue = MutexVecQueue::new();
        let (mut producer, mut consumer) = new_bounded::<_, CAPACITY>();

        for i in 0..N / BATCH_SIZE / 2 {
            let slice = (0..BATCH_SIZE)
                .map(|j| i * BATCH_SIZE + j)
                .collect::<Vec<_>>();

            producer.maybe_push_many(&*slice).unwrap();

            let mut slice = [MaybeUninit::uninit(); BATCH_SIZE];
            producer.pop_many(slice.as_mut_slice());

            for j in 0..BATCH_SIZE {
                let index = i * BATCH_SIZE + j;

                assert_eq!(unsafe { slice[j].assume_init() }, index);
            }
        }

        for i in 0..N / BATCH_SIZE / 2 {
            let slice = (0..BATCH_SIZE)
                .map(|j| i * BATCH_SIZE + j)
                .collect::<Vec<_>>();

            producer.push_many(&*slice, &global_queue);

            assert!(global_queue.is_empty());

            let mut slice = [MaybeUninit::uninit(); BATCH_SIZE];
            consumer.pop_many(slice.as_mut_slice());

            for j in 0..BATCH_SIZE {
                let index = i * BATCH_SIZE + j;

                assert_eq!(unsafe { slice[j].assume_init() }, index);
            }
        }
    }
}
