use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

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
        println!("First future count finished with {}", res1);
        let res2 = count(1 << 10).await;
        println!("Large count finished with {}", res2);
    };

    let fut2 = async {
        let res = count(10).await;
        println!("Second future count finished with {}", res);
    };

    ex.spawn(fut1);
    ex.spawn(fut2);
    ex.run(); 
    println!("all done");
}