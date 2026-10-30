



















pub mod signatures;

use std::alloc::GlobalAlloc;
use std::alloc::Layout;
use std::alloc::System;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;


const HEADER: usize = 8;

const GRANULE: usize = 16;


pub fn charged(size: usize) -> usize {
    return (size + HEADER).div_ceil(GRANULE) * GRANULE;
}

pub struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let now = LIVE.fetch_add(charged(l.size()), Relaxed) + charged(l.size());
            PEAK.fetch_max(now, Relaxed);
        }
        return p;
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(charged(l.size()), Relaxed);
        unsafe { System.dealloc(p, l) }
    }



    unsafe fn realloc(&self, p: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        let new = unsafe { Layout::from_size_align_unchecked(new_size, old.align()) };
        let q = unsafe { self.alloc(new) };
        if !q.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(p, q, old.size().min(new_size));
                self.dealloc(p, old);
            }
        }
        return q;
    }
}



pub struct Window {
    base: usize,
}

impl Window {
    pub fn new() -> Window {
        return Window {
            base: LIVE.load(Relaxed),
        };
    }



    pub fn measure<T>(&self, f: impl FnOnce() -> T) -> (T, usize) {
        PEAK.store(LIVE.load(Relaxed), Relaxed);
        let value = f();
        let peak = PEAK.load(Relaxed).saturating_sub(self.base);
        return (value, peak);
    }
}
