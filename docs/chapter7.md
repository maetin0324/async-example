# chapter7 タイマーを別スレッドで動かす

ここまでで、`wake` を合図にタスクが自分でキューへ戻る仕組みが出来た。次は、一定時間後に `wake` が飛ぶようにして、`sleep(d)` を自然に `await` できるようにする。処理の待機自体は別スレッドに任せ、期限が来たら対応する `Waker` を起こす。Executor 側は、チャネルに返ってきたタスクをいつも通り `poll` すればよい。

タイマーの核はひとつの共有状態とひとつのスレッドだけだ。共有状態は `Mutex<TimerState> + Condvar` で守り、`TimerState` は「期限の昇順リスト」「登録IDからの逆引き」「IDごとの Waker」を持つ。スレッドは次の期限まで条件変数で眠り、到達した分の Waker を取り出してロックを外してからまとめて `wake()` する。これだけで、`await timer.sleep(d)` は Executor の外から静かに再始動できるようになる。

## 実装

まず共有状態を定義する。期限の管理には `BTreeMap<Instant, Vec<u64>>` を使う。登録IDの採番とキャンセルのための逆引きテーブルも用意する。

```rust
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Condvar, Mutex};
use std::task::Waker;
use std::time::{Duration, Instant};
use std::{future::Future, pin::Pin, task::{Context, Poll}};

struct TimerState {
    deadlines: BTreeMap<Instant, Vec<u64>>,
    wakers:    HashMap<u64, Waker>,
    id_to_at:  HashMap<u64, Instant>,
    next_id:   u64,
    shutdown:  bool,
}

impl TimerState {
    fn new() -> Self {
        Self {
            deadlines: BTreeMap::new(),
            wakers:    HashMap::new(),
            id_to_at:  HashMap::new(),
            next_id:   1,
            shutdown:  false,
        }
    }
}

struct Inner {
    mu: Mutex<TimerState>,
    cv: Condvar,
}

impl Inner {
    fn new() -> Self { Self { mu: Mutex::new(TimerState::new()), cv: Condvar::new() } }
}
```

ランタイム側に渡すハンドルは `Timer` とし、`sleep` が `Sleep` という `Future<Output=()>` を返す。登録・更新・キャンセルは内部用メソッドで隠す。

```rust
#[derive(Clone)]
pub struct Timer {
    inner: Arc<Inner>,
}

impl Timer {
    pub fn sleep(&self, d: Duration) -> Sleep {
        Sleep { timer: self.clone(), deadline: Instant::now() + d, id: None }
    }

    fn register(&self, when: Instant, w: Waker) -> u64 {
        let mut st = self.inner.mu.lock().unwrap();
        let id = st.next_id; st.next_id += 1;

        st.wakers.insert(id, w);
        st.id_to_at.insert(id, when);
        st.deadlines.entry(when).or_default().push(id);

        self.inner.cv.notify_all(); // 最短期限が縮んだかもしれない
        id
    }

    fn update_waker(&self, id: u64, w: Waker) {
        let mut st = self.inner.mu.lock().unwrap();
        st.wakers.insert(id, w);
    }

    fn cancel(&self, id: u64) {
        let mut st = self.inner.mu.lock().unwrap();
        if let Some(when) = st.id_to_at.remove(&id) {
            if let Some(v) = st.deadlines.get_mut(&when) {
                if let Some(pos) = v.iter().position(|&x| x == id) {
                    v.swap_remove(pos);
                }
                if v.is_empty() { st.deadlines.remove(&when); }
            }
        }
        st.wakers.remove(&id);
    }
}
```

Reactor 本体はひとつのスレッドで動かす。次の期限まで待機し、到達した分の Waker を抜き出してからミューテックスを手放し、wakeを行う。`wake()` は `RawWaker` 経由で Executor のキュー投入に変換されるので、スレッド間でも問題なく動く。

```rust
pub struct TimerReactor {
    inner: Arc<Inner>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl TimerReactor {
    pub fn start() -> (Self, Timer) {
        let inner = Arc::new(Inner::new());
        let inner_for_thread = inner.clone();

        let worker = Some(std::thread::spawn(move || {
            loop {
                let mut st = inner_for_thread.mu.lock().unwrap();

                while !st.shutdown && st.deadlines.is_empty() {
                    st = inner_for_thread.cv.wait(st).unwrap();
                }
                if st.shutdown { break; }

                let now = Instant::now();
                let next = *st.deadlines.keys().next().unwrap();

                if next > now {
                    let dur = next - now;
                    let (st2, _) = inner_for_thread.cv.wait_timeout(st, dur).unwrap();
                    st = st2;
                    continue;
                }

                let mut to_wake = Vec::<Waker>::new();
                let ready: Vec<Instant> = st.deadlines.range(..=now).map(|(k, _)| *k).collect();
                for k in ready {
                    if let Some(ids) = st.deadlines.remove(&k) {
                        for id in ids {
                            st.id_to_at.remove(&id);
                            if let Some(w) = st.wakers.remove(&id) {
                                to_wake.push(w);
                            }
                        }
                    }
                }

                drop(st); // ロックはここで外す

                for w in to_wake { w.wake(); }
            }
        }));

        (Self { inner: inner.clone(), worker }, Timer { inner })
    }
}

impl Drop for TimerReactor {
    fn drop(&mut self) {
        let mut st = self.inner.mu.lock().unwrap();
        st.shutdown = true;
        self.inner.cv.notify_all();
        drop(st);
        if let Some(h) = self.worker.take() { let _ = h.join(); }
    }
}
```

`Timer::sleep` が返す `Sleep` は、期限に達していれば完了、まだなら登録または Waker の更新をして保留にする。到達前に捨てられた場合に備えて `Drop` でキャンセルもしておく。

```rust
pub struct Sleep {
    timer: Timer,
    deadline: Instant,
    id: Option<u64>,
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();

        if Instant::now() >= this.deadline {
            if let Some(id) = this.id.take() { this.timer.cancel(id); }
            return Poll::Ready(());
        }

        match this.id {
            None => {
                let id = this.timer.register(this.deadline, cx.waker().clone());
                this.id = Some(id);
            }
            Some(id) => {
                this.timer.update_waker(id, cx.waker().clone());
            }
        }
        Poll::Pending
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.timer.cancel(id);
        }
    }
}
```

## 使い方

Executor と `spawn` は `main.rs` の形をそのまま使う。`JoinHandle` は、子タスクが終了したら結果スロットに値を書き、その後で親の Waker を起こす。順番を逆にすると親が空読みして眠ったままになる。以下は `TimerReactor` を起動してから `sleep` を二つのタスクで待つ例。Executor には特別な変更は不要で、タイマースレッドからの `wake` で静かに進む。

```rust
fn main() {
    // 既存の executor
    let executor = SimpleExecutor::new();

    // タイマースレッド起動
    let (reactor, timer) = TimerReactor::start();

    // タスク1：50ms待ってから、さらに30ms待つ
    let t1 = {
        let timer = timer.clone();
        async move {
            println!("[t1] start");
            timer.sleep(Duration::from_millis(50)).await;
            println!("[t1] after 50ms");
            timer.sleep(Duration::from_millis(30)).await;
            println!("[t1] after +30ms (≈80ms total)");
        }
    };

    // タスク2：10msだけ待つ
    let t2 = {
        let timer = timer.clone();
        async move {
            println!("[t2] start");
            timer.sleep(Duration::from_millis(10)).await;
            println!("[t2] after 10ms");
        }
    };

    executor.spawn(t1);
    executor.spawn(t2);
    executor.run();     // ここで各タスクの waker がタイマースレッドから起こされる

    // Reactor の Drop でスレッドを停止・join
    drop(reactor);

    println!("all done");
}
```


`wake()` はミューテックスを外してから呼ぶ。`Waker` は `poll` のたびに変わり得るので `Sleep` は常に更新しておく。時間の比較には常に `Instant` を使う。これらを守っておけば、`sleep` の待機はタイマースレッドに完全に退避され、Executor は不要なビジーループをせずに進む。

