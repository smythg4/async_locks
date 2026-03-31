# async_locks
### About
Test suite uses `smol` runtime. `crossbeam-queue::SegQueue` is used as a lock-free queue in Condvar wakers.

### Building concurrency primitives from scratch for asynchronous Rust.

- *Mutex -* learned the race conditions that come up with `Waker` management. That's why I wrapped the waker queue in a sync Mutex. It makes sense why `tokio` recommends using `std::sync::Mutex` when you don't cross `.await` points. The async Mutex comes with this performance hit.
- *RwLock -* tricky double-check logic required that was glossed over in the sync implementation that simply looped. Learned lots about proper memory
ordering.
- *Condvar -* took me a while to figure out how to hold on to the `Mutex` between `.await` points. Had to work through some issues with stale wakers potentially getting
saved in the queue. Funky `slot` solution and elaborate `.drop` logic solves it.

### Test Output

#### Mutex
Spin up a few threads with copies of the executor on them. Spawn 5 tasks each assigned to increment a counter 5 times.
```
running 1 test
[0] counter = 1
[2] counter = 2
[3] counter = 3
[2] counter = 4
[0] counter = 5
[3] counter = 6
[3] counter = 7
[1] counter = 8
[3] counter = 9
[0] counter = 10
[1] counter = 11
[2] counter = 12
[3] counter = 13
[4] counter = 14
[0] counter = 15
[1] counter = 16
[2] counter = 17
[task 3] finished
[4] counter = 18
[4] counter = 19
[4] counter = 20
[4] counter = 21
[1] counter = 22
[task 4] finished
[2] counter = 23
[0] counter = 24
[task 2] finished
[task 0] finished
[1] counter = 25
[task 1] finished
final counter value: 25
test mutex::tests::test_async_mutex_contended ... ok
```

#### RwLock
Spin up a few threads with copies of the executor on them. Spawn 3 writer tasks and 5 reader tasks. Each worker does 3 reads or writes.
```
running 1 test
[W0] wrote, value is now 1
[W1] wrote, value is now 2
[W2] wrote, value is now 3
[W0] wrote, value is now 4
[W1] wrote, value is now 5
[W1] wrote, value is now 6
[W0] wrote, value is now 7
[R4] read value: 7
[R2] read value: 7
[R3] read value: 7
[R4] read value: 7
[R2] read value: 7
[R3] read value: 7
[R4] read value: 7
[R2] read value: 7
[R3] read value: 7
[R1] read value: 7
[R1] read value: 7
[R1] read value: 7
[R0] read value: 7
[W2] wrote, value is now 8
[R0] read value: 8
[W2] wrote, value is now 9
[R0] read value: 9
test rwlock::tests::test_async_rwlock ... ok
```
#### Condvar
Spin up a few threads with copies of the executor on them. Spawn 4 workers that will run through a loop twice each time calling `.wait()` on the `Condvar`. Then spawn a task that loops through six times, alternating between calling `.notify_one()` and `.notify_all()`.
```
running 1 test
[3] going to sleep (1)...
[0] going to sleep (1)...
[1] going to sleep (1)...
[2] going to sleep (1)...
Wake one up!
[3] waking up (1)...
[3] going to sleep (2)...
Wake 'em all up!
[3] waking up (2)...
[3] All done!
[1] waking up (1)...
[1] going to sleep (2)...
[2] waking up (1)...
[2] going to sleep (2)...
[0] waking up (1)...
[0] going to sleep (2)...
Wake one up!
[1] waking up (2)...
[1] All done!
Wake 'em all up!
[2] waking up (2)...
[2] All done!
[0] waking up (2)...
[0] All done!
Wake one up!
Wake 'em all up!
test condvar::tests::condvar_basics ... ok
```