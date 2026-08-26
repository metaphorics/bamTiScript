//! Owned transport inputs and their wake handles.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::sync::mpsc;

/// A wake handle paired with exactly one transport reader.
pub trait ReaderWaker: Send + Sync + 'static {
    /// Whether `wake` guarantees that an idle read returns promptly.
    const REAPABLE: bool;

    fn wake(&self) -> io::Result<()>;
}

/// Splits ownership: the reader moves to the reader thread and only its wake
/// capability remains with the serving thread.
pub trait TransportInput: Send + 'static {
    type Reader: BufRead + Send + 'static;
    type Waker: ReaderWaker;

    fn split(self) -> io::Result<(Self::Reader, Self::Waker)>;
}

pub struct SocketInput(pub TcpStream);

pub struct SocketWaker(Arc<TcpStream>);

impl TransportInput for SocketInput {
    type Reader = BufReader<TcpStream>;
    type Waker = SocketWaker;

    fn split(self) -> io::Result<(Self::Reader, Self::Waker)> {
        let wake = Arc::new(self.0);
        let reader = wake.try_clone()?;
        Ok((BufReader::new(reader), SocketWaker(wake)))
    }
}

impl ReaderWaker for SocketWaker {
    const REAPABLE: bool = true;

    fn wake(&self) -> io::Result<()> {
        self.0.shutdown(Shutdown::Read)
    }
}

#[cfg(unix)]
pub struct PollInput<F>(pub F);

#[cfg(unix)]
pub struct PollReader<F> {
    input: F,
    wake: std::os::fd::OwnedFd,
}

#[cfg(unix)]
pub struct PollWaker(std::os::fd::OwnedFd);

#[cfg(unix)]
impl<F> Read for PollReader<F>
where
    F: Read + std::os::fd::AsFd,
{
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        use rustix::event::{PollFd, PollFlags};
        let mut poll = [
            PollFd::new(&self.input, PollFlags::IN),
            PollFd::new(&self.wake, PollFlags::IN),
        ];
        rustix::event::poll(&mut poll, None)?;
        if poll[1].revents().intersects(PollFlags::IN | PollFlags::HUP) {
            let mut byte = [0_u8; 1];
            let _ = rustix::io::read(&self.wake, &mut byte);
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "transport reader woken",
            ));
        }
        self.input.read(buffer)
    }
}

#[cfg(unix)]
impl<F> TransportInput for PollInput<F>
where
    F: Read + std::os::fd::AsFd + Send + 'static,
{
    type Reader = BufReader<PollReader<F>>;
    type Waker = PollWaker;

    fn split(self) -> io::Result<(Self::Reader, Self::Waker)> {
        let (wake_read, wake_write) = rustix::pipe::pipe()?;
        Ok((
            BufReader::new(PollReader {
                input: self.0,
                wake: wake_read,
            }),
            PollWaker(wake_write),
        ))
    }
}

#[cfg(unix)]
impl ReaderWaker for PollWaker {
    const REAPABLE: bool = true;

    fn wake(&self) -> io::Result<()> {
        match rustix::io::write(&self.0, &[1]) {
            Ok(_) => Ok(()),
            Err(rustix::io::Errno::PIPE) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

/// A deterministic in-process transport with a real blocking channel reader.
pub struct ChannelInput {
    receiver: mpsc::Receiver<Vec<u8>>,
    waker: mpsc::Sender<Vec<u8>>,
}

pub struct ChannelReader {
    receiver: mpsc::Receiver<Vec<u8>>,
    buffered: VecDeque<u8>,
    eof: bool,
}

pub struct ChannelWaker(mpsc::Sender<Vec<u8>>);

impl ChannelInput {
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        let (sender, receiver) = mpsc::channel();
        sender.send(bytes).expect("fresh transport channel");
        sender.send(Vec::new()).expect("fresh transport channel");
        Self {
            receiver,
            waker: sender,
        }
    }

    #[must_use]
    pub fn channel() -> (mpsc::Sender<Vec<u8>>, Self) {
        let (sender, receiver) = mpsc::channel();
        (
            sender.clone(),
            Self {
                receiver,
                waker: sender,
            },
        )
    }
}

impl Read for ChannelReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        while self.buffered.is_empty() && !self.eof {
            match self.receiver.recv() {
                Ok(chunk) if chunk.is_empty() => self.eof = true,
                Ok(chunk) => self.buffered.extend(chunk),
                Err(_) => self.eof = true,
            }
        }
        let count = output.len().min(self.buffered.len());
        for slot in &mut output[..count] {
            *slot = self.buffered.pop_front().expect("bounded by queue length");
        }
        Ok(count)
    }
}

impl TransportInput for ChannelInput {
    type Reader = BufReader<ChannelReader>;
    type Waker = ChannelWaker;

    fn split(self) -> io::Result<(Self::Reader, Self::Waker)> {
        Ok((
            BufReader::new(ChannelReader {
                receiver: self.receiver,
                buffered: VecDeque::new(),
                eof: false,
            }),
            ChannelWaker(self.waker),
        ))
    }
}

impl ReaderWaker for ChannelWaker {
    const REAPABLE: bool = true;

    fn wake(&self) -> io::Result<()> {
        let _ = self.0.send(Vec::new());
        Ok(())
    }
}

/// Windows stdin is the one deliberately non-reapable input. `maybe_run`
/// contains it by terminating the process immediately after serving.
#[cfg(windows)]
pub struct BlockingStdin;

#[cfg(windows)]
pub struct BlockingStdinWaker;

#[cfg(windows)]
impl TransportInput for BlockingStdin {
    type Reader = BufReader<std::io::Stdin>;
    type Waker = BlockingStdinWaker;

    fn split(self) -> io::Result<(Self::Reader, Self::Waker)> {
        Ok((BufReader::new(io::stdin()), BlockingStdinWaker))
    }
}

#[cfg(windows)]
impl ReaderWaker for BlockingStdinWaker {
    const REAPABLE: bool = false;

    fn wake(&self) -> io::Result<()> {
        Ok(())
    }
}
