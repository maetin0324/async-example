# Futureは「中断」を表す

chapter2 で見たとおり、Rust の非同期の核は `Future` の `poll` だけだ。ここでは Future を「いつか値が手に入る入れ物」ではなく、**実行を中断し、必要になったら再度進めるためのインターフェース**として扱う。

`poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Output>` は「今すぐ進められるところまで進める」ための呼び出しで、進めた結果が完了なら `Poll::Ready`、いまは止まるべきなら `Poll::Pending` を返す。非同期の見かけ上の連続性はあるが、`Pending` の間はユーザーコードは走っていない。

## `poll` の意味

`poll` はブロックしない。完了していなければ速やかに `Pending` を返す。完了したら一度だけ `Ready(x)` を返し、それ以降は `poll` されない前提で設計する。`Context` の中身（再開の合図に使われる口）は後章(Wakerに関して)で扱うので、ここでは型だけ見ておけばよい。

## async/await は状態機械になる

`async fn` や `async { ... }` は、コンパイル時に **generator**（再開可能な関数）の形に下ろされ、その上に `Future` 実装が自動生成される。イメージをつかむために、簡単な例を擬似コードで見ておく。

```rust
// 元コード（高級表現）
async fn add_then_double(x: u32) -> u32 {
    let y = add_async(x, 1).await;
    double_async(y).await
}
```

これがコンパイルされると、だいたい次のような「状態＋局所変数を抱えた構造体」と「`poll` で状態遷移する実装」に展開される（実際の生成物は匿名型）。

```rust
// 擬似コード（概念図）
use std::pin::Pin;
use std::task::{Context, Poll};

enum State<F1, F2> {
    Start { x: u32 },
    Await1 { fut1: F1 },                 // add_async(x, 1) の Future を保持
    Await2 { y: u32, fut2: F2 },         // y を保持しつつ double_async(y) を待つ
    Done,
}

struct AddThenDouble<F1, F2> {
    state: State<F1, F2>,
}

impl<F1, F2> std::future::Future for AddThenDouble<F1, F2>
where
    F1: std::future::Future<Output = u32>,
    F2: std::future::Future<Output = u32>,
{
    type Output = u32;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
        // Pin については後述。ここでは可変参照を取り出すだけにしておく。
        let this = unsafe { self.get_unchecked_mut() };

        loop {
            match &mut this.state {
                State::Start { x } => {
                    let fut1 = add_async(*x, 1);          // サブFutureを生成
                    this.state = State::Await1 { fut1 };
                }

                State::Await1 { fut1 } => match Pin::new(fut1).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(y) => {
                        let fut2 = double_async(y);
                        this.state = State::Await2 { y, fut2 };
                    }
                },

                State::Await2 { fut2, .. } => match Pin::new(fut2).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(z) => {
                        this.state = State::Done;
                        return Poll::Ready(z);
                    }
                },

                State::Done => panic!("polled after completion"),
            }
        }
    }
}
```

要点は二つ、一つは、**`await` のたびに「いったん止まる可能性のある場所」＝中断点が生まれる**こと。もう一つは、**中断点をまたぐ局所変数（ここでは `y`）は構造体のフィールドに退避される**こと。これにより、`poll` は「現在の状態に応じて必要なSub-Future を `poll` し、必要なら `Pending` を返し、先へ進めるなら状態遷移してループを続ける」という形になる。

この「内部で `loop { match state { ... } }` しながら進める」構造が、いわゆる **generator をラップした Future** の正体だ。Rust のコンパイラは実際には `std::ops::Generator` 相当の低レベル表現に下ろしたうえで、これと同種の `Future::poll` を合成している。

## `async` ブロックも同じ

`let f = async { work1().await; work2().await; 42 };` のような `async` ブロックも、上と同様に匿名の `Future` 型（状態＋`poll`）に展開される。`async move` はキャプチャした値をその匿名型のフィールドにムーブするだけで、基本構造は変わらない。

## Pin はなぜ必要か

`poll` のレシーバが `Pin<&mut Self>` なのは、**Future の配置を固定する**ためだ。`await` をまたいで保持される局所変数や、サブ Future への参照が内部に入り込むと、オブジェクト自体をムーブした瞬間に参照が壊れる可能性がある。そこで `Box::pin` 等でヒープ上に固定してから `poll` するのが約束になっている。

```rust
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

struct StepN { step: u32, n: u32 }

impl Future for StepN {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        // 配置固定されたまま内部状態だけを更新する
        let this = unsafe { self.get_unchecked_mut() };
        if this.step < this.n {
            this.step += 1;
            Poll::Pending           // まだ途中（本来は後で再び poll される）
        } else {
            Poll::Ready(())
        }
    }
}

// 実行側のイメージ
let mut fut = Box::pin(StepN { step: 0, n: 3 });
// Executor が何度か poll して進める（具体的な再開の仕組みは後章）
```

自己参照を含む可能性があるので、固定後に「入れ物ごと差し替える」ような操作は避ける。複雑な型でフィールドごとの可変借用が必要なら `pin-project` 系のマクロを使うと安全に書ける。

## Futureとは

Future は「中断点をはさみながら少しずつ前に進む」ための型で、`async/await` はその Future を**状態機械として自動生成**する仕組みだ。`await` はサブ Future を `poll` して、終わっていなければいったん `Pending` を返し、終わっていれば次の状態へ進める。再開の具体的な仕掛けは後の章（Executor / Reactor の実装）で扱う。ここでは、**`poll` は非ブロッキング、`await` は状態遷移点、`Pin` は配置固定**――この三点を掴んでおけば十分だ。
