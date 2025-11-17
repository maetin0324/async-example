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

struct SimpleExecutor {
    // Output=() に揃えて格納する（結果が必要なら内側で共有スロットに書く）
    queue: VecDeque<Pin<Box<dyn Future<Output = ()>>>>,
}

impl SimpleExecutor {
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
`spawn`関数で`Future`をキューに追加し、`run`関数でキューから`Future`を取り出して`poll`を呼び出している。`Poll::Pending`が返ってきた場合は再度キューの後ろに戻し、`Poll::Ready(())`が返ってきた場合は破棄する。

ここで`spawn`の実装を見てみると、受け入れるFutureが`Output=()`に固定されていることがわかる。`Output = T`のように任意の出力を期待するような`spawn`を実装するには、`Future`の出力を格納するための共有スロット、結果を外部から受け取るための`JoinHandle`などが必要になる。
`JoinHandle`の実装については、次章以降で詳しく解説する。

試しに「`poll` されるたびに一歩だけ進む」`CountFuture`を実装して、これを使ったasyncブロックを並行に動かしてみよう 

```rust
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
    let mut ex = SimpleExecutor::new();

    let fut1 = async {
        let res1 = count(5).await;
        println!("First count finished with {}", res1);
        let res2 = count(3).await;
        println!("Second count finished with {}", res2);
    };

    let fut2 = async {
        let res = count(1 << 20).await;
        println!("Large count finished with {}", res);
    };

    ex.spawn(fut1);
    ex.spawn(fut2);
    ex.run(); 
    println!("all done");
}
```

これを実行すると、以下のような出力が得られる。

```
CountFuture completed with count: 5
First future count finished with 5
CountFuture completed with count: 10
Second future count finished with 10
CountFuture completed with count: 1024
Large count finished with 1024
all done
```

`fut1`と`fut2`が並行に実行されかつ、asyncブロックの中で`await`が正しく機能していること、
`await`によりasyncブロックの中でFutureの返り値を利用できていることがわかる。

### なぜasyncブロックの中でawaitした結果が使えるのか
勘のいい読者であれば、「`spawn`関数の引数は`Future<Output=()>`なのに、なぜasyncブロックの中で`await`した結果を使えているのか？」と疑問に思うかもしれない。これは、`executor`がpollするFutureと、asyncブロックの中でawaitされるFutureが異なるためである。
`fut1`のasyncブロックは以下のような形のFutureに展開される。

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fut1State { Start, Await1, After1, Await2, Done }

struct Fut1 {
    state: Fut1State,
    current: Option<CountFuture>, // いま await 中のサブ Future を保持
    res1: u64,                    // 1個目の await の結果を保持
}

impl Fut1 {
    fn new() -> Self {
        Fut1 { state: Fut1State::Start, current: None, res1: 0 }
    }
}

impl Future for Fut1 {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // Fut1 も（この形なら）Unpin なので安全に可変参照を取れる
        let this = self.get_mut();

        loop {
            match this.state {
                Fut1State::Start => {
                    // let res1 = count(5).await; へ進む前に Future を作る
                    this.current = Some(count(5));
                    this.state = Fut1State::Await1;
                }

                Fut1State::Await1 => {
                    // res1 = current.await;
                    let fut = this.current.as_mut().expect("future missing");
                    // CountFuture は Unpin → Pin::new(&mut ...) で OK
                    match Pin::new(fut).poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(r1) => {
                            println!("First count finished with {}", r1);
                            this.res1 = r1;
                            this.current = None;            // 1個目を破棄
                            this.state = Fut1State::After1; // 次の文へ
                        }
                    }
                }

                Fut1State::After1 => {
                    // let res2 = count(3).await;
                    this.current = Some(count(3));
                    this.state = Fut1State::Await2;
                }

                Fut1State::Await2 => {
                    let fut = this.current.as_mut().expect("future missing");
                    match Pin::new(fut).poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(r2) => {
                            println!("Second count finished with {}", r2);
                            this.current = None;
                            this.state = Fut1State::Done;
                            return Poll::Ready(());
                        }
                    }
                }

                Fut1State::Done => {
                    // 冪等に Ready を返す（実際の生成物も概ね同様の振る舞い）
                    return Poll::Ready(());
                }
            }
            // state 遷移だけした場合は同じ poll 呼び出し内で続けて処理
        }
    }
}
```

前章でも説明した通り、asyncブロックは状態機械に展開される。`fut1`のFutureは`CountFuture`を内部に保持しつつ、その`poll`内で`CountFuture`の`poll`を呼び出している。このためexecutorが実行するpollと、asyncブロック内でawaitされるpollは別物となり、`CountFuture`の返り値をasyncブロック内で利用できる。

## どこまで使えるか

この Executor は非常にシンプルな実装ながら、基本的なasyncブロックはすべて動かすことができる。しかし、タスクの中断後に再開する仕組みがなく、
ビジーループでCPUを占有してしまうため、実用的ではない。次章以降で `Waker` を使った中断・再開の仕組みを追加していく。

