use cordyceps::Linked;
use cordyceps::list::Links;
use std::marker::PhantomPinned;
use std::ptr::NonNull;
use std::task::Waker;

#[derive(Debug, Default)]
pub(crate) struct Waiter {
    links: Links<Waiter>,
    pub waker: Option<Waker>,
    _pin: PhantomPinned,
}

impl Waiter {
    pub fn add_waker(&mut self, val: Waker) {
        self.waker = Some(val);
    }
}

unsafe impl Linked<Links<Waiter>> for Waiter {
    type Handle = NonNull<Waiter>;
    unsafe fn from_ptr(ptr: NonNull<Self>) -> Self::Handle {
        // trivial since Self::Handle == NonNull<Waiter> is NonNull<Self> = NonNull<Waiter>
        ptr
    }

    fn into_ptr(r: Self::Handle) -> NonNull<Self> {
        // trivial since Self::Handle == NonNull<Waiter> is NonNull<Self> = NonNull<Waiter>
        r
    }

    unsafe fn links(target: NonNull<Self>) -> NonNull<Links<Waiter>> {
        let target = target.as_ptr();

        // offsets raw pointer to a field without creating a temporary reference
        let links = unsafe { &raw mut (*target).links };

        // Safety: Pointer we offset was NonNull, implying that the pointer produced by the offset
        // is also NonNull
        unsafe { NonNull::new_unchecked(links) }
    }
}
