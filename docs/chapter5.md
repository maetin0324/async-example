# JoinHandle を作る

前章の Executor は `VecDeque` を回し、`spawn` は `Future<Output = ()>` 専用だった。ここでは二つ更新する。ひとつはタスクキューの実装を `std::sync::mpsc::{Sender, Receiver}` に差し替えること。もうひとつは `spawn` の仕様そのものを改め、`Future<Output = T>` を受け取って、結果を外から `await` できる `JoinHandle<T>` を返すようにすることだ。

やることは難しくない。子タスクの完了結果を一時的にしまっておく共有スロットを用意し、親側からはそのスロットを覗くための `JoinHandle<T>` を `Future` として実装する。今回はまだ `RawWaker` を使わないので、`wake_by_ref()` は実質何もしない `Waker::noop()` のままにしておく。その代わり、`poll` が `Pending` だったタスクは**明示的にキューへ戻す**。これで各タスクはビジーループの巡回で少しずつ前へ進む。

## 共有スロットと JoinHandle
まずは共有スロットと `JoinHandle<T>` の実装を示す。
共有スロットは子タスクが結果を書き込むための `res: Option<T>` と、親タスクが `poll` 時に登録する `Waker` を持つ。
共有スロットは `Rc<RefCell<>>` で持ち、結果が入る `Option<T>` と `Waker` をそれぞれ `RefCell` で包んでいる。

子タスクが終わったら結果を `Some(v)` として格納する。親側（`JoinHandle`）は最初に `poll` されたとき、自分の `Waker` を登録しておく。
`spawn`時にwrapped_future内で結果が取れ次第、登録されている`Waker`を呼び出して親タスクを起こす仕組みになる。

```rust
use std::cell::RefCell;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

struct Slot<T> {
    res:   RefCell<Option<T>>,
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
```

## Executorの実装を変えてみる

今後の実装を容易にするために、`Sender/Receiver` でタスクキューを扱うようにする。`run` はキューからタスクを受け取り、`Waker::noop()` で `Context` を作って `poll` する。`Pending` なら**自分で**そのタスクをキューへ戻す。`Ready(())` になったら残数を減らす。`spawn` は結果スロットを用意し、子タスクを「完了時にスロットへ書き込む小さな async」でラップして投入し、呼び出し側へ `JoinHandle<T>` を返す。

```rust
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc::{Receiver, Sender};
use std::task::{Context, Poll, Waker};

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
        Self { rx, tx, remaining: 0.into() }
    }

    // 仕様を更新：結果を受け取る JoinHandle<T> を返す
    pub fn spawn<F, T>(&self, fut: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let slot = Rc::new(Slot { res: RefCell::new(None), waker: RefCell::new(None) });

        // 完了時にスロットへ書き込み
        let slot_for_task = slot.clone();
        let wrapped_future = async move {
            let r = fut.await;
            *slot_for_task.res.borrow_mut() = Some(r);
            if let Some(w) = slot_for_task.waker.borrow().as_ref() {
                // いまは noop だが、次章以降の wake 経路にそのまま繋がる
                w.wake_by_ref();
            }
        };

        // Output=() の Future に包んで投入
        self.tx.send(Task { future: Box::pin(wrapped) }).expect("spawn send failed");
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
```

## 使ってみる

chapter4 の `CountFuture` をそのまま使う。`spawn` は `JoinHandle<T>` を返すようになったので、結果の取得は `await` すればよい。合流用の親タスク自体も `spawn` で投入しておくと、Executor の中だけで完結する。

```rust
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
        count(10).await     // u64
    });

    let _ = ex.spawn(async move {
        let a = j1.await;   // String
        let b = j2.await;   // u64
        println!("joined: a={a}, b={b}");
    });

    ex.run();
    println!("all done");
}
```

各タスクは `poll` され、`Pending` なら自分でキューへ戻る。子タスクが完了すると結果がスロットに置かれ、合流側の `JoinHandle` は次の巡回で `Ready` を返す。`wake_by_ref()` 自体はまだ効いていないが、キューへの再投入をこちらで担保しているので問題なく前へ進む。次の章では `RawWaker` を導入し、`wake` をトリガにしてタスクをキューへ押し戻せるようにする。
