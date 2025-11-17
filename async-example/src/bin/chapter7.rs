use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::sync::mpsc::{Receiver, Sender};
use std::task::{Context, RawWaker, RawWakerVTable};
use std::time::{Duration, Instant};
use std::{
    cell::RefCell,
    future::Future,
    rc::Rc,
    task::{Poll, Waker},
};

static VTABLE: RawWakerVTable =
    RawWakerVTable::new(clone_waker, wake_task, wake_by_ref_task, drop_waker);

// SAFETY: data is a pointer to Arc<Task>
unsafe fn clone_waker(data: *const ()) -> RawWaker {
    let task_rc: Arc<Task> = unsafe { Arc::from_raw(data as *const Task) };
    let task_rc_clone = task_rc.clone();
    let _ = Arc::into_raw(task_rc); // drop guard
    RawWaker::new(Arc::into_raw(task_rc_clone) as *const (), &VTABLE)
}

// SAFETY: data is a pointer to Arc<Task>
unsafe fn wake_task(data: *const ()) {
    let task_rc: Arc<Task> = unsafe { Arc::from_raw(data as *const Task) };
    task_rc.wake();
    // Rc pointer drop here
}

// SAFETY: data is a pointer to Arc<Task>
unsafe fn wake_by_ref_task(data: *const ()) {
    let task_rc: Arc<Task> = unsafe { Arc::from_raw(data as *const Task) };
    task_rc.wake();
    // drop guard
    let _ = Arc::into_raw(task_rc);
}

// SAFETY: data is a pointer to Arc<Task>
unsafe fn drop_waker(data: *const ()) {
    let _ = unsafe { Arc::from_raw(data as *const Task) };
    // Rc pointer drop here
}

struct SimpleExecutor {
    task_queue: Receiver<Arc<Task>>,
    task_sender: Sender<Arc<Task>>,
    task_remaining: Cell<usize>,
}

impl SimpleExecutor {
    fn new() -> SimpleExecutor {
        let (task_sender, task_queue) = std::sync::mpsc::channel();
        SimpleExecutor { task_queue, task_sender, task_remaining: 0.into() }
    }

    fn spawn<F, T>(&self, f: F) -> JoinHandle<T> 
    where 
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let slot: Rc<RefCell<Slot<T>>> = Rc::new(RefCell::new(Slot {
            res_slot: RefCell::new(None),
            waker_slot: None,
        }));

        let slot_clone = slot.clone();

        let wrapped_future = async move {
            let res = f.await;
            let binding = slot_clone.borrow();
            binding.waker_slot.as_ref().map(|w| w.wake_by_ref());
            *binding.res_slot.borrow_mut() = Some(res);
        };
        let wrapped_future = RefCell::new(Box::pin(wrapped_future));

        let task = Arc::new(Task {
            future: wrapped_future,
            sender: self.task_sender.clone(),
        });

        self.task_sender.send(task).expect("spawn failed");
        self.task_remaining.set(self.task_remaining.get() + 1);

        JoinHandle { slot }
    }

    fn run(&self) {
        // task_remainingがzeroになるまで進める
        while self.task_remaining.get() > 0 {
            let task = self.task_queue.recv().expect("task recv failed");
            let waker = task.create_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            match task.poll(&mut cx) {
                Poll::Ready(_) => {
                    self.task_remaining.set(self.task_remaining.get() - 1);
                }
                Poll::Pending => {
                    // do nothing
                }
            }
        }

    }
}

struct Task {
    future: RefCell<Pin<Box<dyn Future<Output = ()>>>>,
    sender: Sender<Arc<Task>>,
}

impl Task {
    fn create_waker(self: &Arc<Self>) -> Waker {
        let rc_ptr = Arc::into_raw(self.clone()) as *const ();
        let raw = RawWaker::new(rc_ptr, &VTABLE);
        unsafe { Waker::from_raw(raw) }
    }

    fn wake(self: &Arc<Self>) {
        self.sender.send(self.clone()).expect("wake failed");
    }

    fn poll(&self, cx: &mut std::task::Context<'_>) -> Poll<()> {
        self.future.borrow_mut().as_mut().poll(cx)
    }
}

struct Slot<T> {
    res_slot: RefCell<Option<T>>,
    waker_slot: Option<Waker>,
}

struct JoinHandle<T> {
    slot: Rc<RefCell<Slot<T>>>,
}

impl<T> Future for JoinHandle<T> {
    type Output = T;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let mut bindings = self.slot.borrow_mut();
        if bindings.waker_slot.is_none() {
            bindings.waker_slot.replace(cx.waker().clone());
        }

        let mut res = bindings.res_slot.borrow_mut();
        if res.is_some() {
            Poll::Ready(res.take().unwrap())
        } else {
            Poll::Pending
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

/// タイマースレッドが使う共有状態
struct TimerState {
    // 期限 -> 登録IDのリスト
    deadlines: BTreeMap<Instant, Vec<u64>>,
    // 登録ID -> Waker
    wakers: HashMap<u64, Waker>,
    // 登録ID -> 期限（キャンセル時に逆引き）
    id_to_deadline: HashMap<u64, Instant>,
    // ID採番
    next_id: u64,
    // 終了フラグ
    shutdown: bool,
}

impl TimerState {
    fn new() -> Self {
        Self {
            deadlines: BTreeMap::new(),
            wakers: HashMap::new(),
            id_to_deadline: HashMap::new(),
            next_id: 1,
            shutdown: false,
        }
    }
}

struct Inner {
    mu: Mutex<TimerState>,
    cv: Condvar,
}

impl Inner {
    fn new() -> Self {
        Self { mu: Mutex::new(TimerState::new()), cv: Condvar::new() }
    }
}

/// ランタイム側に渡すハンドル（クローン可、'static可）
#[derive(Clone)]
pub struct Timer {
    inner: Arc<Inner>,
}

impl Timer {
    /// `duration` 後に Ready になる `Sleep` を返す
    pub fn sleep(&self, duration: Duration) -> Sleep {
        Sleep {
            timer: self.clone(),
            deadline: Instant::now() + duration,
            id: None,
        }
    }

    /// （内部用）新規登録。登録IDを返す。
    fn register(&self, when: Instant, waker: Waker) -> u64 {
        let mut st = self.inner.mu.lock().unwrap();
        let id = st.next_id;
        st.next_id += 1;

        st.wakers.insert(id, waker);
        st.id_to_deadline.insert(id, when);
        st.deadlines.entry(when).or_default().push(id);

        // 次のタイムアウトが早まったかもしれないので通知
        self.inner.cv.notify_all();
        id
    }

    /// （内部用）同じ登録IDの Waker を更新（wake先が変わる可能性に備える）
    fn update_waker(&self, id: u64, waker: Waker) {
        let mut st = self.inner.mu.lock().unwrap();
        st.wakers.insert(id, waker);
        // 次のタイムアウトに影響しないので通知は不要
    }

    /// （内部用）キャンセル（Drop時など）
    fn cancel(&self, id: u64) {
        let mut st = self.inner.mu.lock().unwrap();
        if let Some(deadline) = st.id_to_deadline.remove(&id) {
            if let Some(v) = st.deadlines.get_mut(&deadline) {
                if let Some(pos) = v.iter().position(|&x| x == id) {
                    v.swap_remove(pos);
                }
                if v.is_empty() {
                    st.deadlines.remove(&deadline);
                }
            }
        }
        st.wakers.remove(&id);
    }
}

/// Reactor 本体（スレッド所有者）
pub struct TimerReactor {
    inner: Arc<Inner>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl TimerReactor {
    /// スレッドを起動して (reactor, timer-handle) を返す
    pub fn start() -> (Self, Timer) {
        let inner = Arc::new(Inner::new());
        let inner_for_thread = inner.clone();

        let worker = Some(std::thread::spawn(move || {
            loop {
                // 1) 次に起きるべき期限を決める
                let mut st = inner_for_thread.mu.lock().unwrap();

                // a) 仕事がない間/期限が来ていない間は待つ
                while !st.shutdown && st.deadlines.is_empty() {
                    st = inner_for_thread.cv.wait(st).unwrap();
                }
                if st.shutdown {
                    break;
                }

                let now = Instant::now();
                let next_deadline = *st.deadlines.keys().next().unwrap();

                if next_deadline > now {
                    // 次の期限まで待機（スプリアスも考慮）
                    let dur = next_deadline - now;
                    let (st2, _timeout_res) = inner_for_thread.cv.wait_timeout(st, dur).unwrap();
                    st = st2;
                    // ここでループ先頭に戻り、期限を再評価
                    continue;
                }

                // b) 期限が到来したものを全て回収
                let mut fire_ids = Vec::new();
                // 到来済みキーを一旦収集してから remove（借用制約のため）
                let ready_keys: Vec<Instant> =
                    st.deadlines.range(..=now).map(|(k, _)| *k).collect();

                for k in ready_keys {
                    if let Some(ids) = st.deadlines.remove(&k) {
                        for id in ids {
                            if st.id_to_deadline.remove(&id).is_some() {
                                if let Some(w) = st.wakers.remove(&id) {
                                    // ロックを離してから wake したいので一旦保存
                                    fire_ids.push(w);
                                }
                            }
                        }
                    }
                }

                drop(st); // ロック解除してから wake（デッドロック回避）

                // c) 対応するタスクを起床（あなたの RawWaker がスレッド間 wake に対応）
                for w in fire_ids {
                    w.wake();
                }
                // 次のループへ
            }
        }));

        let reactor = TimerReactor { inner: inner.clone(), worker };
        let timer = Timer { inner };

        (reactor, timer)
    }
}

impl Drop for TimerReactor {
    fn drop(&mut self) {
        let mut st = self.inner.mu.lock().unwrap();
        st.shutdown = true;
        self.inner.cv.notify_all();
        drop(st);
        self.worker.take().unwrap().join().expect("timer thread join failed");
    }
}

/// `Timer::sleep` が返す Future
pub struct Sleep {
    timer: Timer,
    deadline: Instant,
    id: Option<u64>,
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();

        // 期限に到達していたら完了
        if Instant::now() >= this.deadline {
            // 既に登録済みならクリーンアップ
            if let Some(id) = this.id.take() {
                this.timer.cancel(id);
            }
            return Poll::Ready(());
        }

        // まだなら登録/更新して Pending
        match this.id {
            None => {
                let id = this.timer.register(this.deadline, cx.waker().clone());
                this.id = Some(id);
            }
            Some(id) => {
                // Waker が変わる可能性があるので最新に更新
                this.timer.update_waker(id, cx.waker().clone());
            }
        }

        Poll::Pending
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            // キャンセル（未到達で drop されたケース）
            self.timer.cancel(id);
        }
    }
}



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
