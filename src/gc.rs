use std::collections::VecDeque;
use std::cell::{Cell, RefCell, UnsafeCell};
use std::rc::Rc;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::{mem, usize, f64};

pub struct GcParameters {
    // The garbage collector will wait until the live size reaches <current heap size> + <previous
    // retained size> * `pause_multiplier` before beginning a new collection.  Must be >= 0.0,
    // setting this to 0.0 causes the collector to never sleep longer than `min_sleep` before
    // beginning a new collection.
    pause_factor: f64,
    // The garbage collector will try and finish a collection by the time <current heap size> *
    // `timing_factor` additional bytes are allocated.  For example, if the collection is started
    // when the `GcArena` has 100KB live data, and the timing_multiplier is 1.0, the collector
    // should finish its final phase of this collection after another 100KB has been allocated.
    // Must be >= 0.0, setting this to 0.0 causes the collector to behave like a stop-the-world
    // collector.
    timing_factor: f64,
    // The garbage collector will try to collect no fewer than this many bytes as a single unit of
    // work.
    collection_granularity: usize,
    // The minimum allocation amount during sleep before the `GcArena` starts collecting again.
    // This is mostly useful when the heap is very small to prevent rapidly restarting collections.
    min_sleep: usize,
}

impl Default for GcParameters {
    fn default() -> GcParameters {
        const PAUSE_FACTOR: f64 = 0.5;
        const TIMING_FACTOR: f64 = 1.5;
        const COLLECTION_GRANULARITY: usize = 1024;
        const MIN_SLEEP: usize = 4096;

        GcParameters {
            pause_factor: PAUSE_FACTOR,
            timing_factor: TIMING_FACTOR,
            collection_granularity: COLLECTION_GRANULARITY,
            min_sleep: MIN_SLEEP,
        }
    }
}

/// A trait for garbage collected objects that can be placed into `Gc` pointers, and may hold `Gc`
/// pointers from the same parent `GcArena`.  Held `Gc` pointers must not be accessed in drop impls,
/// as the drop order on sweep is not predictable and it is impossible to know whether they are
/// dangling.  A `GcObject` may have internal mutability, but any internal mutability that causes
/// new `Gc` pointers to be contained *must* be accompanied by triggering the write barrier
/// on this object.
pub trait GcObject: 'static {
    /// Return false if this type will never contain Gc pointers.  This object will skip the gray
    /// object queue during tracing, and never have `GcObject::trace` called on it.
    fn needs_trace() -> bool
    where
        Self: Sized,
    {
        true
    }

    /// Must call `tracer.trace` on all held Gc pointers to ensure that they are not collected.
    /// Unsafe, because held `Gc` pointers may be dangling (even if this trait is implemented
    /// correctly, they may be dangling by breaking other rules).  Return true if the object was
    /// successfully traced, false if tracing was blocked for some reason (such as locking for
    /// internal mutability).  A locked trace method will delay the sweep phase, so an object should
    /// remain locked for as little time as possible.
    unsafe fn trace<'a>(&self, _tracer: &GcTracer<'a>) -> bool {
        true
    }
}

pub struct GcTracer<'a> {
    arena: &'a GcArena,
}

impl<'a> GcTracer<'a> {
    pub unsafe fn trace<T: GcObject>(&self, gc: Gc<T>) {
        let gc_box = gc.gc_box.as_ref();
        match gc_box.flags.color() {
            GcColor::Black | GcColor::DarkGray => {}
            GcColor::LightGray => {
                // A light-gray object is already in the gray queue, just turn it dark gray.
                gc_box.flags.set_color(GcColor::DarkGray);
            }
            GcColor::White => {
                if gc_box.flags.needs_trace() {
                    // A white traceable object is not in the gray queue, becomes dark-gray and
                    // enters in the queue at the front.
                    gc_box.flags.set_color(GcColor::DarkGray);
                    self.arena.gray.borrow_mut().push_front(gc.gc_box);
                } else {
                    // A white un-traceable object simply becomes black.
                    gc_box.flags.set_color(GcColor::Black);
                }
            }
        }
    }
}

/// A collection of objects that may contain garbage collected `Gc<T>` pointers.  The garbage
/// collector is designed to be as low overhead as possible, so much of the functionality around
/// garbage collection is unsafe.
pub struct GcArena {
    parameters: GcParameters,

    phase: Cell<GcPhase>,
    total_allocated: Cell<usize>,
    remembered_size: Cell<usize>,
    wakeup_total: Cell<usize>,
    allocation_debt: Cell<f64>,
    granularity_timer: Cell<usize>,

    all: Cell<Option<NonNull<GcBox<GcObject>>>>,
    sweep: Cell<Option<NonNull<GcBox<GcObject>>>>,
    sweep_prev: Cell<Option<NonNull<GcBox<GcObject>>>>,

    gray: RefCell<VecDeque<NonNull<GcBox<GcObject>>>>,
}

impl Drop for GcArena {
    fn drop(&mut self) {
        unsafe {
            let mut next = self.all.get();
            while let Some(p) = next {
                let gc_box = p.as_ref();
                next = gc_box.next.get();
                if gc_box.root_count.is_rooted() {
                    gc_box.flags.set_detached(true);
                } else {
                    Box::from_raw(p.as_ptr());
                }
            }
        }
    }
}

impl GcArena {
    pub fn new(parameters: GcParameters) -> GcArena {
        let min_sleep = parameters.min_sleep;
        let collection_granularity = parameters.collection_granularity;
        GcArena {
            parameters,
            phase: Cell::new(GcPhase::Sleeping),
            total_allocated: Cell::new(0),
            remembered_size: Cell::new(0),
            wakeup_total: Cell::new(min_sleep),
            allocation_debt: Cell::new(0.0),
            granularity_timer: Cell::new(collection_granularity),
            all: Cell::new(None),
            sweep: Cell::new(None),
            sweep_prev: Cell::new(None),
            gray: RefCell::new(VecDeque::new()),
        }
    }

    /// Allocate space for a value of type T, and move it into a `Gc<T>` pointer.  May trigger
    /// collection of other unreachable Gc pointers.  In order to ensure that the returned `Gc<T>`
    /// is not collected before use, it must be placed into a managed `GcObject` impl before any
    /// additional collection is triggered, either through allocating again or other methods that
    /// trigger collection.
    pub fn allocate<T: GcObject>(&self, value: T) -> Gc<T> {
        let alloc_size = mem::size_of::<GcBox<T>>();
        self.total_allocated
            .set(self.total_allocated.get() + alloc_size);
        if self.phase.get() == GcPhase::Sleeping {
            if self.total_allocated.get() > self.wakeup_total.get() {
                self.phase.set(GcPhase::Propagating);
            }
        }

        if self.phase.get() != GcPhase::Sleeping {
            self.allocation_debt.set(
                self.allocation_debt.get() + alloc_size as f64
                    + alloc_size as f64 / self.parameters.timing_factor,
            );

            let granularity_timer = self.granularity_timer.get();
            if granularity_timer + alloc_size >= self.parameters.collection_granularity {
                self.granularity_timer.set(0);
                self.do_collection(self.allocation_debt.get());
            } else {
                self.granularity_timer.set(granularity_timer + alloc_size);
            }
        }

        let gc_box = GcBox {
            flags: GcFlags::new(),
            root_count: RootCount::new(),
            next: Cell::new(self.all.get()),
            value: UnsafeCell::new(value),
        };
        gc_box.flags.set_needs_trace(T::needs_trace());
        let gc_box = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(gc_box))) };
        self.all.set(Some(gc_box));
        if self.phase.get() == GcPhase::Sweeping {
            if self.sweep_prev.get().is_none() {
                self.sweep_prev.set(self.all.get());
            }
        }

        Gc {
            gc_box,
            marker: PhantomData,
        }
    }

    /// Allocate a T and return a root `Rgc` pointer.
    pub fn allocate_root<T: GcObject>(&self, value: T) -> Rgc<T> {
        unsafe { self.root(self.allocate(value)) }
    }

    /// "Root" the given `Gc` pointer, turning it into an `Rgc`.  Root pointers are never collected,
    /// and `Gc` pointers are considered "reachable" only if they can be traced from a root pointer.
    /// Must not be called on a dangling pointer.
    pub unsafe fn root<T: GcObject>(&self, gc: Gc<T>) -> Rgc<T> {
        let gc_box = gc.gc_box.as_ref();
        gc_box.root_count.increment();
        if gc_box.flags.color() == GcColor::White {
            // If our object is white, rooting it should turn it light-gray and place it into the
            // gray queue at the end.  This is done to give the object the maximum amount of time to
            // potentially become un-rooted.
            gc_box.flags.set_color(GcColor::LightGray);
            self.gray.borrow_mut().push_back(gc.gc_box);
        }
        Rgc(gc)
    }

    /// Trigger a "write barrier" on the given object.  If an object is being mutated in such a way
    /// that it may contain a `Gc` pointer that it did not before, you must call this method on a
    /// pointer to that object.  This method may be called before or after the addition, as long as
    /// there are no collections triggered between either when the addition occurs and the call to
    /// `write_barrier`, or if tracing is blocked during mutation, between the period where calls to
    /// `GcObject::trace` on the containing object return false and the call to `write_barrier`.
    /// Must not be called on a dangling `Gc` pointer.
    pub unsafe fn write_barrier<T: GcObject>(&self, gc: Gc<T>) {
        // During the propagating phase, if we are adding a white or light-gray object to a black
        // object, we need it to become dark gray to uphold the black invariant.
        let gc_box = gc.gc_box.as_ref();
        if self.phase.get() == GcPhase::Propagating && gc_box.flags.color() == GcColor::Black {
            gc_box.flags.set_color(GcColor::DarkGray);
            self.gray.borrow_mut().push_back(gc.gc_box);
        }
    }

    /// Run the current garbage collection cycle to completion.  If the garbage collector is
    /// currently sleeping, starts a new cycle and runs it to completion.
    pub fn collect_garbage(&self) {
        if self.phase.get() == GcPhase::Sleeping {
            self.phase.set(GcPhase::Propagating);
        }
        self.do_collection(f64::INFINITY);
    }
}

/// A garbage collected pointer to a value of type T.  Implements Copy, and is a zero-overhead
/// wrapper around a raw pointer.  Any access is generally unsafe because in order to guarantee that
/// it is not dangling, it must be held inside a correct `GcObject` implementation, which is itself
/// held inside a `Gc` or `Rgc` pointer, and the parent `GcArena` must not have been dropped.
#[derive(Eq, PartialEq, Debug)]
pub struct Gc<T: GcObject + ?Sized> {
    gc_box: NonNull<GcBox<T>>,
    marker: PhantomData<Rc<T>>,
}

impl<T: GcObject> Copy for Gc<T> {}

impl<T: GcObject> Clone for Gc<T> {
    fn clone(&self) -> Gc<T> {
        *self
    }
}

impl<T: GcObject> Gc<T> {
    pub unsafe fn as_ptr(&self) -> *mut T {
        self.gc_box.as_ref().value.get()
    }

    pub unsafe fn as_ref(&self) -> &T {
        &*self.gc_box.as_ref().value.get()
    }
}

/// A "root pointer" into a `GcArena`.  This is guaranteed never to be dangling, so it is always
/// safe to access.  After the parent `GcArena` is dropped, `Rgc` behaves similar to an Rc, so the
/// contents will be dropped only when the final `Rgc` pointing to it is dropped.  This is mostly
/// useful for Drop safety, normally `Rgc` pointers should not outlive the parent `GcArena`, as
/// any held `Gc` pointers would no longer be valid.
#[derive(Eq, PartialEq, Debug)]
pub struct Rgc<T: GcObject>(Gc<T>);

impl<T: GcObject> Clone for Rgc<T> {
    fn clone(&self) -> Rgc<T> {
        unsafe {
            self.0.gc_box.as_ref().root_count.increment();
            Rgc(self.0)
        }
    }
}

impl<T: GcObject> Drop for Rgc<T> {
    fn drop(&mut self) {
        unsafe {
            let gc_box = self.0.gc_box.as_ref();
            gc_box.root_count.decrement();
            if !gc_box.root_count.is_rooted() && gc_box.flags.is_detached() {
                // If the managed GcBox is detached (the parent GcArena has been dropped), and we
                // are the last Rgc pointer, delete the contents.
                Box::from_raw(self.0.gc_box.as_ptr());
            }
        }
    }
}

impl<T: GcObject> Rgc<T> {
    pub fn gc(&self) -> Gc<T> {
        self.0
    }

    pub fn as_ptr(&self) -> *mut T {
        unsafe { self.0.as_ptr() }
    }

    pub fn as_ref(&self) -> &T {
        unsafe { self.0.as_ref() }
    }
}

impl GcArena {
    // Do some collection work until we have either reached the target amount of work or have
    // entered the sleeping gc phase.  The unit of "work" here is a byte count of objects either
    // turned black or freed, so to completely collect a heap with 1000 bytes of objects should take
    // 1000 units of work, whatever percentage of them are live or not.
    fn do_collection(&self, work: f64) {
        let mut work_left = work;
        let tracer = GcTracer { arena: self };
        let mut blocked_gray_count = 0;

        while work_left > 0.0 {
            match self.phase.get() {
                GcPhase::Sleeping => break,
                GcPhase::Propagating => unsafe {
                    let next_gray = self.gray.borrow_mut().pop_front();
                    if let Some(gc_box_ptr) = next_gray {
                        let gc_box = gc_box_ptr.as_ref();
                        if gc_box.flags.color() == GcColor::DarkGray
                            || gc_box.root_count.is_rooted()
                        {
                            if (*gc_box.value.get()).trace(&tracer) {
                                // Once an object is successfully traced, we turn it black and
                                // remove it from the queue.
                                gc_box.flags.set_color(GcColor::Black);
                                work_left -= mem::size_of_val(gc_box) as f64;
                                blocked_gray_count = 0;
                            } else {
                                let mut gray = self.gray.borrow_mut();
                                // If an object is blocked from tracing, place it on the back of the
                                // queue to give it the maximum amount of time to un-block.
                                gray.push_back(gc_box_ptr);
                                blocked_gray_count += 1;
                                if blocked_gray_count == gray.len() {
                                    // If the entirety of the gray queue is un-traceable, we can't
                                    // make any progress so just stop.
                                    break;
                                }
                            }
                        }
                    } else {
                        // Once all the grays objects have been processed, we enter the sweeping
                        // phase.
                        self.phase.set(GcPhase::Sweeping);
                        self.sweep.set(self.all.get());
                        self.remembered_size.set(0);
                    }
                },
                GcPhase::Sweeping => unsafe {
                    if let Some(sweep_ptr) = self.sweep.get() {
                        let sweep = sweep_ptr.as_ref();
                        let sweep_size = mem::size_of_val(sweep);

                        let next_ptr = sweep.next.get();
                        self.sweep.set(next_ptr);

                        if sweep.flags.color() == GcColor::White {
                            // We need to remove this object from the main object list.
                            if let Some(sweep_prev) = self.sweep_prev.get() {
                                sweep_prev.as_ref().next.set(next_ptr);
                            } else {
                                // If `sweep_prev` is None, then the sweep pointer is also the
                                // beginning of the main object list, so we need to adjust it.
                                debug_assert_eq!(self.all.get(), Some(sweep_ptr));
                                self.all.set(next_ptr);
                            }
                            self.total_allocated
                                .set(self.total_allocated.get() - sweep_size);
                            work_left -= sweep_size as f64;
                            Box::from_raw(sweep_ptr.as_ptr());
                        } else {
                            // No gray objects should be in the swept portion of the list.
                            debug_assert_eq!(sweep.flags.color(), GcColor::Black);
                            self.sweep_prev.set(Some(sweep_ptr));
                            self.remembered_size
                                .set(self.remembered_size.get() + sweep_size);
                            // If the object we encounter is rooted, it needs to become light-gray
                            // and enter the gray queue, otherwise it just becomes white again.
                            if sweep.root_count.is_rooted() {
                                sweep.flags.set_color(GcColor::LightGray);
                                self.gray.borrow_mut().push_back(sweep_ptr);
                            } else {
                                sweep.flags.set_color(GcColor::White);
                            }
                        }
                    } else {
                        // We are done sweeping, so enter the sleeping phase.
                        self.sweep_prev.set(None);
                        self.phase.set(GcPhase::Sleeping);
                        self.wakeup_total.set(
                            self.total_allocated.get()
                                + ((self.remembered_size.get() as f64
                                    * self.parameters.pause_factor)
                                    .round()
                                    .min(usize::MAX as f64)
                                    as usize)
                                    .max(self.parameters.min_sleep),
                        );
                    }
                },
            }
        }

        if self.phase.get() == GcPhase::Sleeping {
            // Do not let debt accumulate across cycles
            self.allocation_debt.set(0.0);
        } else {
            self.allocation_debt
                .set((self.allocation_debt.get() - work + work_left).max(0.0));
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum GcColor {
    // White objects are unmarked and un-rooted.  At the end of the mark phase, all white objects
    // are unused and may be freed in the sweep phase.
    White,
    // When a white object is rooted, it becomes light-gray and placed in the gray queue.  When it
    // is processed in the gray queue, if it is still rooted at the time of processing, its
    // sub-objects are traced and it becomes black.  If it is not rooted at the time of processing
    // it is turned white.
    LightGray,
    // When a white or light gray object is traced, it becomes dark-gray.  When a dark-gray object
    // is processed, its sub-objects are traced and it becomes black.
    DarkGray,
    // Black objects have been marked and their sub-objects must all be dark-gray or black.  If a
    // white or light-gray object is made a child of a black object (and the black invariant is
    // currently being held), a write barrier must be executed that either turns that child
    // dark-gray (forward) or turns the black object back dark-gray (back).
    Black,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum GcPhase {
    Sleeping,
    Propagating,
    Sweeping,
}

struct GcBox<T: GcObject + ?Sized> {
    flags: GcFlags,
    root_count: RootCount,
    next: Cell<Option<NonNull<GcBox<GcObject>>>>,

    value: UnsafeCell<T>,
}

struct GcFlags(Cell<u8>);

impl GcFlags {
    fn new() -> GcFlags {
        GcFlags(Cell::new(0))
    }

    fn color(&self) -> GcColor {
        match self.0.get() & 0x3 {
            0x0 => GcColor::White,
            0x1 => GcColor::LightGray,
            0x2 => GcColor::DarkGray,
            0x3 => GcColor::Black,
            _ => unreachable!(),
        }
    }

    fn set_color(&self, color: GcColor) {
        self.0.set(
            (self.0.get() & !0x3) | match color {
                GcColor::White => 0x0,
                GcColor::LightGray => 0x1,
                GcColor::DarkGray => 0x2,
                GcColor::Black => 0x3,
            },
        )
    }

    fn is_detached(&self) -> bool {
        self.0.get() & 0x4 != 0x0
    }

    fn set_detached(&self, detached: bool) {
        self.0
            .set((self.0.get() & !0x4) | if detached { 0x4 } else { 0x0 });
    }

    fn needs_trace(&self) -> bool {
        self.0.get() | 0x8 != 0x0
    }

    fn set_needs_trace(&self, needs_trace: bool) {
        self.0
            .set((self.0.get() & !0x8) | if needs_trace { 0x8 } else { 0x0 });
    }
}

struct RootCount(Cell<usize>);

impl RootCount {
    fn new() -> RootCount {
        RootCount(Cell::new(0))
    }

    fn is_rooted(&self) -> bool {
        self.0.get() != 0
    }

    fn increment(&self) {
        assert!(self.0.get() != usize::MAX, "overflow on root count");
        self.0.set(self.0.get() + 1);
    }

    fn decrement(&self) {
        debug_assert!(self.0.get() > 0, "underflow on root count");
        self.0.set(self.0.get() - 1);
    }
}