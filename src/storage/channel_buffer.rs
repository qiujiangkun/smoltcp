use std::mem::MaybeUninit;
use std::sync::{Arc, Weak};

#[repr(C)]
pub struct FixedBuffer<const SIZE: usize> {
    len: usize,
    content: [u8; SIZE],
}

#[allow(unsafe_code)]
impl<const SIZE: usize> FixedBuffer<SIZE> {
    pub fn new() -> FixedBuffer<SIZE> {
        unsafe {
            FixedBuffer {
                content: MaybeUninit::uninit().assume_init(),
                len: 0,
            }
        }
    }

    pub fn push_slice(&mut self, s: &[u8]) {
        let l = self.len;
        let newl = l + s.len();
        let dest = &mut self.content[l..newl];
        dest.copy_from_slice(s);
        self.len = newl;
    }

    pub fn try_push_slice(&mut self, s: &[u8]) -> usize {
        let begin = self.len;
        let min_l = std::cmp::min(s.len(), SIZE - self.len);
        let new_l = begin + min_l;
        self.content[begin..new_l].copy_from_slice(&s[..min_l]);
        self.len = new_l;
        // println!("Written {}", new_l);
        min_l
    }

    pub fn copy_from(x: impl AsRef<[u8]>) -> Self {
        let x = x.as_ref();
        assert!(
            x.len() <= SIZE,
            "String(len {})is longer than current FixedBuffer<{}>",
            x.len(),
            SIZE
        );
        let mut this = FixedBuffer::new();
        this.push_slice(x);
        this
    }
}

impl<const SIZE: usize> AsRef<[u8]> for FixedBuffer<SIZE> {
    fn as_ref(&self) -> &[u8] {
        &self.content[..self.len]
    }
}

impl<const SIZE: usize> Default for FixedBuffer<SIZE> {
    fn default() -> Self {
        FixedBuffer::new()
    }
}

pub type ChannelBufferContent = shared_arena::ArenaBox<FixedBuffer<1600>>;

pub fn channel_buffer_pair() -> (ChannelBufferSender, ChannelBufferReceiver) {
    let queue = Arc::new(crossbeam::queue::ArrayQueue::new(128));
    let weak = Arc::downgrade(&queue);
    (ChannelBufferSender::new(queue), ChannelBufferReceiver::new(weak))
}

#[derive(Debug)]
pub struct ChannelBufferSender {
    tx: Arc<crossbeam::queue::ArrayQueue<ChannelBufferContent>>,
}

impl ChannelBufferSender {
    pub fn new(tx: Arc<crossbeam::queue::ArrayQueue<ChannelBufferContent>>) -> Self {
        Self {
            tx
        }
    }
    pub fn send(&self, content: ChannelBufferContent) -> Result<(), ChannelBufferContent> {
        self.tx.push(content)
    }
    pub fn is_full(&self) -> bool {
        self.tx.is_full()
    }
}

#[derive(Debug)]
pub struct ChannelBufferReceiver {
    rx: Weak<crossbeam::queue::ArrayQueue<ChannelBufferContent>>,
}

impl ChannelBufferReceiver {
    pub fn new(rx: Weak<crossbeam::queue::ArrayQueue<ChannelBufferContent>>) -> Self {
        Self {
            rx
        }
    }
    pub fn try_recv(&self) -> Result<ChannelBufferContent, crossbeam::channel::TryRecvError> {
        if let Some(rx) = self.rx.upgrade() {
            rx.pop().ok_or(crossbeam::channel::TryRecvError::Empty)
        } else {
            Err(crossbeam::channel::TryRecvError::Disconnected)
        }
    }
}

