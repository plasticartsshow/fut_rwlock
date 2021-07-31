# fut_rwlock

## fut_rwlock
`FutRwLock` is a read-write-lock that returns a Future-wrapped lock.
It is a wrapper around the `std` library synchronization primitive of the same name.
- The FutRwLock does not block the calling thread that requests a lock.
- _Any_ call to [read]FutRwLock::read/[write](FutRwLock::write) returns a Future
that must be `await`ed even if it resolves immediately.
- To _attempt_ to acquire an Option-wrapped read or write lock synchronously/immediately
use the [try_read_now](FutRwLock::try_read_now) or [try_write_now](FutRwLock::try_write_now) methods.
- It was made to be suitable for use in single-threaded wasm environments without running
into errors when the environment tries to block upon accessing a synchronization primitive.

License: Apache-2.0
