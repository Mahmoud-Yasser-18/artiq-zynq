use alloc::{collections::BTreeMap, rc::Rc, string::String, vec, vec::Vec};
use core::{cell::RefCell, fmt, slice, str};

use core_io::Error as IoError;
use cslice::CSlice;
use dyld::elf;
use futures::{future::FutureExt, select_biased};
#[cfg(has_drtio)]
use io::Cursor;
#[cfg(has_drtio)]
use ksupport::rpc;
use ksupport::{kernel, resolve_channel_name};
use libasync::{smoltcp::{Sockets, TcpStream},
               task};
use libboard_artiq::drtio_routing;
#[cfg(feature = "target_kasli_soc")]
use libboard_zynq::error_led::ErrorLED;
use libboard_zynq::{self as zynq,
                    smoltcp::{self,
                              iface::{EthernetInterfaceBuilder, NeighborCache},
                              time::Instant,
                              wire::IpCidr},
                    timer::GlobalTimer};
use libconfig::{net_settings, Config};
use libcortex_a9::{mutex::Mutex,
                   semaphore::Semaphore,
                   sync_channel::{Receiver, Sender}};
use log::{error, info, warn};
use num_derive::{FromPrimitive, ToPrimitive};
use num_traits::{FromPrimitive, ToPrimitive};
#[cfg(has_drtio)]
use tar_no_std::TarArchiveRef;

#[cfg(has_drtio)]
use crate::pl;
use crate::{analyzer, mgmt, moninj, proto_async::*, rpc_async, rtio_dma, rtio_mgt};
#[cfg(has_drtio)]
use crate::{subkernel, subkernel::Error as SubkernelError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NetworkError(smoltcp::Error),
    IoError,
    UnexpectedPattern,
    UnrecognizedPacket,
    #[cfg(has_drtio)]
    SubkernelError(subkernel::Error),
    #[cfg(has_drtio)]
    DestinationDown,
}

pub type Result<T> = core::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::NetworkError(error) => write!(f, "network error: {}", error),
            Error::IoError => write!(f, "io error"),
            Error::UnexpectedPattern => write!(f, "unexpected pattern"),
            Error::UnrecognizedPacket => write!(f, "unrecognized packet"),
            #[cfg(has_drtio)]
            Error::SubkernelError(error) => write!(f, "subkernel error: {:?}", error),
            #[cfg(has_drtio)]
            Error::DestinationDown => write!(f, "subkernel destination down"),
        }
    }
}

impl From<smoltcp::Error> for Error {
    fn from(error: smoltcp::Error) -> Self {
        Error::NetworkError(error)
    }
}

impl From<IoError> for Error {
    fn from(_error: IoError) -> Self {
        Error::IoError
    }
}

#[cfg(has_drtio)]
impl From<subkernel::Error> for Error {
    fn from(error: subkernel::Error) -> Self {
        Error::SubkernelError(error)
    }
}

#[derive(Debug, FromPrimitive, ToPrimitive)]
enum Request {
    SystemInfo = 3,
    LoadKernel = 5,
    RunKernel = 6,
    RPCReply = 7,
    RPCException = 8,
    UploadSubkernel = 9,
}

#[derive(Debug, FromPrimitive, ToPrimitive)]
enum Reply {
    SystemInfo = 2,
    LoadCompleted = 5,
    LoadFailed = 6,
    KernelFinished = 7,
    KernelStartupFailed = 8,
    KernelException = 9,
    RPCRequest = 10,
    WatchdogExpired = 14,
    ClockFailure = 15,
}

static CACHE_STORE: Mutex<BTreeMap<String, Vec<i32>>> = Mutex::new(BTreeMap::new());

async fn write_header(stream: &TcpStream, reply: Reply) -> Result<()> {
    stream
        .send_slice(&[0x5a, 0x5a, 0x5a, 0x5a, reply.to_u8().unwrap()])
        .await?;
    Ok(())
}

async fn read_request(stream: &TcpStream, allow_close: bool) -> Result<Option<Request>> {
    match expect(stream, &[0x5a, 0x5a, 0x5a, 0x5a]).await {
        Ok(true) => {}
        Ok(false) => return Err(Error::UnexpectedPattern),
        Err(smoltcp::Error::Finished) => {
            if allow_close {
                info!("peer closed connection");
                return Ok(None);
            } else {
                error!("peer unexpectedly closed connection");
                return Err(smoltcp::Error::Finished)?;
            }
        }
        Err(e) => return Err(e)?,
    }
    Ok(Some(
        FromPrimitive::from_i8(read_i8(&stream).await?).ok_or(Error::UnrecognizedPacket)?,
    ))
}

async fn read_bytes(stream: &TcpStream) -> Result<Vec<u8>> {
    let length = read_i32(&stream).await? as usize;
    let mut buffer = vec![0; length];
    read_chunk(&stream, &mut buffer).await?;
    Ok(buffer)
}

const RETRY_LIMIT: usize = 100;

async fn fast_send(sender: &mut Sender<'_, kernel::Message>, content: kernel::Message) {
    let mut content = content;
    for _ in 0..RETRY_LIMIT {
        match sender.try_send(content) {
            Ok(()) => return,
            Err(v) => {
                content = v;
            }
        }
    }
    sender.async_send(content).await;
}

async fn fast_recv(receiver: &mut Receiver<'_, kernel::Message>) -> kernel::Message {
    for _ in 0..RETRY_LIMIT {
        match receiver.try_recv() {
            Ok(v) => return v,
            Err(()) => (),
        }
    }
    receiver.async_recv().await
}

async fn write_exception_string(stream: &TcpStream, s: CSlice<'static, u8>) -> Result<()> {
    if s.len() == usize::MAX {
        write_i32(stream, -1).await?;
        write_i32(stream, s.as_ptr() as i32).await?
    } else {
        write_chunk(stream, s.as_ref()).await?;
    };
    Ok(())
}

async fn handle_run_kernel(
    stream: Option<&TcpStream>,
    control: &Rc<RefCell<kernel::Control>>,
    _up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
    aux_mutex: &Rc<Mutex<bool>>,
    routing_table: &drtio_routing::RoutingTable,
    timer: GlobalTimer,
) -> Result<()> {
    control.borrow_mut().tx.async_send(kernel::Message::StartRequest).await;
    loop {
        let reply = control.borrow_mut().rx.async_recv().await;
        match reply {
            kernel::Message::RpcSend { is_async, data } => {
                if stream.is_none() {
                    error!("Unexpected RPC from startup/idle kernel!");
                    break;
                }
                let stream = stream.unwrap();
                write_header(stream, Reply::RPCRequest).await?;
                write_bool(stream, is_async).await?;
                stream.send_slice(&data).await?;
                if !is_async {
                    let host_request = read_request(stream, false).await?.unwrap();
                    match host_request {
                        Request::RPCReply => {
                            let tag = read_bytes(stream).await?;
                            let slot = match fast_recv(&mut control.borrow_mut().rx).await {
                                kernel::Message::RpcRecvRequest(slot) => slot,
                                other => panic!("expected root value slot from core1, not {:?}", other),
                            };
                            rpc_async::recv_return(stream, &tag, slot, &|size| {
                                let control = control.clone();
                                async move {
                                    if size == 0 {
                                        // Don't try to allocate zero-length values, as RpcRecvReply(0) is
                                        // used to terminate the kernel-side receive loop.
                                        0 as *mut ()
                                    } else {
                                        let mut control = control.borrow_mut();
                                        fast_send(&mut control.tx, kernel::Message::RpcRecvReply(Ok(size))).await;
                                        match fast_recv(&mut control.rx).await {
                                            kernel::Message::RpcRecvRequest(slot) => slot,
                                            other => {
                                                panic!("expected nested value slot from kernel CPU, not {:?}", other)
                                            }
                                        }
                                    }
                                }
                            })
                            .await?;
                            control
                                .borrow_mut()
                                .tx
                                .async_send(kernel::Message::RpcRecvReply(Ok(0)))
                                .await;
                        }
                        Request::RPCException => {
                            let mut control = control.borrow_mut();
                            match control.rx.async_recv().await {
                                kernel::Message::RpcRecvRequest(_) => (),
                                other => panic!("expected (ignored) root value slot from kernel CPU, not {:?}", other),
                            }
                            let id = read_i32(stream).await? as u32;
                            let message = read_i32(stream).await? as u32;
                            let param = [
                                read_i64(stream).await?,
                                read_i64(stream).await?,
                                read_i64(stream).await?,
                            ];
                            let file = read_i32(stream).await? as u32;
                            let line = read_i32(stream).await?;
                            let column = read_i32(stream).await?;
                            let function = read_i32(stream).await? as u32;
                            control
                                .tx
                                .async_send(kernel::Message::RpcRecvReply(Err(ksupport::RPCException {
                                    id,
                                    message,
                                    param,
                                    file,
                                    line,
                                    column,
                                    function,
                                })))
                                .await;
                        }
                        _ => {
                            error!("unexpected RPC request from host: {:?}", host_request);
                            return Err(Error::UnrecognizedPacket);
                        }
                    }
                }
            }
            kernel::Message::KernelFinished(async_errors) => {
                if let Some(stream) = stream {
                    write_header(stream, Reply::KernelFinished).await?;
                    write_i8(stream, async_errors as i8).await?;
                }
                break;
            }
            kernel::Message::KernelException(exceptions, stack_pointers, backtrace, async_errors) => {
                match stream {
                    Some(stream) => {
                        // only send the exception data to host if there is host,
                        // i.e. not idle/startup kernel.
                        write_header(stream, Reply::KernelException).await?;
                        write_i32(stream, exceptions.len() as i32).await?;
                        for exception in exceptions.iter() {
                            let exception = exception.as_ref().unwrap();
                            write_i32(stream, exception.id as i32).await?;

                            if exception.message.len() == usize::MAX {
                                // exception with host string
                                write_exception_string(stream, exception.message).await?;
                            } else {
                                let msg = str::from_utf8(unsafe {
                                    slice::from_raw_parts(exception.message.as_ptr(), exception.message.len())
                                })
                                .unwrap()
                                .replace(
                                    "{rtio_channel_info:0}",
                                    &format!(
                                        "0x{:04x}:{}",
                                        exception.param[0],
                                        resolve_channel_name(exception.param[0] as u32)
                                    ),
                                );
                                write_exception_string(stream, unsafe { CSlice::new(msg.as_ptr(), msg.len()) }).await?;
                            }

                            write_i64(stream, exception.param[0] as i64).await?;
                            write_i64(stream, exception.param[1] as i64).await?;
                            write_i64(stream, exception.param[2] as i64).await?;
                            write_exception_string(stream, exception.file).await?;
                            write_i32(stream, exception.line as i32).await?;
                            write_i32(stream, exception.column as i32).await?;
                            write_exception_string(stream, exception.function).await?;
                        }
                        for sp in stack_pointers.iter() {
                            write_i32(stream, sp.stack_pointer as i32).await?;
                            write_i32(stream, sp.initial_backtrace_size as i32).await?;
                            write_i32(stream, sp.current_backtrace_size as i32).await?;
                        }
                        write_i32(stream, backtrace.len() as i32).await?;
                        for &(addr, sp) in backtrace {
                            write_i32(stream, addr as i32).await?;
                            write_i32(stream, sp as i32).await?;
                        }
                        write_i8(stream, async_errors as i8).await?;
                    }
                    None => {
                        error!("Uncaught kernel exceptions: {:?}", exceptions);
                    }
                }
                break;
            }
            kernel::Message::CachePutRequest(key, value) => {
                CACHE_STORE.lock().insert(key, value);
            }
            kernel::Message::CacheGetRequest(key) => {
                const DEFAULT: Vec<i32> = Vec::new();
                let value = CACHE_STORE.lock().get(&key).unwrap_or(&DEFAULT).clone();
                control
                    .borrow_mut()
                    .tx
                    .async_send(kernel::Message::CacheGetReply(value))
                    .await;
            }
            kernel::Message::DmaPutRequest(recorder) => {
                let _id = rtio_dma::put_record(aux_mutex, routing_table, timer, recorder).await;
                #[cfg(has_drtio)]
                rtio_dma::remote_dma::upload_traces(aux_mutex, routing_table, timer, _id).await;
            }
            kernel::Message::DmaEraseRequest(name) => {
                // prevent possible OOM when we have large DMA record replacement.
                rtio_dma::erase(name, aux_mutex, routing_table, timer).await;
            }
            kernel::Message::DmaGetRequest(name) => {
                let result = rtio_dma::retrieve(name).await;
                control
                    .borrow_mut()
                    .tx
                    .async_send(kernel::Message::DmaGetReply(result))
                    .await;
            }
            #[cfg(has_drtio)]
            kernel::Message::DmaStartRemoteRequest { id, timestamp } => {
                rtio_dma::remote_dma::playback(aux_mutex, routing_table, timer, id as u32, timestamp as u64).await;
            }
            #[cfg(has_drtio)]
            kernel::Message::DmaAwaitRemoteRequest(id) => {
                let result = rtio_dma::remote_dma::await_done(id as u32, Some(10_000), timer).await;
                let reply = match result {
                    Ok(rtio_dma::remote_dma::RemoteState::PlaybackEnded {
                        error,
                        channel,
                        timestamp,
                    }) => kernel::Message::DmaAwaitRemoteReply {
                        timeout: false,
                        error: error,
                        channel: channel,
                        timestamp: timestamp,
                    },
                    _ => kernel::Message::DmaAwaitRemoteReply {
                        timeout: true,
                        error: 0,
                        channel: 0,
                        timestamp: 0,
                    },
                };
                control.borrow_mut().tx.async_send(reply).await;
            }
            #[cfg(has_drtio)]
            kernel::Message::I2cStartRequest(busno)
            | kernel::Message::I2cRestartRequest(busno)
            | kernel::Message::I2cStopRequest(busno)
            | kernel::Message::I2cSwitchSelectRequest { busno, .. } => {
                let result = rtio_mgt::drtio::i2c_send_basic(aux_mutex, routing_table, timer, &reply, busno).await;
                let reply = match result {
                    Ok(succeeded) => kernel::Message::I2cBasicReply(succeeded),
                    Err(_) => kernel::Message::I2cBasicReply(false),
                };
                control.borrow_mut().tx.async_send(reply).await;
            }
            #[cfg(has_drtio)]
            kernel::Message::I2cWriteRequest { busno, data } => {
                let result = rtio_mgt::drtio::i2c_send_write(aux_mutex, routing_table, timer, busno, data).await;
                let reply = match result {
                    Ok((succeeded, ack)) => kernel::Message::I2cWriteReply { succeeded, ack },
                    Err(_) => kernel::Message::I2cWriteReply {
                        succeeded: false,
                        ack: false,
                    },
                };
                control.borrow_mut().tx.async_send(reply).await;
            }
            #[cfg(has_drtio)]
            kernel::Message::I2cReadRequest { busno, ack } => {
                let result = rtio_mgt::drtio::i2c_send_read(aux_mutex, routing_table, timer, busno, ack).await;
                let reply = match result {
                    Ok((succeeded, data)) => kernel::Message::I2cReadReply { succeeded, data },
                    Err(_) => kernel::Message::I2cReadReply {
                        succeeded: false,
                        data: 0xFF,
                    },
                };
                control.borrow_mut().tx.async_send(reply).await;
            }
            #[cfg(has_drtio)]
            kernel::Message::SubkernelLoadRunRequest {
                id,
                destination: _,
                run,
                timestamp,
            } => {
                let succeeded = match subkernel::load(aux_mutex, routing_table, timer, id, run, timestamp).await {
                    Ok(()) => true,
                    Err(e) => {
                        error!("Error loading subkernel: {:?}", e);
                        false
                    }
                };
                control
                    .borrow_mut()
                    .tx
                    .async_send(kernel::Message::SubkernelLoadRunReply { succeeded: succeeded })
                    .await;
            }
            #[cfg(has_drtio)]
            kernel::Message::SubkernelAwaitFinishRequest { id, timeout } => {
                let res = subkernel::await_finish(aux_mutex, routing_table, timer, id, timeout).await;
                let response = match res {
                    Ok(res) => {
                        if res.status == subkernel::FinishStatus::CommLost {
                            kernel::Message::SubkernelError(kernel::SubkernelStatus::CommLost)
                        } else if let Some(exception) = res.exception {
                            kernel::Message::SubkernelError(kernel::SubkernelStatus::Exception(exception))
                        } else {
                            kernel::Message::SubkernelAwaitFinishReply
                        }
                    }
                    Err(SubkernelError::Timeout) => kernel::Message::SubkernelError(kernel::SubkernelStatus::Timeout),
                    Err(SubkernelError::IncorrectState) => {
                        kernel::Message::SubkernelError(kernel::SubkernelStatus::IncorrectState)
                    }
                    Err(_) => kernel::Message::SubkernelError(kernel::SubkernelStatus::OtherError),
                };
                control.borrow_mut().tx.async_send(response).await;
            }
            #[cfg(has_drtio)]
            kernel::Message::SubkernelMsgSend { id, destination, data } => {
                let res =
                    subkernel::message_send(aux_mutex, routing_table, timer, id, destination.unwrap(), data).await;
                match res {
                    Ok(_) => (),
                    Err(e) => {
                        error!("error sending subkernel message: {:?}", e)
                    }
                };
                control
                    .borrow_mut()
                    .tx
                    .async_send(kernel::Message::SubkernelMsgSent)
                    .await;
            }
            #[cfg(has_drtio)]
            kernel::Message::SubkernelMsgRecvRequest { id, timeout, tags } => {
                let message_received = subkernel::message_await(id as u32, timeout, timer).await;
                let response = match message_received {
                    Ok(ref message) => kernel::Message::SubkernelMsgRecvReply { count: message.count },
                    Err(SubkernelError::Timeout) => kernel::Message::SubkernelError(kernel::SubkernelStatus::Timeout),
                    Err(SubkernelError::IncorrectState) => {
                        kernel::Message::SubkernelError(kernel::SubkernelStatus::IncorrectState)
                    }
                    Err(SubkernelError::CommLost) => kernel::Message::SubkernelError(kernel::SubkernelStatus::CommLost),
                    Err(SubkernelError::SubkernelException) => {
                        // just retrieve the exception
                        let status = subkernel::await_finish(aux_mutex, routing_table, timer, id as u32, timeout)
                            .await
                            .unwrap();
                        kernel::Message::SubkernelError(kernel::SubkernelStatus::Exception(status.exception.unwrap()))
                    }
                    Err(_) => kernel::Message::SubkernelError(kernel::SubkernelStatus::OtherError),
                };
                control.borrow_mut().tx.async_send(response).await;
                if let Ok(message) = message_received {
                    // receive code almost identical to RPC recv, except we are not reading from a stream
                    let mut reader = Cursor::new(message.data);
                    let mut current_tags: &[u8] = &tags;
                    let mut i = 0;
                    loop {
                        // kernel has to consume all arguments in the whole message
                        let slot = match fast_recv(&mut control.borrow_mut().rx).await {
                            kernel::Message::RpcRecvRequest(slot) => slot,
                            other => panic!("expected root value slot from core1, not {:?}", other),
                        };
                        let remaining_tags = rpc::recv_return(&mut reader, &current_tags, slot, &mut |size| {
                            if size == 0 {
                                0 as *mut ()
                            } else {
                                let mut control = control.borrow_mut();
                                control.tx.send(kernel::Message::RpcRecvReply(Ok(size)));
                                match control.rx.recv() {
                                    kernel::Message::RpcRecvRequest(slot) => slot,
                                    other => {
                                        panic!("expected nested value slot from kernel CPU, not {:?}", other)
                                    }
                                }
                            }
                        })?;
                        control
                            .borrow_mut()
                            .tx
                            .async_send(kernel::Message::RpcRecvReply(Ok(0)))
                            .await;
                        i += 1;
                        if i < message.count {
                            current_tags = remaining_tags;
                        } else {
                            break;
                        }
                    }
                }
            }
            #[cfg(has_drtio)]
            kernel::Message::UpDestinationsRequest(destination) => {
                let result = _up_destinations.borrow()[destination as usize];
                control
                    .borrow_mut()
                    .tx
                    .async_send(kernel::Message::UpDestinationsReply(result))
                    .await;
            }
            #[cfg(has_drtio)]
            kernel::Message::RtioInitRequest => {
                rtio_mgt::drtio::reset(aux_mutex, routing_table, timer).await;
                control.borrow_mut().tx.async_send(kernel::Message::RtioInitReply).await;
            }
            _ => {
                panic!("unexpected message from core1 while kernel was running: {:?}", reply);
            }
        }
    }
    Ok(())
}

async fn handle_flash_kernel(
    buffer: &Vec<u8>,
    control: &Rc<RefCell<kernel::Control>>,
    _up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
    _aux_mutex: &Rc<Mutex<bool>>,
    _routing_table: &drtio_routing::RoutingTable,
    _timer: GlobalTimer,
) -> Result<()> {
    if buffer[0] == elf::ELFMAG0 && buffer[1] == elf::ELFMAG1 && buffer[2] == elf::ELFMAG2 && buffer[3] == elf::ELFMAG3
    {
        // assume ELF file, proceed as before
        load_kernel(buffer, control, None).await
    } else {
        #[cfg(has_drtio)]
        {
            let archive = TarArchiveRef::new(buffer.as_ref());
            let entries = archive.entries();
            let mut main_lib: Vec<u8> = Vec::new();
            for entry in entries {
                if entry.filename().as_str() == "main.elf" {
                    main_lib = entry.data().to_vec();
                } else {
                    // subkernel filename must be in format:
                    // "<subkernel id> <destination>.elf"
                    let filename = entry.filename();
                    let mut iter = filename.as_str().split_whitespace();
                    let sid: u32 = iter.next().unwrap().parse().unwrap();
                    let dest: u8 = iter.next().unwrap().strip_suffix(".elf").unwrap().parse().unwrap();
                    let up = _up_destinations.borrow()[dest as usize];
                    if up {
                        let subkernel_lib = entry.data().to_vec();
                        subkernel::add_subkernel(sid, dest, subkernel_lib).await;
                        match subkernel::upload(_aux_mutex, _routing_table, _timer, sid).await {
                            Ok(_) => (),
                            Err(_) => return Err(Error::UnexpectedPattern),
                        }
                    } else {
                        return Err(Error::DestinationDown);
                    }
                }
            }
            load_kernel(&main_lib, control, None).await
        }
        #[cfg(not(has_drtio))]
        {
            panic!("multi-kernel libraries are not supported in standalone systems");
        }
    }
}

async fn load_kernel(
    buffer: &Vec<u8>,
    control: &Rc<RefCell<kernel::Control>>,
    stream: Option<&TcpStream>,
) -> Result<()> {
    let mut control = control.borrow_mut();
    control.restart();
    control
        .tx
        .async_send(kernel::Message::LoadRequest(buffer.to_vec()))
        .await;
    let reply = control.rx.async_recv().await;
    match reply {
        kernel::Message::LoadCompleted => {
            if let Some(stream) = stream {
                write_header(stream, Reply::LoadCompleted).await?;
            }
            Ok(())
        }
        kernel::Message::LoadFailed => {
            if let Some(stream) = stream {
                write_header(stream, Reply::LoadFailed).await?;
                write_chunk(stream, b"core1 failed to process data").await?;
            } else {
                error!("Kernel load failed");
            }
            Err(Error::UnexpectedPattern)
        }
        _ => {
            error!("unexpected message from core1: {:?}", reply);
            if let Some(stream) = stream {
                write_header(stream, Reply::LoadFailed).await?;
                write_chunk(stream, b"core1 sent unexpected reply").await?;
            }
            Err(Error::UnrecognizedPacket)
        }
    }
}

async fn handle_connection(
    stream: &mut TcpStream,
    control: Rc<RefCell<kernel::Control>>,
    up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
    aux_mutex: &Rc<Mutex<bool>>,
    routing_table: &drtio_routing::RoutingTable,
    timer: GlobalTimer,
) -> Result<()> {
    stream.set_ack_delay(None);

    if !expect(stream, b"ARTIQ coredev\n").await? {
        return Err(Error::UnexpectedPattern);
    }
    stream.send_slice("e".as_bytes()).await?;
    #[cfg(has_drtio)]
    subkernel::clear_subkernels().await;
    loop {
        let request = read_request(stream, true).await?;
        if request.is_none() {
            #[cfg(has_drtio)]
            subkernel::clear_subkernels().await;
            return Ok(());
        }
        let request = request.unwrap();
        match request {
            Request::SystemInfo => {
                write_header(stream, Reply::SystemInfo).await?;
                stream.send_slice("ARZQ".as_bytes()).await?;
            }
            Request::LoadKernel => {
                let buffer = read_bytes(stream).await?;
                load_kernel(&buffer, &control, Some(stream)).await?;
            }
            Request::RunKernel => {
                handle_run_kernel(
                    Some(stream),
                    &control,
                    &up_destinations,
                    aux_mutex,
                    routing_table,
                    timer,
                )
                .await?;
            }
            Request::UploadSubkernel => {
                #[cfg(has_drtio)]
                {
                    let id = read_i32(stream).await? as u32;
                    let destination = read_i8(stream).await? as u8;
                    let buffer = read_bytes(stream).await?;
                    subkernel::add_subkernel(id, destination, buffer).await;
                    match subkernel::upload(aux_mutex, routing_table, timer, id).await {
                        Ok(_) => write_header(stream, Reply::LoadCompleted).await?,
                        Err(_) => {
                            write_header(stream, Reply::LoadFailed).await?;
                            write_chunk(stream, b"subkernel failed to load").await?;
                            return Err(Error::UnexpectedPattern);
                        }
                    }
                }
                #[cfg(not(has_drtio))]
                {
                    write_header(stream, Reply::LoadFailed).await?;
                    write_chunk(stream, b"No DRTIO on this system, subkernels are not supported").await?;
                    return Err(Error::UnexpectedPattern);
                }
            }
            _ => {
                error!("unexpected request from host: {:?}", request);
                return Err(Error::UnrecognizedPacket);
            }
        }
    }
}

async fn load_and_run_idle_kernel(
    buffer: &Vec<u8>,
    control: &Rc<RefCell<kernel::Control>>,
    up_destinations: &Rc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
    aux_mutex: &Rc<Mutex<bool>>,
    routing_table: &drtio_routing::RoutingTable,
    timer: GlobalTimer,
) {
    info!("Loading idle kernel");
    let res = handle_flash_kernel(buffer, control, up_destinations, aux_mutex, routing_table, timer).await;
    match res {
        Err(_) => warn!("error loading idle kernel"),
        _ => (),
    }
    info!("Running idle kernel");
    let _ = handle_run_kernel(None, control, up_destinations, aux_mutex, routing_table, timer)
        .await
        .map_err(|_| warn!("error running idle kernel"));
    info!("Idle kernel terminated");
}

pub fn main(timer: GlobalTimer, cfg: Config) {
    let net_addresses = net_settings::get_addresses(&cfg);
    info!("network addresses: {}", net_addresses);

    let eth = zynq::eth::Eth::eth0(net_addresses.hardware_addr.0.clone());
    const RX_LEN: usize = 64;
    // Number of transmission buffers (minimum is two because with
    // one, duplicate packet transmission occurs)
    const TX_LEN: usize = 64;
    let eth = eth.start_rx(RX_LEN);
    let mut eth = eth.start_tx(TX_LEN);

    let neighbor_cache = NeighborCache::new(alloc::collections::BTreeMap::new());
    let mut iface = match net_addresses.ipv6_addr {
        Some(addr) => {
            let ip_addrs = [
                IpCidr::new(net_addresses.ipv4_addr, 0),
                IpCidr::new(net_addresses.ipv6_ll_addr, 0),
                IpCidr::new(addr, 0),
            ];
            EthernetInterfaceBuilder::new(&mut eth)
                .ethernet_addr(net_addresses.hardware_addr)
                .ip_addrs(ip_addrs)
                .neighbor_cache(neighbor_cache)
                .finalize()
        }
        None => {
            let ip_addrs = [
                IpCidr::new(net_addresses.ipv4_addr, 0),
                IpCidr::new(net_addresses.ipv6_ll_addr, 0),
            ];
            EthernetInterfaceBuilder::new(&mut eth)
                .ethernet_addr(net_addresses.hardware_addr)
                .ip_addrs(ip_addrs)
                .neighbor_cache(neighbor_cache)
                .finalize()
        }
    };

    Sockets::init(32);

    let aux_mutex: Rc<Mutex<bool>> = Rc::new(Mutex::new(false));
    #[cfg(has_drtio)]
    let drtio_routing_table = Rc::new(RefCell::new(drtio_routing::config_routing_table(
        pl::csr::DRTIO.len(),
        &cfg,
    )));
    #[cfg(not(has_drtio))]
    let drtio_routing_table = Rc::new(RefCell::new(drtio_routing::RoutingTable::default_empty()));
    let up_destinations = Rc::new(RefCell::new([false; drtio_routing::DEST_COUNT]));
    #[cfg(has_drtio_routing)]
    drtio_routing::interconnect_disable_all();

    rtio_mgt::startup(&aux_mutex, &drtio_routing_table, &up_destinations, &cfg, timer);
    ksupport::setup_device_map(&cfg);

    analyzer::start(&aux_mutex, &drtio_routing_table, &up_destinations, timer);
    moninj::start(timer, &aux_mutex, &drtio_routing_table);

    let control: Rc<RefCell<kernel::Control>> = Rc::new(RefCell::new(kernel::Control::start()));
    let idle_kernel = Rc::new(cfg.read("idle_kernel").ok());
    if let Ok(buffer) = cfg.read("startup_kernel") {
        info!("Loading startup kernel...");
        let routing_table = drtio_routing_table.borrow();
        if let Ok(()) = task::block_on(handle_flash_kernel(
            &buffer,
            &control,
            &up_destinations,
            &aux_mutex,
            &routing_table,
            timer,
        )) {
            info!("Starting startup kernel...");
            let _ = task::block_on(handle_run_kernel(
                None,
                &control,
                &up_destinations,
                &aux_mutex,
                &routing_table,
                timer,
            ));
            info!("Startup kernel finished!");
        } else {
            error!("Error loading startup kernel!");
        }
    }

    mgmt::start(cfg);

    task::spawn(async move {
        let connection = Rc::new(Semaphore::new(0, 1));
        let terminate = Rc::new(Semaphore::new(0, 1));
        {
            let control = control.clone();
            let idle_kernel = idle_kernel.clone();
            let connection = connection.clone();
            let terminate = terminate.clone();
            let up_destinations = up_destinations.clone();
            let aux_mutex = aux_mutex.clone();
            let routing_table = drtio_routing_table.clone();
            task::spawn(async move {
                let routing_table = routing_table.borrow();
                select_biased! {
                    _ = (async {
                        if let Some(buffer) = &*idle_kernel {
                            load_and_run_idle_kernel(&buffer, &control, &up_destinations, &aux_mutex, &routing_table, timer).await;
                        }
                    }).fuse() => (),
                    _ = terminate.async_wait().fuse() => ()
                }
                connection.signal();
            });
        }

        loop {
            let mut stream = TcpStream::accept(1381, 0x10_000, 0x10_000).await.unwrap();

            if connection.try_wait().is_none() {
                // there is an existing connection
                terminate.signal();
                connection.async_wait().await;
            }

            let control = control.clone();
            let idle_kernel = idle_kernel.clone();
            let connection = connection.clone();
            let terminate = terminate.clone();
            let up_destinations = up_destinations.clone();
            let aux_mutex = aux_mutex.clone();
            let routing_table = drtio_routing_table.clone();

            // we make sure the value of terminate is 0 before we start
            let _ = terminate.try_wait();
            task::spawn(async move {
                let routing_table = routing_table.borrow();
                select_biased! {
                    _ = (async {
                        let _ = handle_connection(&mut stream, control.clone(), &up_destinations, &aux_mutex, &routing_table, timer)
                            .await
                            .map_err(|e| warn!("connection terminated: {}", e));
                        if let Some(buffer) = &*idle_kernel {
                            load_and_run_idle_kernel(&buffer, &control, &up_destinations, &aux_mutex, &routing_table, timer).await;
                        }
                    }).fuse() => (),
                    _ = terminate.async_wait().fuse() => ()
                }
                connection.signal();
                let _ = stream.flush().await;
                let _ = stream.abort().await;
            });
        }
    });

    Sockets::run(&mut iface, || Instant::from_millis(timer.get_time().0 as i32));
}

pub fn soft_panic_main(timer: GlobalTimer, cfg: Config) -> ! {
    let net_addresses = net_settings::get_addresses(&cfg);
    info!("network addresses: {}", net_addresses);

    let eth = zynq::eth::Eth::eth0(net_addresses.hardware_addr.0.clone());
    const RX_LEN: usize = 64;
    // Number of transmission buffers (minimum is two because with
    // one, duplicate packet transmission occurs)
    const TX_LEN: usize = 64;
    let eth = eth.start_rx(RX_LEN);
    let mut eth = eth.start_tx(TX_LEN);

    let neighbor_cache = NeighborCache::new(alloc::collections::BTreeMap::new());
    let mut iface = match net_addresses.ipv6_addr {
        Some(addr) => {
            let ip_addrs = [
                IpCidr::new(net_addresses.ipv4_addr, 0),
                IpCidr::new(net_addresses.ipv6_ll_addr, 0),
                IpCidr::new(addr, 0),
            ];
            EthernetInterfaceBuilder::new(&mut eth)
                .ethernet_addr(net_addresses.hardware_addr)
                .ip_addrs(ip_addrs)
                .neighbor_cache(neighbor_cache)
                .finalize()
        }
        None => {
            let ip_addrs = [
                IpCidr::new(net_addresses.ipv4_addr, 0),
                IpCidr::new(net_addresses.ipv6_ll_addr, 0),
            ];
            EthernetInterfaceBuilder::new(&mut eth)
                .ethernet_addr(net_addresses.hardware_addr)
                .ip_addrs(ip_addrs)
                .neighbor_cache(neighbor_cache)
                .finalize()
        }
    };

    Sockets::init(32);

    mgmt::start(cfg);

    // getting eth settings disables the LED as it resets GPIO
    // need to re-enable it here
    #[cfg(feature = "target_kasli_soc")]
    {
        let mut err_led = ErrorLED::error_led();
        err_led.toggle(true);
    }

    Sockets::run(&mut iface, || Instant::from_millis(timer.get_time().0 as i32));
}
