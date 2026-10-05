use crate::MAX_UDP_DATAGRAM;
use crate::arena_alloc::{Core, ReaderSet, WriterLane};
use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const PROTOCOL_VERSION: u8 = 1;
const HEADER_LEN: usize = 16;
const MAX_FRAME_BODY: usize = 16 * 1024 * 1024;
const MAX_BATCH_SIZE: u16 = 1024;

type Record = Arc<[u8]>;
type Subscribers = Arc<Mutex<Vec<SyncSender<Record>>>>;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Open = 1,
    Ack = 2,
    Delivery = 3,
    Ping = 4,
    Error = 5,
    PushBatch = 6,
}

impl Op {
    fn from_byte(value: u8) -> io::Result<Self> {
        match value {
            1 => Ok(Self::Open),
            2 => Ok(Self::Ack),
            3 => Ok(Self::Delivery),
            4 => Ok(Self::Ping),
            5 => Ok(Self::Error),
            6 => Ok(Self::PushBatch),
            _ => Err(invalid_data("unknown frame op")),
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientMode {
    Read = 1,
    Write = 2,
    ReadWrite = 3,
}

impl ClientMode {
    fn from_byte(value: u8) -> io::Result<Self> {
        match value {
            1 => Ok(Self::Read),
            2 => Ok(Self::Write),
            3 => Ok(Self::ReadWrite),
            _ => Err(invalid_data("unknown client mode")),
        }
    }

    fn can_read(self) -> bool {
        matches!(self, Self::Read | Self::ReadWrite)
    }

    fn can_write(self) -> bool {
        matches!(self, Self::Write | Self::ReadWrite)
    }
}

pub struct Frame {
    pub op: Op,
    pub flags: u16,
    pub stream_id: u64,
    pub body: Vec<u8>,
}

impl Frame {
    pub fn read_from(reader: &mut impl Read) -> io::Result<Self> {
        read_frame(reader)
    }

    pub fn mode_and_batch(&self) -> io::Result<(ClientMode, u16)> {
        if self.body.len() != 3 {
            return Err(invalid_data("open frame body must be mode + batch size"));
        }
        let mode = ClientMode::from_byte(self.body[0])?;
        let batch_size = u16::from_be_bytes([self.body[1], self.body[2]]);
        if batch_size == 0 || batch_size > MAX_BATCH_SIZE {
            return Err(invalid_data("batch size out of range"));
        }
        Ok((mode, batch_size))
    }
}

pub struct Server {
    local_addr: SocketAddr,
    _accept: JoinHandle<()>,
    _dispatch: JoinHandle<()>,
}

impl Server {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

pub struct VataClient {
    stream: TcpStream,
    pending_deliveries: VecDeque<Vec<Vec<u8>>>,
}

impl VataClient {
    pub fn connect_reader(addr: SocketAddr, batch_size: u16) -> io::Result<Self> {
        Self::connect(addr, ClientMode::Read, batch_size)
    }

    pub fn connect_read_writer(addr: SocketAddr, batch_size: u16) -> io::Result<Self> {
        Self::connect(addr, ClientMode::ReadWrite, batch_size)
    }

    fn connect(addr: SocketAddr, mode: ClientMode, batch_size: u16) -> io::Result<Self> {
        let mut stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true)?;
        write_frame(
            &mut stream,
            &Frame {
                op: Op::Open,
                flags: 0,
                stream_id: 0,
                body: subscribe_body(mode, batch_size),
            },
        )?;
        expect_ack(&mut stream)?;
        Ok(Self {
            stream,
            pending_deliveries: VecDeque::new(),
        })
    }

    pub fn connect_reader_with_retry(
        addr: SocketAddr,
        batch_size: u16,
        attempts: usize,
        delay: Duration,
    ) -> io::Result<Self> {
        let attempts = attempts.max(1);
        let mut last_error = None;
        for _ in 0..attempts {
            match Self::connect_reader(addr, batch_size) {
                Ok(client) => return Ok(client),
                Err(err) => last_error = Some(err),
            }
            thread::sleep(delay);
        }
        Err(last_error.unwrap_or_else(|| invalid_input("connect attempts must be positive")))
    }

    pub fn warm(addr: SocketAddr) -> io::Result<()> {
        let mut stream = TcpStream::connect(addr)?;
        write_frame(
            &mut stream,
            &Frame {
                op: Op::Ping,
                flags: 0,
                stream_id: 0,
                body: Vec::new(),
            },
        )?;
        expect_ack(&mut stream)
    }

    pub fn read_batch(&mut self) -> io::Result<Vec<Vec<u8>>> {
        if let Some(batch) = self.pending_deliveries.pop_front() {
            return Ok(batch);
        }
        let frame = Frame::read_from(&mut self.stream)?;
        if frame.op != Op::Delivery {
            return Err(invalid_data("expected delivery frame"));
        }
        records_from_body(&frame.body, MAX_UDP_DATAGRAM)
    }

    pub fn read_batch_timeout(&mut self, timeout: Duration) -> io::Result<Vec<Vec<u8>>> {
        self.stream.set_read_timeout(Some(timeout))?;
        self.read_batch()
    }

    pub fn write_batch<I, B>(&mut self, records: I) -> io::Result<()>
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        write_frame(
            &mut self.stream,
            &Frame {
                op: Op::PushBatch,
                flags: 0,
                stream_id: 0,
                body: records_to_body(records),
            },
        )?;
        self.expect_ack_buffering_deliveries()
    }

    fn expect_ack_buffering_deliveries(&mut self) -> io::Result<()> {
        loop {
            let frame = Frame::read_from(&mut self.stream)?;
            match frame.op {
                Op::Ack => return Ok(()),
                Op::Delivery => self
                    .pending_deliveries
                    .push_back(records_from_body(&frame.body, MAX_UDP_DATAGRAM)?),
                Op::Error => {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        String::from_utf8_lossy(&frame.body).into_owned(),
                    ));
                }
                _ => return Err(invalid_data("expected ack frame")),
            }
        }
    }
}

pub fn spawn<A: ToSocketAddrs>(
    addr: A,
    core: Arc<Core>,
    writer: Option<WriterLane>,
    reader_id: usize,
    queue_capacity: usize,
) -> io::Result<Server> {
    let listener = TcpListener::bind(addr)?;
    let local_addr = listener.local_addr()?;
    let subscribers = Subscribers::default();
    // ponytail: one TCP writer lane behind a mutex; shard by connection if TCP write throughput matters.
    let writer = writer.map(|lane| Arc::new(Mutex::new(lane)));
    let dispatch_subscribers = Arc::clone(&subscribers);
    let queue_capacity = queue_capacity.max(1);

    let dispatch = thread::Builder::new()
        .name("client-dispatch".into())
        .spawn(move || dispatch_records(core, reader_id, dispatch_subscribers))?;

    let accept_subscribers = Arc::clone(&subscribers);
    let accept_writer = writer.clone();
    let accept = thread::Builder::new()
        .name("client-listener".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let subscribers = Arc::clone(&accept_subscribers);
                        let writer = accept_writer.clone();
                        let _ =
                            thread::Builder::new()
                                .name("client-conn".into())
                                .spawn(move || {
                                    let _ =
                                        handle_client(stream, subscribers, writer, queue_capacity);
                                });
                    }
                    Err(err) if err.kind() == ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        })?;

    Ok(Server {
        local_addr,
        _accept: accept,
        _dispatch: dispatch,
    })
}

pub fn read_frame(reader: &mut impl Read) -> io::Result<Frame> {
    let mut header = [0; HEADER_LEN];
    reader.read_exact(&mut header)?;
    let body_len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if body_len > MAX_FRAME_BODY {
        return Err(invalid_data("frame body too large"));
    }
    if header[4] != PROTOCOL_VERSION {
        return Err(invalid_data("unsupported protocol version"));
    }
    let op = Op::from_byte(header[5])?;
    let flags = u16::from_be_bytes([header[6], header[7]]);
    let stream_id = u64::from_be_bytes([
        header[8], header[9], header[10], header[11], header[12], header[13], header[14],
        header[15],
    ]);
    let mut body = vec![0; body_len];
    reader.read_exact(&mut body)?;
    Ok(Frame {
        op,
        flags,
        stream_id,
        body,
    })
}

pub fn write_frame(writer: &mut impl Write, frame: &Frame) -> io::Result<()> {
    if frame.body.len() > MAX_FRAME_BODY {
        return Err(invalid_input("frame body too large"));
    }
    let mut header = [0; HEADER_LEN];
    header[..4].copy_from_slice(&(frame.body.len() as u32).to_be_bytes());
    header[4] = PROTOCOL_VERSION;
    header[5] = frame.op as u8;
    header[6..8].copy_from_slice(&frame.flags.to_be_bytes());
    header[8..16].copy_from_slice(&frame.stream_id.to_be_bytes());
    writer.write_all(&header)?;
    writer.write_all(&frame.body)
}

pub fn subscribe_body(mode: ClientMode, batch_size: u16) -> Vec<u8> {
    let mut body = Vec::with_capacity(3);
    body.push(mode as u8);
    body.extend_from_slice(&batch_size.to_be_bytes());
    body
}

pub fn records_to_body<I, B>(records: I) -> Vec<u8>
where
    I: IntoIterator<Item = B>,
    B: AsRef<[u8]>,
{
    let mut body = vec![0, 0];
    let mut count = 0u16;
    for record in records {
        let record = record.as_ref();
        count = count.checked_add(1).expect("too many records in batch");
        body.extend_from_slice(&(record.len() as u32).to_be_bytes());
        body.extend_from_slice(record);
    }
    body[..2].copy_from_slice(&count.to_be_bytes());
    body
}

pub fn records_from_body(body: &[u8], max_record_len: usize) -> io::Result<Vec<Vec<u8>>> {
    if body.len() < 2 {
        return Err(invalid_data("delivery body missing record count"));
    }
    let count = u16::from_be_bytes([body[0], body[1]]) as usize;
    let mut records = Vec::with_capacity(count);
    let mut offset = 2;
    for _ in 0..count {
        if offset + 4 > body.len() {
            return Err(invalid_data("delivery body missing record length"));
        }
        let len = u32::from_be_bytes([
            body[offset],
            body[offset + 1],
            body[offset + 2],
            body[offset + 3],
        ]) as usize;
        offset += 4;
        if len > max_record_len {
            return Err(invalid_data("delivery record too large"));
        }
        if offset + len > body.len() {
            return Err(invalid_data("delivery body truncated"));
        }
        records.push(body[offset..offset + len].to_vec());
        offset += len;
    }
    if offset != body.len() {
        return Err(invalid_data("delivery body has trailing bytes"));
    }
    Ok(records)
}

fn dispatch_records(core: Arc<Core>, reader_id: usize, subscribers: Subscribers) {
    let reader = unsafe { ReaderSet::new(core, reader_id) };
    let mut out = vec![0; MAX_UDP_DATAGRAM];
    loop {
        let len = reader.read_next_blocking(&mut out);
        let record = Record::from(&out[..len]);
        let mut subscribers = subscribers.lock().expect("subscriber mutex poisoned");
        subscribers.retain(|sender| match sender.try_send(Arc::clone(&record)) {
            Ok(()) | Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Disconnected(_)) => false,
        });
    }
}

fn handle_client(
    mut stream: TcpStream,
    subscribers: Subscribers,
    writer: Option<Arc<Mutex<WriterLane>>>,
    queue_capacity: usize,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let frame = Frame::read_from(&mut stream)?;
    match frame.op {
        Op::Ping => write_ack(&mut stream, frame.stream_id),
        Op::Open => {
            let (mode, batch_size) = frame.mode_and_batch()?;
            if mode.can_write() && writer.is_none() {
                return write_error(
                    &mut stream,
                    frame.stream_id,
                    "tcp write mode needs a writer lane",
                );
            }
            if mode.can_read() && mode.can_write() {
                let writes = writer.expect("checked when write mode is enabled");
                stream_read_write(stream, subscribers, writes, queue_capacity, batch_size)
            } else if mode.can_read() {
                stream_batches(stream, subscribers, queue_capacity, batch_size)
            } else {
                let writes = writer.expect("checked when write mode is enabled");
                read_push_batches(stream, writes)
            }
        }
        _ => write_error(
            &mut stream,
            frame.stream_id,
            "first frame must be open or ping",
        ),
    }
}

fn stream_read_write(
    stream: TcpStream,
    subscribers: Subscribers,
    writer: Arc<Mutex<WriterLane>>,
    queue_capacity: usize,
    batch_size: u16,
) -> io::Result<()> {
    let (sender, receiver) = mpsc::sync_channel(queue_capacity);
    subscribers
        .lock()
        .expect("subscriber mutex poisoned")
        .push(sender);
    let write_stream = Arc::new(Mutex::new(stream.try_clone()?));
    write_ack_locked(&write_stream, 0)?;
    let deliveries = Arc::clone(&write_stream);
    thread::Builder::new()
        .name("client-send".into())
        .spawn(move || {
            let _ = write_batches_from_receiver(deliveries, receiver, batch_size);
        })?;
    read_push_batches_with_ack(stream, writer, write_stream)
}

fn stream_batches(
    mut stream: TcpStream,
    subscribers: Subscribers,
    queue_capacity: usize,
    batch_size: u16,
) -> io::Result<()> {
    let (sender, receiver) = mpsc::sync_channel(queue_capacity);
    subscribers
        .lock()
        .expect("subscriber mutex poisoned")
        .push(sender);
    write_ack(&mut stream, 0)?;

    write_batches_from_receiver(Arc::new(Mutex::new(stream)), receiver, batch_size)
}

fn write_batches_from_receiver(
    stream: Arc<Mutex<TcpStream>>,
    receiver: mpsc::Receiver<Record>,
    batch_size: u16,
) -> io::Result<()> {
    loop {
        let mut batch = Vec::with_capacity(batch_size as usize);
        for _ in 0..batch_size {
            let record = receiver
                .recv()
                .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "dispatcher stopped"))?;
            batch.push(record);
        }
        let body = records_to_body(batch.iter().map(|record| record.as_ref()));
        write_frame_locked(
            &stream,
            &Frame {
                op: Op::Delivery,
                flags: 0,
                stream_id: 0,
                body,
            },
        )?;
    }
}

fn read_push_batches(stream: TcpStream, writer: Arc<Mutex<WriterLane>>) -> io::Result<()> {
    let write_stream = Arc::new(Mutex::new(stream.try_clone()?));
    write_ack_locked(&write_stream, 0)?;
    read_push_batches_with_ack(stream, writer, write_stream)
}

fn read_push_batches_with_ack(
    mut stream: TcpStream,
    writer: Arc<Mutex<WriterLane>>,
    write_stream: Arc<Mutex<TcpStream>>,
) -> io::Result<()> {
    loop {
        let frame = Frame::read_from(&mut stream)?;
        match frame.op {
            Op::PushBatch => {
                let records = records_from_body(&frame.body, MAX_UDP_DATAGRAM)?;
                write_ack_locked(&write_stream, frame.stream_id)?;
                writer
                    .lock()
                    .expect("writer mutex poisoned")
                    .append_batch(|out| {
                        for record in &records {
                            out.append(record);
                        }
                    });
            }
            Op::Ping => write_ack_locked(&write_stream, frame.stream_id)?,
            _ => write_error_locked(&write_stream, frame.stream_id, "expected push batch")?,
        }
    }
}

fn write_ack(stream: &mut TcpStream, stream_id: u64) -> io::Result<()> {
    write_frame(
        stream,
        &Frame {
            op: Op::Ack,
            flags: 0,
            stream_id,
            body: Vec::new(),
        },
    )
}

fn write_ack_locked(stream: &Arc<Mutex<TcpStream>>, stream_id: u64) -> io::Result<()> {
    write_frame_locked(
        stream,
        &Frame {
            op: Op::Ack,
            flags: 0,
            stream_id,
            body: Vec::new(),
        },
    )
}

fn write_error(stream: &mut TcpStream, stream_id: u64, message: &str) -> io::Result<()> {
    write_frame(
        stream,
        &Frame {
            op: Op::Error,
            flags: 0,
            stream_id,
            body: message.as_bytes().to_vec(),
        },
    )
}

fn write_error_locked(
    stream: &Arc<Mutex<TcpStream>>,
    stream_id: u64,
    message: &str,
) -> io::Result<()> {
    write_frame_locked(
        stream,
        &Frame {
            op: Op::Error,
            flags: 0,
            stream_id,
            body: message.as_bytes().to_vec(),
        },
    )
}

fn write_frame_locked(stream: &Arc<Mutex<TcpStream>>, frame: &Frame) -> io::Result<()> {
    write_frame(
        &mut *stream.lock().expect("client stream mutex poisoned"),
        frame,
    )
}

fn expect_ack(stream: &mut TcpStream) -> io::Result<()> {
    let frame = Frame::read_from(stream)?;
    match frame.op {
        Op::Ack => Ok(()),
        Op::Error => Err(io::Error::new(
            ErrorKind::InvalidData,
            String::from_utf8_lossy(&frame.body).into_owned(),
        )),
        _ => Err(invalid_data("expected ack frame")),
    }
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}

fn invalid_input(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidInput, message)
}
