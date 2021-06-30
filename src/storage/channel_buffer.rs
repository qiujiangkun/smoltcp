use std::mem::MaybeUninit;

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

#[derive(Debug)]
pub struct ChannelBufferSender {
    tx: crossbeam::channel::Sender<ChannelBufferContent>,
}

impl ChannelBufferSender {
    pub fn new(tx: crossbeam::channel::Sender<ChannelBufferContent>) -> Self {
        Self {
            tx
        }
    }
    pub fn send(&self, content: ChannelBufferContent) {
        let _ = self.tx.send(content);
    }
}

#[derive(Debug)]
pub struct ChannelBufferReceiver {
    rx: crossbeam::channel::Receiver<ChannelBufferContent>,
}

impl ChannelBufferReceiver {
    pub fn new(rx: crossbeam::channel::Receiver<ChannelBufferContent>) -> Self {
        Self {
            rx
        }
    }
    pub fn try_recv(&self) -> Result<ChannelBufferContent, crossbeam::channel::TryRecvError> {
        self.rx.try_recv()
    }
}

