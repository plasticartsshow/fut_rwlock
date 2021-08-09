[![Workflow Status](https://github.com/plasticartsshow/fut_rwlock/workflows/Rust/badge.svg)](https://github.com/plasticartsshow/fut_rwlock/actions?query=workflow%3A%22Rust%22)
![Maintenance](https://img.shields.io/badge/maintenance-experimental-blue.svg)

# fut_rwlock

## fut_rwlock
`FutRwLock` returns a Future-wrapped locks to a read-write lock synchronization primitive.
It is a wrapper around the std library synchronization primitive [std::sync::RwLock].
- The FutRwLock does not block the calling thread that requests a lock.
- _Any_ call to [read]FutRwLock::read/[write](FutRwLock::write) returns an _asynchronous_ Future
that must be `await`ed even if it resolves immediately. If a lock isn't awaited, it does nothing.
- To _attempt_ to acquire an Option-wrapped read or write lock synchronously/immediately
use the [try_read_now](FutRwLock::try_read_now) or [try_write_now](FutRwLock::try_write_now).
_synchronous_ methods.
- It was made to be suitable for use in single-threaded wasm environments without running
into errors when the environment tries to block upon accessing a synchronization primitive.
- Locks are alloted to callers in request order.

License: Apache-2.0
