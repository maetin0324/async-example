#　概要
このプロジェクトはRustの非同期ランタイムがどのような仕組みになっているかを理解するために、
小規模かつ十分な機能を持ったRust非同期ランタイムを実装することを目指す

#　Rust言語の非同期API
Rust言語の一つの特徴として、非同期処理のスケジューラが言語非依存となっており、差し替え可能なことが挙げられる。
これはRust言語が組み込みプログラムなどにも使用されることを想定して、async/await構文やWakeのAPIそれ自体は提供する一方でasync/await構文によって生成される非同期タスクのスケジューリング戦略はOSやデバイスごとによって自由度が高く設計できるようにすることを目指したためである

Rust言語それ自体に組み込まれている非同期処理のAPIはシンプルで、`Future`と`Waker`(とその内部のRawWaker, RawWakerVTable)の二つのみである。

```rust
pub trait Future {
    type Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>;
}

pub struct Waker {
    waker: RawWaker,
}

pub struct RawWaker {
    data: *const (),
    vtable: &'static RawWakerVTable,
}

pub struct RawWakerVTable {
    clone: unsafe fn(*const ()) -> RawWaker,
    wake: unsafe fn(*const ()),
    wake_by_ref: unsafe fn(*const ()),
    drop: unsafe fn(*const ()),
}
```

`Future`は実行の**中断・yield**を、`Waker`は**中断されたタスクの再実行**を行うという極めて簡潔かつ綺麗な抽象化がなされている。

いきなりそれぞれの定義を見ても理解するのは難しいと思うので、これ以降のchapterでは`Future`, `Waker`それぞれの意味と使い方を解説していく。