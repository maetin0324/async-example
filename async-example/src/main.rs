use std::cell::Cell;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::task::{RawWaker, RawWakerVTable};
use std::{
    cell::RefCell,
    future::Future,
    rc::Rc,
    task::{Poll, Waker},
};

static VTABLE: RawWakerVTable =
    RawWakerVTable::new(clone_waker, wake_task, wake_by_ref_task, drop_waker);

// SAFETY: data is a pointer to Arc<Task>
unsafe fn clone_waker(data: *const ()) -> RawWaker {
    let task_rc: Arc<Task> = unsafe { Arc::from_raw(data as *const Task) };
    let task_rc_clone = task_rc.clone();
    RawWaker::new(Arc::into_raw(task_rc_clone) as *const (), &VTABLE)
}

// SAFETY: data is a pointer to Arc<Task>
unsafe fn wake_task(data: *const ()) {
    let task_rc: Arc<Task> = unsafe { Arc::from_raw(data as *const Task) };
    task_rc.wake();
    // Rc pointer drop here
}

// SAFETY: data is a pointer to Arc<Task>
unsafe fn wake_by_ref_task(data: *const ()) {
    let task_rc: Arc<Task> = unsafe { Arc::from_raw(data as *const Task) };
    task_rc.wake();
    // drop guard
    let _ = Arc::into_raw(task_rc);
}

// SAFETY: data is a pointer to Arc<Task>
unsafe fn drop_waker(data: *const ()) {
    let _ = unsafe { Arc::from_raw(data as *const Task) };
    // Rc pointer drop here
}

struct SimpleExecutor {
    task_queue: Receiver<Arc<Task>>,
    task_sender: Sender<Arc<Task>>,
    task_remaining: Cell<usize>,
}

impl SimpleExecutor {
    fn new() -> SimpleExecutor {
        let (task_sender, task_queue) = std::sync::mpsc::channel();
        SimpleExecutor { task_queue, task_sender, task_remaining: 0.into() }
    }

    fn spawn<F, T>(&self, f: F) -> JoinHandle<T> 
    where 
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let slot: Rc<RefCell<Slot<T>>> = Rc::new(RefCell::new(Slot {
            res_slot: RefCell::new(None),
            waker_slot: None,
        }));

        let slot_clone = slot.clone();

        let wrapped_future = async move {
            let res = f.await;
            let binding = slot_clone.borrow();
            binding.waker_slot.as_ref().map(|w| w.wake_by_ref());
            *binding.res_slot.borrow_mut() = Some(res);
        };
        let wrapped_future = RefCell::new(Box::pin(wrapped_future));

        let task = Arc::new(Task {
            future: wrapped_future,
            sender: self.task_sender.clone(),
        });

        self.task_sender.send(task).expect("spawn failed");
        self.task_remaining.set(self.task_remaining.get() + 1);

        JoinHandle { slot }
    }

    fn run(&self) {
        // task_remainingがzeroになるまで進める
        while self.task_remaining.get() > 0 {
            let task = self.task_queue.recv().expect("task recv failed");
            let waker = task.create_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            match task.poll(&mut cx) {
                Poll::Ready(_) => {
                    self.task_remaining.set(self.task_remaining.get() - 1);
                }
                Poll::Pending => {
                    // do nothing
                }
            }
        }

    }
}

struct Task {
    future: RefCell<Pin<Box<dyn Future<Output = ()>>>>,
    sender: Sender<Arc<Task>>,
}

impl Task {
    fn create_waker(self: &Arc<Self>) -> Waker {
        let rc_ptr = Arc::into_raw(self.clone()) as *const ();
        let raw = RawWaker::new(rc_ptr, &VTABLE);
        unsafe { Waker::from_raw(raw) }
    }

    fn wake(self: &Arc<Self>) {
        self.sender.send(self.clone()).expect("wake failed");
    }

    fn poll(&self, cx: &mut std::task::Context<'_>) -> Poll<()> {
        self.future.borrow_mut().as_mut().poll(cx)
    }
}

struct Slot<T> {
    res_slot: RefCell<Option<T>>,
    waker_slot: Option<Waker>,
}

struct JoinHandle<T> {
    slot: Rc<RefCell<Slot<T>>>,
}

impl<T> Future for JoinHandle<T> {
    type Output = T;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let mut bindings = self.slot.borrow_mut();
        if bindings.waker_slot.is_none() {
            bindings.waker_slot.replace(cx.waker().clone());
        }

        let mut res = bindings.res_slot.borrow_mut();
        if res.is_some() {
            Poll::Ready(res.take().unwrap())
        } else {
            Poll::Pending
        }
    }
}


struct CountFuture {
	init: u64,
    count: u64,
}

fn count(init: u64) -> CountFuture {
    CountFuture {
        init,
        count: 0,
    }
}

impl Future for CountFuture {
    type Output = u64;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
		if self.count >= self.init {
            println!("CountFuture completed with count: {}", self.count);
			return std::task::Poll::Ready(self.count)
		}

        cx.waker().wake_by_ref(); // 自分自身を再度スケジュールする
        self.get_mut().count += 1; // カウントを進める

		std::task::Poll::Pending
    }
}


fn main() {
    let executor = SimpleExecutor::new();
    executor.spawn(count(5));
    executor.spawn(count(3));
    executor.spawn(count(1 << 20));
    executor.run();
}
