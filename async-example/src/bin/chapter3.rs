use std::future::Future;

struct CountFuture {
	init: u64,
    count: u64,
}

impl Future for CountFuture {
    type Output = u64;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
		if self.count >= self.init {
			return std::task::Poll::Ready(self.count)
		}

		std::task::Poll::Pending
    }
}


fn main() {
}