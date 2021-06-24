#![allow(unsafe_code)]

use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};

static CNT: AtomicUsize = AtomicUsize::new(1);

pub fn new_socket_buffer_channel() -> (SocketBufferSender, SocketBufferReceiver) {
    let (tx, rx) = crossbeam::channel::unbounded();
    (
        SocketBufferSender {
            tx,
            id: CNT.fetch_add(1, Ordering::AcqRel),
        },
        SocketBufferReceiver {
            rx,
            id: CNT.fetch_add(1, Ordering::AcqRel),
        },
    )
}
#[derive(Debug)]
pub struct SocketBufferReceiver {
    rx: crossbeam::channel::Receiver<Vec<u8>>,
    pub id: usize,
}

#[derive(Debug)]
pub struct SocketBufferSender {
    tx: crossbeam::channel::Sender<Vec<u8>>,
    pub id: usize,
}
impl Drop for SocketBufferSender {
    fn drop(&mut self) {
        net_debug!("SocketBufferSender dropped {}\n{:?}", self.id, backtrace::Backtrace::new());
    }
}
impl Drop for SocketBufferReceiver {
    fn drop(&mut self) {
        net_debug!("SocketBufferReceiver dropped {}\n{:?}", self.id, backtrace::Backtrace::new());
    }
}
impl Deref for SocketBufferReceiver {
    type Target = crossbeam::channel::Receiver<Vec<u8>>;

    fn deref(&self) -> &Self::Target {
        &self.rx
    }
}

impl Deref for SocketBufferSender {
    type Target = crossbeam::channel::Sender<Vec<u8>>;

    fn deref(&self) -> &Self::Target {
        &self.tx
    }
}
