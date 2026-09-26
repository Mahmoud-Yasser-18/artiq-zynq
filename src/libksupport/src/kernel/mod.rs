use alloc::{string::String, vec::Vec};
use core::ptr;

use libcortex_a9::{mutex::Mutex, semaphore::Semaphore, sync_channel};

use crate::{eh_artiq, RPCException};

mod control;
pub use control::Control;
mod api;
pub mod core1;
mod dma;
pub mod i2c;
mod rpc;
#[cfg(ki_impl = "csr")]
#[path = "rtio_csr.rs"]
pub mod rtio;
#[cfg(ki_impl = "acp")]
#[path = "rtio_acp.rs"]
pub mod rtio;
pub use dma::DmaRecorder;
mod cache;
#[cfg(has_cxp_grabber)]
pub mod cxp;
#[cfg(has_drtio)]
mod subkernel;

#[cfg(has_drtio)]
#[derive(Debug, Clone)]
pub enum SubkernelStatus {
    NoError,
    Timeout,
    IncorrectState,
    CommLost,
    Exception(Vec<u8>),
    OtherError,
}

#[derive(Debug, Clone)]
pub enum Message {
    LoadRequest(Vec<u8>),
    LoadCompleted,
    LoadFailed,
    StartRequest,
    KernelFinished(u8),
    KernelException(
        &'static [Option<eh_artiq::Exception<'static>>],
        &'static [eh_artiq::StackPointerBacktrace],
        &'static [(usize, usize)],
        u8,
    ),

    #[cfg(has_drtio)]
    RtioInitRequest,
    #[cfg(has_drtio)]
    RtioInitReply,

    RpcSend {
        is_async: bool,
        data: Vec<u8>,
    },
    RpcRecvRequest(*mut ()),
    RpcRecvReply(Result<usize, RPCException>),

    CacheGetRequest(String),
    CacheGetReply(Vec<i32>),
    CachePutRequest(String, Vec<i32>),

    DmaPutRequest(DmaRecorder),
    DmaEraseRequest(String),
    DmaGetRequest(String),
    DmaGetReply(Option<(i32, i64, bool)>),
    #[cfg(has_drtio)]
    DmaStartRemoteRequest {
        id: i32,
        timestamp: i64,
    },
    #[cfg(has_drtio)]
    DmaAwaitRemoteRequest(i32),
    #[cfg(has_drtio)]
    DmaAwaitRemoteReply {
        timeout: bool,
        error: u8,
        channel: u32,
        timestamp: u64,
    },

    #[cfg(has_drtio)]
    UpDestinationsRequest(i32),
    #[cfg(has_drtio)]
    UpDestinationsReply(bool),

    #[cfg(has_drtio)]
    I2cStartRequest(u32),
    #[cfg(has_drtio)]
    I2cRestartRequest(u32),
    #[cfg(has_drtio)]
    I2cStopRequest(u32),
    #[cfg(has_drtio)]
    I2cWriteRequest {
        busno: u32,
        data: u8,
    },
    #[cfg(has_drtio)]
    I2cWriteReply {
        succeeded: bool,
        ack: bool,
    },
    #[cfg(has_drtio)]
    I2cReadRequest {
        busno: u32,
        ack: bool,
    },
    #[cfg(has_drtio)]
    I2cReadReply {
        succeeded: bool,
        data: u8,
    },
    #[cfg(has_drtio)]
    I2cBasicReply(bool),
    #[cfg(has_drtio)]
    I2cSwitchSelectRequest {
        busno: u32,
        address: u8,
        mask: u8,
    },

    #[cfg(has_drtio)]
    SubkernelLoadRunRequest {
        id: u32,
        destination: u8,
        run: bool,
        timestamp: u64,
    },
    #[cfg(has_drtio)]
    SubkernelLoadRunReply {
        succeeded: bool,
    },
    #[cfg(has_drtio)]
    SubkernelAwaitFinishRequest {
        id: u32,
        timeout: i64,
    },
    #[cfg(has_drtio)]
    SubkernelAwaitFinishReply,
    #[cfg(has_drtio)]
    SubkernelMsgSend {
        id: u32,
        destination: Option<u8>,
        data: Vec<u8>,
    },
    #[cfg(has_drtio)]
    SubkernelMsgSent,
    #[cfg(has_drtio)]
    SubkernelMsgRecvRequest {
        id: i32,
        timeout: i64,
        tags: Vec<u8>,
    },
    #[cfg(has_drtio)]
    SubkernelMsgRecvReply {
        count: u8,
    },
    #[cfg(has_drtio)]
    SubkernelError(SubkernelStatus),
}

static CHANNEL_0TO1: Mutex<Option<sync_channel::Sender<'static, Message>>> = Mutex::new(None);
static CHANNEL_1TO0: Mutex<Option<sync_channel::Receiver<'static, Message>>> = Mutex::new(None);
static CHANNEL_SEM: Semaphore = Semaphore::new(0, 1);

static mut KERNEL_CHANNEL_0TO1: Option<sync_channel::Receiver<'static, Message>> = None;
static mut KERNEL_CHANNEL_1TO0: Option<sync_channel::Sender<'static, Message>> = None;

pub static mut KERNEL_IMAGE: *const core1::KernelImage = ptr::null();

static INIT_LOCK: Mutex<()> = Mutex::new(());
