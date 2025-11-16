use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

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

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // Pin越しに可変参照を取り出す（今回自己参照は持っていないのでunsafeで剥がす）
        let this = unsafe { self.get_unchecked_mut() };

        if this.cur < this.n {
            this.cur += 1;
            println!("{} step = {}", this.name, this.cur);
            cx.waker().wake_by_ref(); // BusyExecutorでは意味がない
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