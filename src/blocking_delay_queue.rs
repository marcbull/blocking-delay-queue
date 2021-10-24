use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::delay_item::Delayed;

type MinHeap<T> = BinaryHeap<Reverse<T>>;

pub struct BlockingDelayQueue<T>
{
    heap: Mutex<MinHeap<T>>,
    condvar: Condvar,
}

impl<T> BlockingDelayQueue<T>
    where T: Delayed + Ord
{
    pub fn new_unbounded() -> Self {
        BlockingDelayQueue {
            heap: Mutex::new(BinaryHeap::new()),
            condvar: Condvar::new(),
        }
    }

    pub fn new_with_capacity(capacity: usize) -> Self {
        if capacity == 0 {
            Self::new_unbounded()
        } else {
            BlockingDelayQueue {
                heap: Mutex::new(BinaryHeap::with_capacity(capacity)),
                condvar: Condvar::new(),
            }
        }
    }

    pub fn add(&self, e: T) {
        let mut heap = self.heap_mutex();
        if Self::can_accept_element(&heap) {
            heap.push(Reverse(e));
        } else {
            let cap = heap.capacity();
            let mut mutex = self
                .condvar
                .wait_while(heap, |h| h.len() >= cap)
                .expect("Queue lock poisoned");
            mutex.push(Reverse(e));
        }

        self.condvar.notify_one()
    }

    pub fn offer(&self, e: T, timeout: Duration) -> bool {
        let mut heap = self.heap_mutex();
        if Self::can_accept_element(&heap) {
            heap.push(Reverse(e));
            self.condvar.notify_one();
            true
        } else {
            let cap = heap.capacity();
            let mut mutex = self
                .condvar
                .wait_timeout_while(heap, timeout, |heap| heap.len() >= cap)
                .expect("Queue lock poisoned");
            if mutex.1.timed_out() {
                false
            } else {
                mutex.0.push(Reverse(e));
                self.condvar.notify_one();
                true
            }
        }
    }

    pub fn take(&self) -> T {
        match self.wait_for_element(Duration::ZERO) {
            Some(e) => e,
            _ => unreachable!()
        }
    }

    pub fn poll(&self, timeout: Duration) -> Option<T> {
        self.wait_for_element(timeout)
    }

    fn size(&self) -> usize {
        self.heap_mutex().len()
    }

    fn clear(&self) {
        self.heap_mutex().clear();
    }

    fn heap_mutex(&self) -> MutexGuard<'_, MinHeap<T>> {
        self.heap.lock().expect("Queue lock poisoned")
    }

    fn wait_for_element(&self, timeout: Duration) -> Option<T> {
        let heap = self.heap_mutex();
        if let Some(e) = heap.peek() {
            let current_time = Instant::now();
            if Self::is_expired(&e.0) {
                // remove head
                Some(self.pop_and_notify(heap))
            } else {
                let delay = match timeout {
                    // delay until head expiration
                    Duration::ZERO => e.0.delay() - current_time,
                    // delay until timeout
                    _ => timeout
                };

                // wait until condition is satisfied respecting timeout (delay)
                match self.wait_for_element_with_timeout(heap, delay) {
                    // available element within timeout
                    Some(e) => Some(e),
                    _ => {
                        match timeout {
                            // unreachable code but let's keep it in case Q.remove(e) is added
                            Duration::ZERO => self.wait_for_element(timeout),
                            // when within timeout there is no available element
                            _ => None
                        }
                    }
                }
            }
        } else {
            match timeout {
                Duration::ZERO => {
                    let guard = self
                        .condvar
                        .wait_while(heap, Self::wait_condition())
                        .expect("Condvar lock poisoned");

                    Some(self.pop_and_notify(guard))
                }
                _ => self.wait_for_element_with_timeout(heap, timeout)
            }
        }
    }

    fn wait_for_element_with_timeout(&self, heap: MutexGuard<MinHeap<T>>, timeout: Duration) -> Option<T> {
        let (heap, res) = self
            .condvar
            .wait_timeout_while(heap, timeout, Self::wait_condition())
            .expect("Condvar lock poisoned");

        match res.timed_out() {
            true => None,
            _ => Some(self.pop_and_notify(heap))
        }
    }

    fn pop_and_notify(&self, mut mutex: MutexGuard<MinHeap<T>>) -> T {
        let e = mutex.pop().unwrap().0;
        self.condvar.notify_one();
        e
    }

    fn can_accept_element(m: &MutexGuard<MinHeap<T>>) -> bool {
        if m.capacity() == 0 {
            true
        } else {
            m.len() < m.capacity()
        }
    }

    fn wait_condition() -> impl Fn(&mut MinHeap<T>) -> bool {
        move |heap: &mut MinHeap<T>| {
            heap.peek()
                .map_or(true, |item| item.0.delay() > Instant::now())
        }
    }

    fn is_expired(e: &T) -> bool {
        e.delay() <= Instant::now()
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Sub;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use crate::blocking_delay_queue::BlockingDelayQueue;
    use crate::delay_item::DelayItem;

    type MeasuredResult<T> = (T, Duration);

    #[test]
    fn should_put_and_take_ordered() {
        let queue = BlockingDelayQueue::new_unbounded();
        queue.add(DelayItem::new(1, Instant::now()));
        queue.add(DelayItem::new(2, Instant::now()));

        assert_eq!(1, queue.take().data);
        assert_eq!(2, queue.take().data);
        assert_eq!(0, queue.size());
    }

    #[test]
    fn should_put_and_take_delayed_items() {
        let queue = BlockingDelayQueue::new_unbounded();
        queue.add(DelayItem::new(1, Instant::now() + Duration::from_millis(10)));
        queue.add(DelayItem::new(2, Instant::now()));

        assert_eq!(2, queue.take().data);
        assert_eq!(1, queue.take().data);
        assert_eq!(0, queue.size());
    }

    #[test]
    fn should_block_until_item_is_available_take() {
        let queue = Arc::new(BlockingDelayQueue::new_unbounded());
        let queue_rc = queue.clone();
        let handle = thread::spawn(move || queue_rc.take());
        queue.add(DelayItem::new(1, Instant::now() + Duration::from_millis(50)));
        let res = handle.join().unwrap().data;
        assert_eq!(1, res);
        assert_eq!(0, queue.size());
    }

    #[test]
    fn should_block_until_item_is_available_poll() {
        let queue = Arc::new(BlockingDelayQueue::new_unbounded());
        let queue_rc = queue.clone();
        let handle = thread::spawn(move || queue_rc.poll(Duration::from_millis(10)));
        queue.add(DelayItem::new(1, Instant::now() + Duration::from_millis(5)));
        let res = handle.join().unwrap().unwrap().data;
        assert_eq!(1, res);
        assert_eq!(0, queue.size());
    }

    #[test]
    fn should_block_until_item_can_be_added() {
        let queue = Arc::new(BlockingDelayQueue::new_with_capacity(1));
        queue.add(DelayItem::new(1, Instant::now() + Duration::from_millis(50)));
        let queue_rc = queue.clone();
        let handle = thread::spawn(move || queue_rc.add(DelayItem::new(2, Instant::now())));
        assert_eq!(1, queue.take().data);
        handle.join().unwrap();
        assert_eq!(1, queue.size());
        assert_eq!(2, queue.take().data);
    }

    #[test]
    fn should_timeout_if_element_cant_be_added() {
        let queue = BlockingDelayQueue::new_with_capacity(1);
        let accepted = queue.offer(DelayItem::new(1, Instant::now()), Duration::from_millis(5));
        // fill capacity
        assert!(accepted);

        // q is full here should block until timeout without inserting
        let timeout = Duration::from_millis(50);
        let res = measure_time_millis(|| queue.offer(DelayItem::new(2, Instant::now()), timeout));
        // element is not accepted - timeout occurred
        assert!(!res.0);
        // timeout is respected with some delta
        assert!(res.1 >= timeout && res.1.sub(timeout) <= Duration::from_millis(10));

        assert_eq!(1, queue.take().data);
        assert_eq!(0, queue.size());
    }

    #[test]
    fn should_timeout_if_element_cant_be_polled() {
        let queue: BlockingDelayQueue<DelayItem<u8>> = BlockingDelayQueue::new_unbounded();
        let e = queue.poll(Duration::from_millis(5));
        assert_eq!(None, e);
    }

    fn measure_time_millis<T>(f: impl Fn() -> T) -> MeasuredResult<T> {
        let now = Instant::now();
        let t = f();
        (t, now.elapsed())
    }
}