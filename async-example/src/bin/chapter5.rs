use std::cell::{Cell, RefCell};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::task::{Context, Poll, Waker};

struct Slot<T> {
    res: RefCell<Option<T>>,
    waker: RefCell<Option<Waker>>,
}

pub struct JoinHandle<T> {
    slot: Rc<Slot<T>>,
}

impl<T> std::future::Future for JoinHandle<T> {
    type Output = T;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        *self.slot.waker.borrow_mut() = Some(cx.waker().clone());
        if let Some(v) = self.slot.res.borrow_mut().take() {
            Poll::Ready(v)
        } else {
            Poll::Pending
        }
    }
}

// タスク本体（単一スレッド想定なので Send/Sync 要件は持たない）
struct Task {
    future: Pin<Box<dyn Future<Output = ()> + 'static>>,
}

pub struct SimpleExecutor {
    rx: Receiver<Task>,
    tx: Sender<Task>,
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

    // 仕様を更新：結果を受け取る JoinHandle<T> を返す
    pub fn spawn<F, T>(&self, fut: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let slot = Rc::new(Slot {
            res: RefCell::new(None),
            waker: RefCell::new(None),
        });

        // 完了時にスロットへ書き込み
        let slot_for_task = slot.clone();
        let wrapped = async move {
            let r = fut.await;
            *slot_for_task.res.borrow_mut() = Some(r);
            if let Some(w) = slot_for_task.waker.borrow().as_ref() {
                // いまは noop だが、次章以降の wake 経路にそのまま繋がる
                w.wake_by_ref();
            }
        };

        // Output=() の Future に包んで投入
        self.tx
            .send(Task {
                future: Box::pin(wrapped),
            })
            .expect("spawn send failed");
        self.remaining.set(self.remaining.get() + 1);

        JoinHandle { slot }
    }

    pub fn run(&self) {
        // 残っているタスクが全て完了するまで回す
        while self.remaining.get() > 0 {
            let mut task = self.rx.recv().expect("task recv failed");
            let mut cx = Context::from_waker(Waker::noop());
            match task.future.as_mut().poll(&mut cx) {
                Poll::Ready(()) => {
                    self.remaining.set(self.remaining.get() - 1);
                }
                Poll::Pending => {
                    // まだなら自分で戻す（wake の代わり）
                    self.tx.send(task).expect("requeue failed");
                }
            }
            // 張り付き防止に少しだけ譲る
            std::thread::yield_now();
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
            println!("CountFuture completed: {}", self.count);
            return Poll::Ready(self.count);
        }
        // いまは noop だが、次章以降の wake で効くようになる
        cx.waker().wake_by_ref();
        self.get_mut().count += 1;
        Poll::Pending
    }
}

fn main() {
    let ex = SimpleExecutor::new();

    let j1 = ex.spawn(async {
        let n = count(5).await;
        format!("done-{n}") // String
    });

    let j2 = ex.spawn(async {
        count(10).await // u64
    });

    let _ = ex.spawn(async move {
        let a = j1.await; // String
        let b = j2.await; // u64
        println!("joined: a={a}, b={b}");
    });

    ex.run();
    println!("all done");
}
