use std::{
    cell::{Cell, RefCell},
    pin::Pin,
    rc::Rc,
    sync::{
        Arc,
        mpsc::{Receiver, Sender},
    },
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

static VTABLE: RawWakerVTable =
    RawWakerVTable::new(clone_waker, wake_task, wake_by_ref_task, drop_waker);

// SAFETY: data は Arc<Task> を into_raw したポインタ
unsafe fn clone_waker(data: *const ()) -> RawWaker {
    let arc: Arc<Task> = unsafe { Arc::from_raw(data as *const Task) };
    let cloned = arc.clone();
    // 元の参照は消費しない（所有権を戻す）
    let _ = Arc::into_raw(arc);
    RawWaker::new(Arc::into_raw(cloned) as *const (), &VTABLE)
}

unsafe fn wake_task(data: *const ()) {
    let arc: Arc<Task> = unsafe { Arc::from_raw(data as *const Task) };
    arc.wake(); // キューへ再投入
    // ここで arc は drop される（参照1つ消費）
}

unsafe fn wake_by_ref_task(data: *const ()) {
    unsafe {
        let arc: Arc<Task> = Arc::from_raw(data as *const Task);
        arc.wake(); // 参照は保持したままにする
        let _ = Arc::into_raw(arc);
    }
}

unsafe fn drop_waker(data: *const ()) {
    // clone で増やした参照をここで回収
    let _ = unsafe { Arc::from_raw(data as *const Task) };
}

// 生成ユーティリティ
impl Task {
    fn create_waker(self: &Arc<Self>) -> Waker {
        let raw = RawWaker::new(Arc::into_raw(self.clone()) as *const (), &VTABLE);
        unsafe { Waker::from_raw(raw) }
    }
}

struct Task {
    future: RefCell<Pin<Box<dyn Future<Output = ()>>>>,
    tx: Sender<Arc<Task>>,
}

impl Task {
    fn wake(self: &Arc<Self>) {
        // 起床 = 自分自身をキューへ返す
        let _ = self.tx.send(self.clone());
    }

    fn poll(&self, cx: &mut Context<'_>) -> Poll<()> {
        self.future.borrow_mut().as_mut().poll(cx)
    }
}

struct Slot<T> {
    res: RefCell<Option<T>>,
    waker: RefCell<Option<Waker>>,
}

pub struct JoinHandle<T> {
    slot: Rc<Slot<T>>,
}

impl<T> Future for JoinHandle<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        *self.slot.waker.borrow_mut() = Some(cx.waker().clone());
        if let Some(v) = self.slot.res.borrow_mut().take() {
            Poll::Ready(v)
        } else {
            Poll::Pending
        }
    }
}

pub struct SimpleExecutor {
    rx: Receiver<Arc<Task>>,
    tx: Sender<Arc<Task>>,
    remaining: Cell<usize>,
}

impl SimpleExecutor {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            rx,
            tx,
            remaining: 0.into(),
        }
    }

    pub fn spawn<F, T>(&self, fut: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let slot = Rc::new(Slot {
            res: RefCell::new(None),
            waker: RefCell::new(None),
        });

        let slot_clone = slot.clone();
        let wrapped_future = async move {
            let v = fut.await;
            *slot_clone.res.borrow_mut() = Some(v); // 先に書き込む
            if let Some(w) = slot_clone.waker.borrow().as_ref() {
                w.wake_by_ref(); // それから起こす
            }
        };

        let t = Arc::new(Task {
            future: RefCell::new(Box::pin(wrapped_future)),
            tx: self.tx.clone(),
        });
        self.tx.send(t).expect("spawn send failed");
        self.remaining.set(self.remaining.get() + 1);

        JoinHandle { slot }
    }

    fn run(&mut self) {
        while self.remaining.get() > 0 {
            let task = self.rx.recv().expect("task recv failed");
            let waker = task.create_waker();
            let mut cx = Context::from_waker(&waker);

            match task.poll(&mut cx) {
                Poll::Ready(()) => self.remaining.set(self.remaining.get() - 1),
                Poll::Pending => { /* ここでは戻さない。wake で戻る */ }
            }
        }
    }
}

struct CountFuture {
    init: u64,
    count: u64,
}
fn count(n: u64) -> CountFuture {
    CountFuture { init: n, count: 0 }
}
impl Future for CountFuture {
    type Output = u64;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u64> {
        if self.count >= self.init {
            return Poll::Ready(self.count);
        }
        cx.waker().wake_by_ref();
        self.get_mut().count += 1;
        Poll::Pending
    }
}

fn main() {
    let mut ex = SimpleExecutor::new();

    let j1 = ex.spawn(async { format!("done-{}", count(5).await) });
    let j2 = ex.spawn(async { count(10).await });

    let _ = ex.spawn(async move {
        let a = j1.await;
        let b = j2.await;
        println!("joined: a={a}, b={b}");
    });

    ex.run();
}
