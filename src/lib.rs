//! # fut_rwlock 
//! `FutRwLock` returns a Future-wrapped locks to a read-write lock synchronization primitive.
//! It is a wrapper around the std library synchronization primitive [std::sync::RwLock].
//! - The FutRwLock does not block the calling thread that requests a lock.
//! - _Any_ call to [read]FutRwLock::read/[write](FutRwLock::write) returns an _asynchronous_ Future 
//! that must be `await`ed even if it resolves immediately. If a lock isn't awaited, it does nothing.
//! - To _attempt_ to acquire an Option-wrapped read or write lock synchronously/immediately
//! use the [try_read_now](FutRwLock::try_read_now) or [try_write_now](FutRwLock::try_write_now). 
//! _synchronous_ methods.
//! - It was made to be suitable for use in single-threaded wasm environments without running 
//! into errors when the environment tries to block upon accessing a synchronization primitive.
//! - Locks are alloted to callers in request order. 
use futures::{
  future::{
    Future,
    Shared,
  },
  FutureExt,
  lock,
};
use std::{
  fmt,
  ops::{
    Deref,
    DerefMut,
  },
  pin::Pin,
  sync::{
    Arc,
    atomic::{
      AtomicUsize,
      Ordering,
    },
    RwLock,
    RwLockReadGuard,
    RwLockWriteGuard,
    Weak,
  },
  task::{
    Context,
    Poll,
    Waker,
  },
};

/// A read-write lock that returns futures upon read/write requests.
pub struct FutRwLock<T: ?Sized> {
  inner: Arc<RwLock<T>>,
  reader_locks: Arc<AtomicUsize>,
  waker: Arc<RwLock<Option<Waker>>>,
  writer_awaiting_reader_locks_future: Arc<
    lock::Mutex<Option<Shared<WriterAwaitingReaderLocksFuture>>>
  >,
  writer_lock: Arc<lock::Mutex<()>>,
}

impl<T> FutRwLock<T> {
  /// Create a new, unlocked instance of FutRwLock
  pub fn new(t: T) -> FutRwLock<T> {
    FutRwLock{
      inner: Arc::new(RwLock::new(t)),
      reader_locks: Arc::new(AtomicUsize::new(0usize)),
      waker: Arc::new(RwLock::new(None)),
      writer_awaiting_reader_locks_future: Arc::new(lock::Mutex::new(None)),
      writer_lock: Arc::new(lock::Mutex::new(())),
    }
  }
}

impl <T> From<T> for FutRwLock<T> {
  fn from(t: T) -> FutRwLock<T> {
    FutRwLock::new(t)
  }
}

impl <T: Default> Default for FutRwLock<T> {
  fn default() -> FutRwLock<T> {
    FutRwLock::new(Default::default())
  }
}

impl <T: ?Sized + fmt::Debug> fmt::Debug for FutRwLock<T> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f
      .debug_struct("FutRwLock")
      .field("inner", &self.inner)
      .finish()
  }
}

impl <T: ?Sized> FutRwLock<T> {
  /// Is the underlying RwLock poisoned?
  pub fn is_poisoned(&self) -> bool { 
    self.inner.is_poisoned() 
  }
  
  /// Locks this FutRwLock with shared read access, returning a Future
  /// that resolves to a RwLockReadGuard when such access is available.
  /// If there is currently a writer holding the write lock, it waits until 
  /// the writers are done to return the requested read lock.
  pub async fn read(&self) -> FutRwLockReadGuard<'_, T> {
    // Async wait for writer lock
    let _writer_lock = self.writer_lock.lock().await;
    // Increase the reader lock count
    self.read_lock_increment();
    // Return wrapped guard
    FutRwLockReadGuard::new(
      self,
      self.inner
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner()),      
    ) 
  }
  
  /// Called by a reader lock holder to wake up 
  fn read_lock_decrement(&self) {
    // Remove this read lock indicator
    let _post_op_reader_lock_count = self
      .reader_locks
      .fetch_sub(1usize, Ordering::SeqCst);
    // Wake the reader locks future
    // Being unable to do this asynchronously introduces a potential race condiion:
    // 1. a reader releases, waking up the future
    // 2. before the future is done polling, another reader releases, waking again
    let _ = self
      .waker
      .read()
      .map(|waker_unlock_result| 
        waker_unlock_result
          .as_ref()
          .map(|waker|{
            // wake up the reader locks future by reference if it exists
            waker.wake_by_ref()
          })
      );
  }
  
  /// Called by a reader to increment lock and store the lock count for easy lookup
  fn read_lock_increment(&self) {
    let _ = self
      .reader_locks
      .fetch_add(1usize, Ordering::SeqCst);
  }
    
  /// Try to read now, returning an option-wrapped [read guard](FutRwLockReadGuard)
  pub fn try_read_now(&self) -> Option<FutRwLockReadGuard<'_, T>> {
    self
      .writer_lock
      .try_lock()
      .map(|_writer_lock_guard| {
        // Increase the reader lock count
        self.read_lock_increment();
        // Return wrapped guard
        FutRwLockReadGuard::new(
          self,
          self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
      })
  } 
  
  /// Try to write now, returning an option-wrapped [write guard](FutRwLockWriteGuard). 
  pub fn try_write_now(&self) -> Option<FutRwLockWriteGuard<'_, T>> {
    if let Some (writer_lock) = self
      .writer_lock
      .try_lock()
    {
      // The writer lock was available
      if self.reader_locks.load(Ordering::SeqCst) == 0 {
        // There were no readers
        Some(FutRwLockWriteGuard::new(
          self,
          self.inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
          writer_lock // keeps hold of writer_lock lock::MutexGuard
        ))
      } else {
        // There were readers waiting
        None
      }
    } else {
      // There were writers or readers waiting
      None
    }
  } 
  
  /// Locks this FutRwLock with unique write access, returning a Future
  /// that resolves to a RwLockReadGuard when such access is available.
  /// If there are currently readers holding read locks or another writer,
  /// holding the write lock, it waits until these locks are  done to 
  /// return the requested write lock.
  pub async fn write(&self) -> FutRwLockWriteGuard<'_, T> {
    // Async wait for writer lock FIRST
    let writer_lock = self.writer_lock.lock().await;
    // Async wait for reader locks SECOND 
    if self.reader_locks.load(Ordering::SeqCst) > 0 {
      // make a new future to wait for the reader locks
      let new_writer_awaiting_reader_locks_future = WriterAwaitingReaderLocksFuture{
        reader_locks: Arc::downgrade(&self.reader_locks),
        waker: Arc::downgrade(&self.waker),
      };
      let shared_future = new_writer_awaiting_reader_locks_future.shared();
      // let new_writer_awaiting_reader_future_ptr: *const WriterAwaitingReaderLocksFuture<T> = &new_writer_awaiting_reader_locks_future;
      // Store a reference to that future so it can be woken up
      self.writer_awaiting_reader_locks_future
        .lock()
        .await
        .replace(
          shared_future.clone()
        );
      // wait for the new reader locks future to finish (when there are zero reader locks)
      let _ = shared_future.await;
      // remove the reference to the reader locks future so it is no longer waked or polled
      *self.writer_awaiting_reader_locks_future.lock().await = None;
    } 
    // Return wrapped guard
    FutRwLockWriteGuard::new(
      self,
      self.inner
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()),
      writer_lock // keeps hold of writer_lock lock::MutexGuard
    )
  }
}

/// Async RwLock Read Guard
pub struct FutRwLockReadGuard<'a, T: ?Sized + 'a> {
  async_rwlock: &'a FutRwLock<T>,
  inner_read_guard: RwLockReadGuard<'a, T>,
  // lock_num: usize,
}

impl <T: ?Sized> Deref for FutRwLockReadGuard <'_, T> {
  type Target = T;
  fn deref(&self) -> &Self::Target {
    // Just delegate to the inner guard
    self.inner_read_guard.deref()
  }
}

impl <'a, T: ?Sized + 'a> Drop for FutRwLockReadGuard <'a, T>{
  fn drop(&mut self) {
    // Remove this reader's lock number
    self
      .async_rwlock
      .read_lock_decrement();
  }
}

impl <'a, T: 'a + ?Sized > FutRwLockReadGuard <'a, T> {
  /// Create wrapped guard from FutRwLock and lock_num identifier
  fn new(
    async_rwlock: &'a FutRwLock<T>,
    inner_read_guard: RwLockReadGuard<'a, T>,
    // lock_num: usize,
  ) -> FutRwLockReadGuard<'a, T> {
    FutRwLockReadGuard {
      async_rwlock,
      inner_read_guard,
      // lock_num,
    }
  }
}

/// Async RwLock Write Guard
#[allow(dead_code)]
pub struct FutRwLockWriteGuard<'a, T: ?Sized + 'a> {
  async_rwlock: &'a FutRwLock<T>,
  inner_write_guard: RwLockWriteGuard<'a, T>,
  writer_lock: lock::MutexGuard<'a, ()>,
}

impl <'a, T: 'a + ?Sized > FutRwLockWriteGuard <'a, T> {
  fn new(
    async_rwlock: &'a FutRwLock<T>,
    inner_write_guard: RwLockWriteGuard<'a, T>,
    writer_lock: lock::MutexGuard<'a, ()>,
  ) -> FutRwLockWriteGuard<'a, T> {
    FutRwLockWriteGuard {
      async_rwlock,
      inner_write_guard,
      writer_lock,
    }
  }
}

impl <'a, T:'a + ?Sized> Deref for FutRwLockWriteGuard <'a, T> {
  type Target = T;
  fn deref(&self) -> &Self::Target {
    // Just delegate to the inner guard
    self.inner_write_guard.deref()
  }
}

impl <'a, T:'a + ?Sized> DerefMut for FutRwLockWriteGuard <'a, T> {
  fn deref_mut(&mut self) -> &mut Self::Target {
    // Just delegate to the inner guard
    self.inner_write_guard.deref_mut()
  }
}

impl <T: fmt::Debug> fmt::Debug for FutRwLockWriteGuard<'_, T> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f
      .debug_struct("FutRwLockWriteGuard")
      .field("async_rwlock", &self.async_rwlock)
      .field("inner_write_guard", &self.inner_write_guard)
      .finish()
  }
}

impl <T: ?Sized + fmt::Display> fmt::Display for FutRwLockWriteGuard<'_, T> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    (*self.inner_write_guard).fmt(f)
  }
}

/// A Future that resolves when there are no readers on the parent FutRwLock
struct WriterAwaitingReaderLocksFuture {
  reader_locks: Weak<AtomicUsize>,
  waker: Weak<RwLock<Option<Waker>>>,
}

impl Future for WriterAwaitingReaderLocksFuture {
  type Output = Result<(), ()>;
  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    // let this_mut = self.get_mut();
    if let Some(reader_locks_atomicusize) = self.reader_locks.upgrade() {
      if let Some(waker_rwlock) = self.waker.upgrade() {
        // The waker weak pointer's inner value has not been dropped.
        if reader_locks_atomicusize.load(Ordering::SeqCst) > 0 {
          // There are readers
          waker_rwlock
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .replace(cx.waker().clone());
          // Yield still pending
          Poll::Pending
        } else {
          // There are no readers
          // Attempt to remove the waker to prevent undefined behavior
          let _ = waker_rwlock
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
          // Yield ready 
          Poll::Ready(Ok(()))
        }
      } else {
        // The waker weak pointer's inner value has been dropped, so this future 
        // should no longer be active
        Poll::Ready(Err(()))
        // Poll::Ready(Err(()))
      }
    } else {
      // The reader_locks weak pointer's inner value has been dropped, so this future 
      // should no longer be active
      Poll::Ready(Err(()))
    }
  }
}





#[cfg(test)]
mod tests {
  wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

  use super::*;
  // use core::future::Future;
  use futures::{
    join,
    future::{
      join_all,
    },
  };
  
  use rand::{RngCore,SeedableRng};
  use rand_chacha::ChaChaRng;
  
  use wasm_bindgen_test::*;
  // #[cfg(not(feature = "wasm32"))]
  // use std::time::{Instant, Duration};
  use instant::{Instant, Duration};
  
  
  // Test utility: Acquire some read locks and wait a while before dropping them.
  // Return approximate time of lock acquisition.
  async fn get_some_reads_then_wait(
    rwlock: &FutRwLock<()>,
    wait_ns: u64
  ) -> Instant {
    let _read_1 = rwlock.read().await;
    let _read_2 = rwlock.read().await;
    let reads_acquired_instant = Instant::now();
    sleep(wait_ns).await;
    reads_acquired_instant
  }
    
  // Test utility: Acquire a write locks and wait a while before dropping it
  // Return approximate time of lock acquisition.
  async fn get_write_then_wait (
    rwlock: &FutRwLock<()>,
    wait_ns: u64,
  ) -> Instant {
    let _write_0 = rwlock.write().await;
    let write_acquired_instant = Instant::now();
    sleep(wait_ns).await;
    write_acquired_instant
  }
  
  // Test utility: Acquire a write lock.
  // Return approximate time of lock acquisition.
  async fn get_write (
    rwlock: &FutRwLock<()>,
  ) -> Instant {
    let _write_0 = rwlock.write().await;
    let write_acquired_instant = Instant::now();
    write_acquired_instant
  }
  
  // Test utility: Acquire some read locks.
  // Return approximate time of lock acquisition.
  async fn get_some_reads(
    rwlock: &FutRwLock<()>,
  ) -> Instant {
    let _read_1 = rwlock.read().await;
    let _read_2 = rwlock.read().await;
    let reads_acquired_instant = Instant::now();
  
    reads_acquired_instant
  }
  
  // Test utility: put a caller to sleep for a given time 
  async fn sleep(t: u64) {
    let msg: String = format!("{}", t as i32);
    if cfg!(target_arch = "wasm32") {
      let fuff = js_function_promisify::Callback::new(move || {
        web_sys::console::warn_1(js_sys::JsString::from(msg.clone()).as_ref());
        Ok("".into())
      });
      let window = web_sys::window().expect("Must get window to sleep");
      let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
        fuff.as_function().as_ref(), t as i32,
      ).unwrap();
      let _ = fuff.await;
    } else {
      #[cfg(not(target_arch = "wasm32"))]
      let _ = tokio::time::sleep(Duration::from_millis(t)).await;
      std::println!("{}", msg );
    }  
  }
  
  // run a test in the target-appropriate executor
  #[cfg(not(target_arch = "wasm32"))]
  macro_rules! run_test {
    ($f:ident) => {
      {
        tokio_test::block_on($f())
      }
    };
  }
  #[cfg(target_arch = "wasm32")]
  macro_rules! run_test {
    ($f:ident) => {
      {
        wasm_bindgen_futures::spawn_local($f())
      }
    };
  }
  
  #[test]
  #[wasm_bindgen_test]
  fn write_mutates_inner_value() {
    let async_test = move || async {
      let p: FutRwLock<Option<u8>> = FutRwLock::new(None);
      let mut a = p.write().await;
      *a = Some(16);
      assert_eq!(*a, Some(16));
    };
    run_test!(async_test)
  }
  
  #[test]
  #[wasm_bindgen_test]
  fn write_awaits_reads() {
    let async_test = move || async {
      let p: FutRwLock<()> = FutRwLock::new(());
      let target_wait_ns = 10u64;
      let (reads_acquired_instant, write_acquired_instant) = join!(
        get_some_reads_then_wait(&p, target_wait_ns),
        get_write(&p),
      );
      assert!(
        write_acquired_instant >= reads_acquired_instant, 
        "Writes acquired after reads done"
      );
      let read_to_write_duration = write_acquired_instant
        .duration_since(reads_acquired_instant);
      
      assert!(
        read_to_write_duration >= Duration::from_nanos(target_wait_ns), 
        "Writes acquired after reads done by duration magnitude"
      );
    };
    run_test!(async_test)
  }
  
  #[test]
  #[wasm_bindgen_test]
  fn write_order () {
    let async_test = move || async {
      type TimeTy = u64;
      type ValTy = i8;
      type TestTy = Vec<ValTy>;
      type FutrwTy = FutRwLock<TestTy>;
      
      const TEST_DEPTH:usize = 32;
      const TEST_NS_STEP:TimeTy = 100;
      
      let p: FutrwTy = FutRwLock::new(Vec::with_capacity(TEST_DEPTH));
      {
        let mut rng = ChaChaRng::from_entropy();
        type Spec = (
          TimeTy,  //  delay, 
          usize, // index to write
          ValTy // value to write
        );
        // Test utility: Acquire a write lock then write by pushing a value 
        // Return approximate time of lock acquisition.
        async fn sleep_then_write_push (
          rwlock: &FutrwTy,
          ns: TimeTy,
          val: ValTy,
        ) -> Instant {
          sleep(ns).await;
          let write_acquire_attempted_instant = Instant::now();
          let mut w = rwlock.write().await;
          (*w).push(val.clone());
          write_acquire_attempted_instant
        }
        
        let mut specs: Vec<Spec> = Vec::with_capacity(TEST_DEPTH);
        for i in 0..TEST_DEPTH {
          specs.push((
            (TEST_DEPTH - i) as TimeTy * TEST_NS_STEP, // delay, highest to lowest
            i, // write index
            rng.next_u32() as ValTy, //write val 
          ));
        }
        
        let (futs, target) = specs.iter().fold(
          (vec![], vec![]),
          |(mut fs, mut ts), (ns, ind, val)| {
            fs.push(sleep_then_write_push(&p, *ns, *val));
            ts.push((ns, ind, val));
            (fs, ts)
          }
        );
        // target.sort_by(|(i,_,_), (j,_,_)| i.partial_cmp(j).unwrap());
        let target_vals : Vec<ValTy> = target.iter().map(|(_,_,v)| **v).collect();
        let write_acquired_attempt_instants: Vec<Instant> = join_all(futs).await;
        let mut targets_by_acquisition_attempt_instant: Vec<(Instant, ValTy)> = (0..TEST_DEPTH)
          .map(|i| (write_acquired_attempt_instants[i], target_vals[i]) ).collect();
        targets_by_acquisition_attempt_instant.sort_by(|(i,_), (j,_)| i.partial_cmp(j).unwrap());
        let target_vals : Vec<ValTy> = targets_by_acquisition_attempt_instant.iter().map(|(_,v)| *v).collect();
        let result = p.read().await;
        assert_eq!(
          *result,
          target_vals,
          "Write results in a buffer must match acquire attempt order: \n{:#?}",
          specs
        )        
      }
    };
    run_test!(async_test)
  }
  
  #[test]
  #[wasm_bindgen_test]
  fn write_then_drop_then_read() {
    let async_test = move || async {
      let p: FutRwLock<Option<u8>> = FutRwLock::new(None);
      {
        let mut a = p.write().await;
        *a = Some(16);
        drop(a);
        let b = *p.read().await;
        assert_eq!(b, Some(16), "read following write 1 must see new value");
      }
      let mut a = p.write().await;
      *a = Some(144);
      drop(a);
      let b = *p.read().await;
      assert_eq!(b, Some(144), "read following write 2 must see new value");
    };
    run_test!(async_test)
  }
  
  #[test]
  #[wasm_bindgen_test]
  fn write_prevents_try_write_now_and_try_read_now() {
    let async_test = move || async {
      let p: FutRwLock<Option<u8>> = FutRwLock::new(None);
      let _a = p.write().await;
      assert!(p.try_write_now().is_none(), "try_write_now returns None when write lock active");
      assert!(p.try_read_now().is_none(), "try_read_now returns None when write lock active");
      assert!(p.try_write_now().is_none(), "try_write_now returns None when write lock active");
      assert!(p.try_read_now().is_none(), "try_read_now returns None when write lock active");
      drop(_a);
      assert!(p.try_write_now().is_some(), "try_write_now returns Some when write lock active");
      assert!(p.try_read_now().is_some(), "try_read_now returns Some when write lock active");
    };
    run_test!(async_test)
  }
  
  #[test]
  #[wasm_bindgen_test]
  fn reads_await_write() {
    let async_test = move || async {
      let p: FutRwLock<()> = FutRwLock::new(());
      let target_wait_ns = 10u64;
      let (write_acquired_instant, reads_acquired_instant) = join!(
        get_write_then_wait(&p, target_wait_ns),
        get_some_reads(&p),
      );
      assert!(
        reads_acquired_instant >= write_acquired_instant, 
        "Read acquired after write done"
      );
      let write_to_read_duration = reads_acquired_instant
        .duration_since(write_acquired_instant);
      
      assert!(
        write_to_read_duration >= Duration::from_nanos(target_wait_ns), 
        "Reads acquired after write done by duration magnitude"
      );
    };
    run_test!(async_test)
  }
  
  
  #[test]
  #[wasm_bindgen_test]
  fn read_one() {
    let async_test = move || async {
      let p: FutRwLock<Option<u8>> = FutRwLock::new(Some(22));
      let a = p.read().await;
      assert_eq!(*a, Some(22));
    };
    run_test!(async_test)
  }
  
  #[test]
  #[wasm_bindgen_test]
  fn read_multiple() {
    let async_test = move || async {
      let p: FutRwLock<Option<u8>> = FutRwLock::new(Some(22));
      let a = p.read().await;
      let a_0 = p.read().await;
      assert_eq!(*a, Some(22));
      assert_eq!(*a_0, Some(22));
    };
    run_test!(async_test)
  }
  
  #[test]
  #[wasm_bindgen_test]
  fn read_prevents_try_write_now_and_allows_try_read_now() {
    let async_test = move || async {
      let p: FutRwLock<u16> = FutRwLock::new(1622);
      let read_0 = p.read().await;
      let read_1_opt = p.try_read_now();
      assert!(p.try_write_now().is_none(), "try_write_now returns None when read lock active");
      assert!(read_1_opt.is_some(), "try_read_now returns Some when read lock active");
      assert_eq!(*read_0, *read_1_opt.unwrap(), "Read and try read now point to same");
    };
    run_test!(async_test)
  }
  
  // #[test]
  // #[wasm_bindgen_test]
  // fn lock_propagates_poisoned_status () {
  //   use std::panic::{self, AssertUnwindSafe};
  // 
  //   let async_test = || async {
  //     type TestTy = FutRwLock<String>;
  //     let rwlock: TestTy = String::from("parenchyma").into();
  //     let mut write_lock = rwlock.write().await;
  //     (*write_lock).push_str(&" stonecell");
  //     drop(write_lock);
  //     assert_eq!(rwlock.is_poisoned(), false, "RwLock starts life unpoisoned.");
  //     panic::catch_unwind(AssertUnwindSafe( || async{
  //       let _read_lock = rwlock.read().await;
  //       panic!("Panics holding read lock");
  //     }));
  //     assert_eq!(rwlock.is_poisoned(), false, "RwLock remains unpoisoned following panic holding read.");
  //     panic::catch_unwind(AssertUnwindSafe( || async{
  //       let mut write_lock = rwlock.write().await;
  //       (*write_lock).push_str(&" stonecell");
  //       panic!("Panics holding read lock");
  //     }));
  //     std::println!("{}", *rwlock.read().await);
  //     assert_eq!(rwlock.is_poisoned(), true, "RwLock is poisoned following panic holding write.");
  //   };
  //   run_test!(async_test);
  // }
  
}
