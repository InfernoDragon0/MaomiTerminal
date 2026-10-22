//! Moves bytes between a russh channel on the tokio thread and the blocking reader / writer
//! threads of a [`maomi_term::PtySession`].

use std::io::{self, Read, Write};
use std::sync::{Arc, mpsc};

use maomi_term::{RemoteControl, TermSize};
use russh::client::{Handle, Msg};
use russh::{Channel, ChannelMsg, Disconnect};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use crate::Client;

enum Command {
    Data(Vec<u8>),
    Resize(TermSize),
    Close,
}

/// The SSH sessions behind one shell and its forward listeners; they close together.
pub(crate) struct Link {
    pub target: Arc<Handle<Client>>,
    /// Jump hosts in route order.
    pub jumps: Vec<Handle<Client>>,
    pub listeners: Vec<JoinHandle<()>>,
}

impl Link {
    /// Stops the listeners, then disconnects from the target back to the first jump host.
    async fn close(self) {
        for listener in &self.listeners {
            listener.abort();
        }
        _ = self
            .target
            .disconnect(Disconnect::ByApplication, "", "en")
            .await;
        for jump in self.jumps.iter().rev() {
            _ = jump.disconnect(Disconnect::ByApplication, "", "en").await;
        }
    }
}

pub(crate) struct Parts {
    pub reader: ChannelReader,
    pub writer: ChannelWriter,
    pub control: Control,
    pub exit: mpsc::Receiver<Option<u32>>,
}

/// Columns, rows and pixel width / height for PTY requests.
pub fn pty_size(size: TermSize) -> (u32, u32, u32, u32) {
    let (cols, rows) = (u32::from(size.columns), u32::from(size.rows));
    (
        cols,
        rows,
        cols * u32::from(size.cell_width),
        rows * u32::from(size.cell_height),
    )
}

/// Starts the pump task for an open shell channel. Must run on the tokio runtime.
pub(crate) fn start(link: Link, channel: Channel<Msg>) -> Parts {
    let (commands, command_rx) = unbounded_channel();
    let (output, output_rx) = mpsc::channel();
    let (exit, exit_rx) = mpsc::channel();
    tokio::spawn(pump(link, channel, command_rx, output, exit));
    Parts {
        reader: ChannelReader {
            output: output_rx,
            chunk: Vec::new(),
            pos: 0,
        },
        writer: ChannelWriter(commands.clone()),
        control: Control(commands),
        exit: exit_rx,
    }
}

/// Runs until the channel closes or the session asks to close it, then disconnects.
async fn pump(
    link: Link,
    mut channel: Channel<Msg>,
    mut commands: UnboundedReceiver<Command>,
    output: mpsc::Sender<Vec<u8>>,
    exit: mpsc::Sender<Option<u32>>,
) {
    let mut exit_code = None;
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Data(bytes)) => {
                    if channel.data_bytes(bytes).await.is_err() {
                        break;
                    }
                }
                Some(Command::Resize(size)) => {
                    let (cols, rows, width, height) = pty_size(size);
                    _ = channel.window_change(cols, rows, width, height).await;
                }
                Some(Command::Close) | None => {
                    _ = channel.close().await;
                    break;
                }
            },
            message = channel.wait() => match message {
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    if output.send(data.to_vec()).is_err() {
                        break;
                    }
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => exit_code = Some(exit_status),
                Some(ChannelMsg::Close) | None => break,
                _ => {}
            },
        }
    }
    // Closing the output ends the reader; the exit code follows so the waiter can finish.
    drop(output);
    _ = exit.send(exit_code);
    link.close().await;
}

/// Shell output as a blocking [`Read`]; EOF once the channel closes.
pub(crate) struct ChannelReader {
    output: mpsc::Receiver<Vec<u8>>,
    chunk: Vec<u8>,
    pos: usize,
}

impl Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.chunk.len() {
            match self.output.recv() {
                Ok(chunk) => {
                    self.chunk = chunk;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.chunk.len() - self.pos);
        buf[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Input for the shell; fails with a broken pipe once the channel is gone.
pub(crate) struct ChannelWriter(UnboundedSender<Command>);

impl Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .send(Command::Data(buf.to_vec()))
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) struct Control(UnboundedSender<Command>);

impl RemoteControl for Control {
    fn resize(&self, size: TermSize) {
        _ = self.0.send(Command::Resize(size));
    }

    fn close(&self) {
        _ = self.0.send(Command::Close);
    }
}
