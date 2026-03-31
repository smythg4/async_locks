# async_locks
### About
Test suite uses `smol` runtime.

### Building concurrency primitives from scratch for asynchronous Rust.

- Mutex - learned the race conditions that come up with `Waker` management. That's why I wrapped the waker queue in a sync Mutex.
- RwLock - tricky double-check logic required that was glossed over in the sync implementation that simply looped
- Condvar - took me a while to figure out how to hold on to the `Mutex` between `.await` points. My `counter` check might be redundant.