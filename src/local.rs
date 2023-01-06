use crate::{Domain, Global, HazardPointer};
use crate::pointer::{Reclaim, Pointer};
use core::cell::{RefCell, Cell};
use std::mem;
use std::thread_local;

thread_local! {
    static DEF_LOCAL_RETIRED: RefCell<LocalBag<'static, Global>> = RefCell::new(LocalBag::new(Domain::global()));
}

#[inline]
#[allow(clippy::mutable_key_type, missing_docs)]
pub fn retire_locally<T>(ptr: *mut T)
where
    T: Send
{
    DEF_LOCAL_RETIRED.with(|r| {
        unsafe { r.borrow_mut().retire::<_, Box<_>>(ptr) }
    })
}

#[inline]
#[allow(clippy::mutable_key_type, missing_docs)]
pub fn retire_locally_pp<T>(ptr: *mut T)
where
    T: Send
{
    DEF_LOCAL_RETIRED.with(|r| {
        unsafe { r.borrow_mut().retire_pp::<_, Box<_>>(ptr) }
    })
}

#[allow(missing_docs)]
pub fn try_unlink<'l, T: 'l, F1, F2>(
    links: &[*mut T],
    to_be_unlinked: &[*mut T],  // used slices instead of Iterators
    do_unlink: F1,              // because they must be used multiple times.
    set_stop: F2,
) -> bool
where
    T: Send,
    F1: FnOnce() -> bool,
    F2: Fn(*mut T) -> ()
{
    let mut hps: Vec<_> = links
        .iter()
        .map(|&ptr| {
            let mut hp = HazardPointer::new();
            hp.protect_raw(ptr);
            hp
        })
        .collect();
    
    let unlinked = do_unlink();
    if unlinked {
        for &ptr in to_be_unlinked {
            set_stop(ptr);
        }
        DEF_LOCAL_RETIRED.with(|r| {
            let mut local = r.borrow_mut();
            local.hps.append(&mut hps);
            for &ptr in to_be_unlinked {
                unsafe { local.retire_pp::<_, Box<_>>(ptr) }
            }
        });
    } else {
        drop(hps);
    }
    // drop(hps);
    unlinked
}

pub struct LocalRetired {
    ptr: *mut dyn Reclaim,
    deleter: unsafe fn(ptr: *mut dyn Reclaim),
}

impl LocalRetired {
    unsafe fn new<'domain, F>(
        _: &'domain Domain<F>,
        ptr: *mut (dyn Reclaim + 'domain),
        deleter: unsafe fn(ptr: *mut dyn Reclaim),
    ) -> Self {
        Self {
            ptr: unsafe { core::mem::transmute::<_, *mut (dyn Reclaim + 'static)>(ptr) },
            deleter
        }
    }
}

pub struct LocalBag<'s, F> {
    domain: &'s Domain<F>,
    // It contains pairs of (pointer, deleter)
    retired: Vec<LocalRetired>,
    // Used for HP++
    hps: Vec<HazardPointer<'s>>,
    collect_count: Cell<usize>,
}

impl<'s, F> LocalBag<'s, F> {
    const COUNTS_BETWEEN_COLLECT: usize = 128;

    pub fn new(domain: &'s Domain<F>) -> Self {
        Self {
            domain,
            retired: Vec::new(),
            hps: Vec::new(),
            collect_count: Cell::new(0)
        }
    }

    pub unsafe fn retire<T, P>(&mut self, ptr: *mut T)
    where
        T: Send,
        P: Pointer<T>
    {
        self.retired.push(unsafe {
            LocalRetired::new(self.domain, ptr, |ptr: *mut dyn Reclaim| {
                // Safety: the safety requirements of `from_raw` are the same as the ones to call
                // the deleter.
                let _ = P::from_raw(ptr as *mut T);
            })
        });
        let collect_count = self.collect_count.get().wrapping_add(1);
        self.collect_count.set(collect_count);

        if collect_count % Self::COUNTS_BETWEEN_COLLECT == 0 {
            self.do_reclamation();
        }
    }
    
    #[inline]
    fn do_reclamation(&mut self) {
        membarrier::heavy();
        let guarded_ptrs = self.domain.collect_guarded_ptrs();
        self.retired = self.retired
            .iter()
            .filter_map(|element| {
                if guarded_ptrs.contains(&(element.ptr as *mut u8)) {
                    Some(LocalRetired {
                        ptr: element.ptr,
                        deleter: element.deleter
                    })
                } else {
                    unsafe { (element.deleter)(element.ptr) };
                    None
                }
            })
            .collect();
    }

    pub unsafe fn retire_pp<T, P>(&mut self, ptr: *mut T)
    where
        T: Send,
        P: Pointer<T>
    {
        self.retired.push(unsafe {
            LocalRetired::new(self.domain, ptr, |ptr: *mut dyn Reclaim| {
                // Safety: the safety requirements of `from_raw` are the same as the ones to call
                // the deleter.
                let _ = P::from_raw(ptr as *mut T);
            })
        });
        let collect_count = self.collect_count.get().wrapping_add(1);
        self.collect_count.set(collect_count);

        if collect_count % Self::COUNTS_BETWEEN_COLLECT == 0 {
            self.do_reclamation_pp();
        }
    }

    #[inline]
    fn do_reclamation_pp(&mut self) {
        membarrier::heavy();
        drop(mem::replace(&mut self.hps, Vec::new()));

        let guarded_ptrs = self.domain.collect_guarded_ptrs();
        self.retired = self.retired
            .iter()
            .filter_map(|element| {
                if guarded_ptrs.contains(&(element.ptr as *mut u8)) {
                    Some(LocalRetired {
                        ptr: element.ptr,
                        deleter: element.deleter
                    })
                } else {
                    unsafe { (element.deleter)(element.ptr) };
                    None
                }
            })
            .collect();
    }
}

impl<'s, F> Drop for LocalBag<'s, F> {
    fn drop(&mut self) {
        while !self.retired.is_empty() {
            self.do_reclamation();
            core::hint::spin_loop();
        }
    }
}