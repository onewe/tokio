#![cfg_attr(not(all(feature = "rt", feature = "net")), allow(dead_code))]

mod registration;
pub(crate) use registration::Registration;

mod scheduled_io;
use scheduled_io::ScheduledIo;

mod metrics;

use crate::io::interest::Interest;
use crate::io::ready::Ready;
use crate::runtime::driver;
use crate::util::slab::{self, Slab};
use crate::{loom::sync::RwLock, util::bit};

use metrics::IoDriverMetrics;

use std::fmt;
use std::io;
use std::time::Duration;

/// I/O driver, backed by Mio.
pub(crate) struct Driver {
    /// Tracks the number of times `turn` is called. It is safe for this to wrap
    /// as it is mostly used to determine when to call `compact()`.
    tick: u8,

    /// True when an event with the signal token is received
    signal_ready: bool,

    /// Reuse the `mio::Events` value across calls to poll.
    events: mio::Events,

    /// Primary slab handle containing the state for each resource registered
    /// with this driver.
    resources: Slab<ScheduledIo>,

    /// The system event queue.
    poll: mio::Poll,
}

/// A reference to an I/O driver.
pub(crate) struct Handle {
    /// Registers I/O resources.
    registry: mio::Registry,

    /// Allocates `ScheduledIo` handles when creating new resources.
    io_dispatch: RwLock<IoDispatcher>,

    /// Used to wake up the reactor from a call to `turn`.
    /// Not supported on Wasi due to lack of threading support.
    #[cfg(not(tokio_wasi))]
    waker: mio::Waker,

    pub(crate) metrics: IoDriverMetrics,
}

#[derive(Debug)]
pub(crate) struct ReadyEvent {
    tick: u8,
    pub(crate) ready: Ready,
    is_shutdown: bool,
}

struct IoDispatcher {
    allocator: slab::Allocator<ScheduledIo>,
    is_shutdown: bool,
}

#[derive(Debug, Eq, PartialEq, Clone, Copy)]
enum Direction {
    Read,
    Write,
}

enum Tick {
    Set(u8),
    Clear(u8),
}

// TODO: Don't use a fake token. Instead, reserve a slot entry for the wakeup
// token.
const TOKEN_WAKEUP: mio::Token = mio::Token(1 << 31);
const TOKEN_SIGNAL: mio::Token = mio::Token(1 + (1 << 31));

const ADDRESS: bit::Pack = bit::Pack::least_significant(24);

// Packs the generation value in the `readiness` field.
//
// The generation prevents a race condition where a slab slot is reused for a
// new socket while the I/O driver is about to apply a readiness event. The
// generation value is checked when setting new readiness. If the generation do
// not match, then the readiness event is discarded.
const GENERATION: bit::Pack = ADDRESS.then(7);

fn _assert_kinds() {
    fn _assert<T: Send + Sync>() {}

    _assert::<Handle>();
}

// ===== impl Driver =====

impl Driver {
    /// Creates a new event loop, returning any error that happened during the
    /// creation.
    pub(crate) fn new(nevents: usize) -> io::Result<(Driver, Handle)> {
        // 创建一个 poll 用于获取事件的底层驱动
        let poll = mio::Poll::new()?;
        // 注册 TOKEN 用于唤醒 线程 什么都不做的 事件
        #[cfg(not(tokio_wasi))]
        let waker = mio::Waker::new(poll.registry(), TOKEN_WAKEUP)?;
        
        let registry = poll.registry().try_clone()?;
        // 创建一个 slab , 用于分配 ScheduledIO , 这种比普通分配器效率更高
        // slab 只能用于获取对象
        let slab = Slab::new();
        // 创建一个 slab 分配器, 用于分配空间
        let allocator = slab.allocator();
        
        // 创建一个 driver
        let driver = Driver {
            // tick 用于计数, 到到默认值 255 释放 slab 空间
            tick: 0,
            // ready 信号, 表示已经接收到了 ready 信号
            signal_ready: false,
            // 事件集合, POLL 没拉取一次都会陷入 block 直到有 readiness 事件返回为止
            // 返回的事件会 回填到 events 集合里 可以通过迭代来访问它
            // nevents 默认为 1024
            events: mio::Events::with_capacity(nevents),
            poll,
            // slab 用于通过 事件中的 token 来获取 ScheduledIo
            resources: slab,
        };

        // 创建一个处理器
        let handle = Handle {
            // 用于注册事件
            registry,
            // dispatch 用于分发事件
            io_dispatch: RwLock::new(IoDispatcher::new(allocator)),
            // 此 waker 单纯的用于唤醒一个线程, 什么事也不干
            #[cfg(not(tokio_wasi))]
            waker,
            // 指标
            metrics: IoDriverMetrics::default(),
        };

        Ok((driver, handle))
    }

    pub(crate) fn park(&mut self, rt_handle: &driver::Handle) {
        let handle = rt_handle.io();
        self.turn(handle, None);
    }

    pub(crate) fn park_timeout(&mut self, rt_handle: &driver::Handle, duration: Duration) {
        let handle = rt_handle.io();
        self.turn(handle, Some(duration));
    }

    pub(crate) fn shutdown(&mut self, rt_handle: &driver::Handle) {
        let handle = rt_handle.io();

        if handle.shutdown() {
            self.resources.for_each(|io| {
                // If a task is waiting on the I/O resource, notify it that the
                // runtime is being shutdown. And shutdown will clear all wakers.
                io.shutdown();
            });
        }
    }

    fn turn(&mut self, handle: &Handle, max_wait: Option<Duration>) {
        // How often to call `compact()` on the resource slab
        // 设定 255 次归还内存给他 slab 
        const COMPACT_INTERVAL: u8 = 255;

        // compact 次数 + 1
        self.tick = self.tick.wrapping_add(1);

        // 判断 tick 是否达到 COMPACT_INTERVAL 也就是 255 次
        // 如果达到 归还内存给 slab
        if self.tick == COMPACT_INTERVAL {
            self.resources.compact()
        }

        let events = &mut self.events;

        // Block waiting for an event to happen, peeling out how many events
        // happened.
        // 调用底层 等待 readiness 的事件
        match self.poll.poll(events, max_wait) {
            Ok(_) => {}
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            #[cfg(tokio_wasi)]
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => {
                // In case of wasm32_wasi this error happens, when trying to poll without subscriptions
                // just return from the park, as there would be nothing, which wakes us up.
            }
            Err(e) => panic!("unexpected error when polling the I/O driver: {:?}", e),
        }

        // Process all the events that came in, dispatching appropriately
        let mut ready_count = 0;
        // 遍历所有 readiness 事件
        for event in events.iter() {
            // 获取事件中的 token
            let token = event.token();
            // 如果 token 等于 TOKEN_WAKEUP 什么事都不用做 单纯用于唤醒此线程
            if token == TOKEN_WAKEUP {
                // Nothing to do, the event is used to unblock the I/O driver
            } else if token == TOKEN_SIGNAL {
                // 如果 token 等于 TOKEN_SIGNAL 那么 signal_ready 设置为 true
                self.signal_ready = true;
            } else {
                // dispatch 时间
                Self::dispatch(
                    &mut self.resources,
                    self.tick,
                    token,
                    Ready::from_mio(event),
                );
                // ready count +1 用于设置指标
                ready_count += 1;
            }
        }

        handle.metrics.incr_ready_count_by(ready_count);
    }

    fn dispatch(resources: &mut Slab<ScheduledIo>, tick: u8, token: mio::Token, ready: Ready) {
        // 取 token 中的 右24位 为 slab 索引地址
        let addr = slab::Address::from_usize(ADDRESS.unpack(token.0));
        
        // 通过索引获取 slab 中的 ScheduledIO
        let io = match resources.get(addr) {
            Some(io) => io,
            None => return,
        };

        // 设置 ScheduledIO 为 readiness
        let res = io.set_readiness(Some(token.0), Tick::Set(tick), |curr| curr | ready);

        if res.is_err() {
            // token no longer valid!
            return;
        }

        io.wake(ready);
    }
}

impl fmt::Debug for Driver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Driver")
    }
}

impl Handle {
    /// Forces a reactor blocked in a call to `turn` to wakeup, or otherwise
    /// makes the next call to `turn` return immediately.
    ///
    /// This method is intended to be used in situations where a notification
    /// needs to otherwise be sent to the main reactor. If the reactor is
    /// currently blocked inside of `turn` then it will wake up and soon return
    /// after this method has been called. If the reactor is not currently
    /// blocked in `turn`, then the next call to `turn` will not block and
    /// return immediately.
    pub(crate) fn unpark(&self) {
        #[cfg(not(tokio_wasi))]
        self.waker.wake().expect("failed to wake I/O driver");
    }

    /// Registers an I/O resource with the reactor for a given `mio::Ready` state.
    ///
    /// The registration token is returned.
    pub(super) fn add_source(
        &self,
        source: &mut impl mio::event::Source,
        interest: Interest,
    ) -> io::Result<slab::Ref<ScheduledIo>> {
        let (address, shared) = self.allocate()?;

        let token = GENERATION.pack(shared.generation(), ADDRESS.pack(address.as_usize(), 0));

        self.registry
            .register(source, mio::Token(token), interest.to_mio())?;

        self.metrics.incr_fd_count();

        Ok(shared)
    }

    /// Deregisters an I/O resource from the reactor.
    pub(super) fn deregister_source(&self, source: &mut impl mio::event::Source) -> io::Result<()> {
        self.registry.deregister(source)?;

        self.metrics.dec_fd_count();

        Ok(())
    }

    /// shutdown the dispatcher.
    fn shutdown(&self) -> bool {
        let mut io = self.io_dispatch.write().unwrap();
        if io.is_shutdown {
            return false;
        }
        io.is_shutdown = true;
        true
    }

    fn allocate(&self) -> io::Result<(slab::Address, slab::Ref<ScheduledIo>)> {
        let io = self.io_dispatch.read().unwrap();
        if io.is_shutdown {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                crate::util::error::RUNTIME_SHUTTING_DOWN_ERROR,
            ));
        }
        io.allocator.allocate().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "reactor at max registered I/O resources",
            )
        })
    }
}

impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Handle")
    }
}

// ===== impl IoDispatcher =====

impl IoDispatcher {
    fn new(allocator: slab::Allocator<ScheduledIo>) -> Self {
        Self {
            allocator,
            is_shutdown: false,
        }
    }
}

impl Direction {
    pub(super) fn mask(self) -> Ready {
        match self {
            Direction::Read => Ready::READABLE | Ready::READ_CLOSED,
            Direction::Write => Ready::WRITABLE | Ready::WRITE_CLOSED,
        }
    }
}

// Signal handling
cfg_signal_internal_and_unix! {
    impl Handle {
        pub(crate) fn register_signal_receiver(&self, receiver: &mut mio::net::UnixStream) -> io::Result<()> {
            self.registry.register(receiver, TOKEN_SIGNAL, mio::Interest::READABLE)?;
            Ok(())
        }
    }

    impl Driver {
        pub(crate) fn consume_signal_ready(&mut self) -> bool {
            let ret = self.signal_ready;
            self.signal_ready = false;
            ret
        }
    }
}
