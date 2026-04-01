# async_locks
### About
A learning project for async concurrency primitives in Rust. Recently changed waker management systems to a `std::sync::Mutex<HashMap<usize, Waker>>` instead of the queue I was using before. This creates issues of fairness as it's no longer FIFO ordering when pulling something off the queue, it's all in the hands of the hashing gods. Production libraries use intrusive linked lists in the parent referring to wakers embedded in the Future.

4/1/26 - Imported `cordyceps` and updated each waker manager. Lots of `unsafe` introduced in this one!

### Dependencies
Test suite uses `smol` runtime and `futures` crate for `join_all`.

### Building concurrency primitives from scratch for asynchronous Rust.

- **Mutex -** learned the race conditions that come up with `Waker` management. That's why I wrapped the waker queue in a `std::sync::Mutex`. It makes sense why `tokio` recommends using `std::sync::Mutex` when you don't cross `.await` points. The async Mutex comes with this internal performance hit.
- **RwLock -** tricky double-check logic required that was glossed over in the sync implementation that simply looped. Learned lots about proper memory ordering.
- **Condvar -** took me a while to figure out how to hold on to the `Mutex` between `.await` points. Had to work through some issues with stale wakers potentially getting saved in the queue. Lots of challenging 'move' issues in the `.poll()` method on this one.
- **Semaphore -** built using the Condvar and Mutex created above.


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
Testing `.notify_one()`
```
running 1 test
Waking one worker up!
[0] woken up!
[1] going to sleep
[3] going to sleep
[2] going to sleep
Waking one worker up!
[1] woken up!
Waking one worker up!
Waking one worker up!
[3] woken up!
[2] woken up!
test condvar::tests::condvar_notify_one ... ok
```
Testing `.notify_all()`
```
running 1 test
[0] going to sleep
[2] going to sleep
[3] going to sleep
[1] going to sleep
Waking everyone up!
[0] woken up!
[1] woken up!
[2] woken up!
[3] woken up!
test condvar::tests::condvar_notify_all ... ok
```
#### Semphore
Spawn 12 tasks competing for a semaphore with 3 permits. Each task acquires a permit,
records the current concurrency, yields (holding the permit), then releases.
Asserts that concurrency never exceeds 3 and that all 12 tasks complete.
```
running 1 test
[0] acquired permit, concurrency now: 1
[7] acquired permit, concurrency now: 1
[10] acquired permit, concurrency now: 2
[7] released permit, tasks completed: 1
[0] released permit, tasks completed: 2
[2] acquired permit, concurrency now: 2
[8] acquired permit, concurrency now: 3
[10] released permit, tasks completed: 3
[5] acquired permit, concurrency now: 3
[9] acquired permit, concurrency now: 3
[5] released permit, tasks completed: 5
[8] released permit, tasks completed: 6
[4] acquired permit, concurrency now: 3
[9] released permit, tasks completed: 7
[4] released permit, tasks completed: 8
[2] released permit, tasks completed: 4
[1] acquired permit, concurrency now: 2
[1] released permit, tasks completed: 9
[11] acquired permit, concurrency now: 2
[11] released permit, tasks completed: 10
[6] acquired permit, concurrency now: 2
[6] released permit, tasks completed: 11
[3] acquired permit, concurrency now: 1
[3] released permit, tasks completed: 12
peak concurrency: 3/3
test semaphore::tests::test_semaphore ... ok
```