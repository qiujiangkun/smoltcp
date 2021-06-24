#![allow(unsafe_code)]

use std::ops::Deref;


pub fn new_socket_buffer_channel() -> (SocketBufferSender, SocketBufferReceiver) {
    let (tx, rx) = crossbeam::channel::unbounded();
    (SocketBufferSender {
        tx
    }, SocketBufferReceiver {
        rx
    })
}
#[derive(Debug)]
pub struct SocketBufferReceiver {
    rx: crossbeam::channel::Receiver<Vec<u8>>,
}

#[derive(Debug)]
pub struct SocketBufferSender {
    tx: crossbeam::channel::Sender<Vec<u8>>,
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