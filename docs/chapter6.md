# Wakerを作る

前章までは `Waker::noop()` を使い、`Pending` のタスクは自分でキューへ戻していた。ここではついに本物の `Waker` を実装し、`wake` を合図にタスク自身が**再スケジュール**されるようにする。これでビジーループの無駄が減り、外部スレッド（タイマやIO）からも自然にwakeできるようになる。

## `RawWaker` と vtable

`Waker` は内部に `RawWaker` を持ち、そのふるまいは `RawWakerVTable` の4つの関数ポインタで定義される。今回の設計ではTaskを `Arc<Task>` として保持しRawWakerのdataに保持し、`wake` が呼ばれたらチャネルに自分自身（`Arc<Task>`）を投げ返す。Executor 側はチャネルを受けてそのタスクを `poll` する。vtable の各関数は、`Arc<Task>` への生ポインタ（`*const ()`）を行き来させるだけだ。

```rust
// vtable 本体
static VTABLE: RawWakerVTable =
    RawWakerVTable::new(clone_waker, wake_task, wake_by_ref_task, drop_waker);

// SAFETY: data は Arc<Task> を into_raw したポインタ
unsafe fn clone_waker(data: *const ()) -> RawWaker {
    let arc: Arc<Task> = Arc::from_raw(data as *const Task);
    let cloned = arc.clone();
    // 元の参照は消費しない（所有権を戻す）
    let _ = Arc::into_raw(arc);
    RawWaker::new(Arc::into_raw(cloned) as *const (), &VTABLE)
}

unsafe fn wake_task(data: *const ()) {
    let arc: Arc<Task> = Arc::from_raw(data as *const Task);
    arc.wake();                // キューへ再投入
    // ここで arc は drop される（参照1つ消費）
}

unsafe fn wake_by_ref_task(data: *const ()) {
    let arc: Arc<Task> = Arc::from_raw(data as *const Task);
    arc.wake();                // 参照は保持したままにする
    let _ = Arc::into_raw(arc);
}

unsafe fn drop_waker(data: *const ()) {
    // clone で増やした参照をここで回収
    let _ = Arc::from_raw(data as *const Task);
}

// 生成ユーティリティ
impl Task {
    fn create_waker(self: &Arc<Self>) -> Waker {
        let raw = RawWaker::new(Arc::into_raw(self.clone()) as *const (), &VTABLE);
        unsafe { Waker::from_raw(raw) }
    }
}
```

生ポインタに `Arc` を出し入れする順序を間違えると二重解放やリークの温床になる。`wake_task` は「起床=再スケジュールの後に参照を1つ捨てる」実装で、`wake_by_ref_task` は「再スケジュールはするが参照は維持」する挙動にしておくと覚えやすい。

## Taskと Executor のループ

Taskは `Future<Output=()>` を保持し、wake時に自分をチャネルへ押し戻せればよい。`Pending` のときに**即座に戻す必要はない**。戻すべきタイミングは `wake` が鳴ったときだからだ。Executor はチャネルから受けたタスクを一つずつ `poll` し、`Ready` になったら残数を減らすだけでいい。

```rust
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

pub struct SimpleExecutor {
    rx: Receiver<Arc<Task>>,
    tx: Sender<Arc<Task>>,
    remaining: Cell<usize>,
}

impl SimpleExecutor {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self { rx, tx, remaining: 0.into() }
    }

    fn spawn<F>(&mut self, fut: F)
    where
        F: Future<Output = ()> + 'static,
    {
        let t = Arc::new(Task { future: RefCell::new(Box::pin(fut)), tx: self.tx.clone() });
        self.tx.send(t).expect("schedule failed");
        self.remaining += 1;
    }

    fn run(&mut self) {
        while self.remaining > 0 {
            let task = self.rx.recv().expect("task recv failed");
            let waker = task.create_waker();
            let mut cx = Context::from_waker(&waker);

            match task.poll(&mut cx) {
                Poll::Ready(()) => self.remaining -= 1,
                Poll::Pending => { /* ここでは戻さない。wake で戻る */ }
            }
        }
    }
}
```

この形にすると、`Pending` を返すたびに無条件でキューへ戻す必要がなくなる。`wake` が鳴ったときだけ再投入されるので、CPU の張り付きが一気に和らぐ。

## `JoinHandle<T>` をWakerにつなぐ

前章の `JoinHandle<T>` は、子タスクの結果を**共有スロット**に置いてから、親の `Waker` を鳴らすだけでよかった。`wake` を使う世界でもやることは同じだが、**順序**には注意する。必ず「値を書いてから `wake`」にする。逆順だと親が起きて `poll` してもスロットが空で、もう一度起こしてもらえずに止まってしまう。

```rust
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

// spawn 側（値→wake の順で）
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
            *slot_clone.res.borrow_mut() = Some(v);          // 先に書き込む
            if let Some(w) = slot_clone.waker.borrow().as_ref() {
                w.wake_by_ref();                        // それから起こす
            }
        };

        let t = Arc::new(Task { future: RefCell::new(Box::pin(wrapped_future)), tx: self.tx.clone() });
        self.tx.send(t).expect("spawn send failed");
        self.remaining.set(self.remaining.get() + 1);

        JoinHandle { slot }
    }

    pub fn run(&self) {
        while self.remaining.get() > 0 {
            let task = self.rx.recv().expect("task recv failed");
            let waker = task.to_waker();
            let mut cx = Context::from_waker(&waker);
            match task.poll(&mut cx) {
                Poll::Ready(()) => self.remaining.set(self.remaining.get() - 1),
                Poll::Pending => {} // wake で戻る
            }
        }
    }
```

これで、子が終わった瞬間に親のタスクがキューへ戻る（`wake` 経由）。親が `poll` するとスロットに値があり、`Ready` を返す。

`Waker::noop()` をやめ、`wake` をスケジューラの入口に据えるだけで、Executor のループは驚くほど静かになる。次章では、外部スレッドを用いたTimer Reactorの実装に挑戦する。
