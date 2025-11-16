# 簡単なExecutorを作る

前章までで `Future` と `async/await` の仕組みを確認した。本章では、それらを実行する最小の Executor を用意する。やることは単純で、キューから `Future` を取り出して `poll`、`Pending` なら後ろに戻し、`Ready` なら破棄。これを完了するまで繰り返す。

## `Future::poll`を呼び出す
まずは非常にシンプルな Executor を作る。タスクキューを持ち、`spawn` で `Future` を登録し、`run` でキューが空になるまで `poll` を回す。
`Context`を作成するために`Waker`が必要だが、今回はまだ`wake`を使わないので、`Waker::noop()`で作成したダミーを使う。
```rust
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

struct BusyExecutor {
    // Output=() に揃えて格納する（結果が必要なら内側で共有スロットに書く）
    queue: VecDeque<Pin<Box<dyn Future<Output = ()>>>>,
}

impl BusyExecutor {
    fn new() -> Self {
        Self { queue: VecDeque::new() }
    }

    fn spawn<F>(&mut self, fut: F)
    where
        F: Future<Output = ()> + 'static,
    {
        self.queue.push_back(Box::pin(fut));
    }

    /// キューが空になるまでビジーループでpollし続ける
    fn run(&mut self) {
        let mut cx = Context::from_waker(Waker::noop());

        while let Some(mut fut) = self.queue.pop_front() {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(()) => {
                    // 完了：破棄
                }
                Poll::Pending => {
                    // まだ：後ろに戻す
                    self.queue.push_back(fut);
                }
            }

            if self.queue.is_empty() {
                // すべて完了
                break;
            }

            // ビジーループをほんの少し譲る（CPU張り付き抑制）
            std::thread::yield_now();
        }
    }
}
```

試しに「`poll` されるたびに一歩だけ進む」`Future` を二つ並べて走らせる。`await` は使っていないが、内部状態で中断を表現しているので、Executor 側は `poll` を回すだけで進む。

```rust
use std::task::{Context, Poll};
use std::pin::Pin;

/// pollが呼ばれるたびに1カウント進み、n回で完了するFuture
struct StepN {
    name: &'static str,
    cur: u32,
    n: u32,
}

impl StepN {
    fn new(name: &'static str, n: u32) -> Self { Self { name, cur: 0, n } }
}

impl Future for StepN {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        // Pin越しに可変参照を取り出す（今回自己参照は持っていないのでunsafeで剥がす）
        let this = unsafe { self.get_unchecked_mut() };

        if this.cur < this.n {
            this.cur += 1;
            println!("{} step = {}", this.name, this.cur);
            Poll::Pending
        } else {
            println!("{} done", this.name);
            Poll::Ready(())
        }
    }
}

fn main() {
    let mut ex = BusyExecutor::new();

    ex.spawn(StepN::new("A", 3));
    ex.spawn(StepN::new("B", 5));

    ex.run();
    println!("all done");
}
```

出力の雰囲気は次のとおり。

```
A step = 1
B step = 1
A step = 2
B step = 2
A step = 3
A done
B step = 3
B step = 4
B step = 5
B done
all done
```

## どこまで使えるか

この Executor は「`poll` を当て続ければ自力で進む」Future に対しては働く。純粋な計算や、内部カウンタで擬似的に“中断”しているだけの例なら十分だ。一方、I/O やタイマーのように外部イベントで進む Future は、`wake` による再実行のきっかけが必要になる。ここで作ったダミー Waker は何もしないので、その種の Future は永遠に `Pending` を返し、ループが空回りする。実用に向けるには、起床キューを持ち、`wake` を受けてタスクを再キューする仕組みが要る。これは次章で実装していく。

## もう少しだけ手入れ

ビジーループはCPUを占有しやすい。最小限の対策として `yield_now` を入れた。回し方を一定回数ごとに休ませる、あるいは「キューを一周して全件 `Pending` だったら短いスリープを入れる」程度の工夫でも体感は改善する。いずれにせよ、本章の目的は **`poll` を当てるだけで Future が前進する**という感覚を掴むことだ。wakeやリアクタ統合は後で段階的に足す。
