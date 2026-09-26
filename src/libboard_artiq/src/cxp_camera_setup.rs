use core::fmt;

use libasync::task;
use crate::cxp_compat as timer;
use log::debug;

use crate::{cxp_ctrl::Error as CtrlErr,
            cxp_packet::{async_read_bytes, async_read_u32, async_read_u64, async_write_u32, async_write_u64,
                         reset_tag, send_test_packet, write_bytes_no_ack},
            cxp_phys::{CXPSpeed, rx, tx},
            pl::csr::{CXP, CXP_LEN}};

// Bootstrap registers address
const STANDARD: u32 = 0x0000;
const REVISION: u32 = 0x0004;
const DEVICE_MODEL_NAME: u32 = 0x2020;
const IMAGE_1_STREAM_ID_ADDRESS: u32 = 0x301C;
const CONNECTION_RESET: u32 = 0x4000;
const DEVICE_CONNECTION_ID: u32 = 0x4004;
const MASTER_HOST_CONNECTION_ID: u32 = 0x4008;

const STREAM_PACKET_SIZE_MAX: u32 = 0x4010;
const CONNECTION_CFG: u32 = 0x4014;
const CONNECTION_CFG_DEFAULT: u32 = 0x4018;

const TESTMODE: u32 = 0x401C;
const TEST_ERROR_COUNT_SELECTOR: u32 = 0x4020;
const TEST_ERROR_COUNT: u32 = 0x4024;
const TEST_PACKET_COUNT_TX: u32 = 0x4028;
const TEST_PACKET_COUNT_RX: u32 = 0x4030;

const VERSION_SUPPORTED: u32 = 0x4044;
const VERSION_USED: u32 = 0x4048;

// Setup const
const HOST_CONNECTION_ID: u32 = 0x00006303; // TODO: rename to CXP grabber sinara number when it comes out
#[cfg(max_cxp_stream_pak_size = "8192")]
const MAX_STREAM_PAK_SIZE: u32 = 8192; // 8 KiB FIFO size 
#[cfg(max_cxp_stream_pak_size = "2048")]
const MAX_STREAM_PAK_SIZE: u32 = 2048; // 2 KiB FIFO size 
const TX_TEST_CNT: u8 = 10;
// From DS191 (v1.18.1), max CDR time lock is 37*10^6 UI,
// 37*10^6 UI at lowest CXP linerate of 1.25Gbps = 29.6 ms, double it to account for overhead
const MONITOR_TIMEOUT_MS: u64 = 60;
// Section 12.1.2 (CXP-001-2021)
// Grabber should wait 200 ms for camera to complete connection configuration
const CAMERA_CONNECTION_SETUP_TIME: u64 = 200;

pub const MASTER_CHANNEL: u32 = 0;

#[cfg(max_cxp_speed = "CXP-6")]
const MAX_SPEED: CXPSpeed = CXPSpeed::CXP6;
#[cfg(max_cxp_speed = "CXP-10")]
const MAX_SPEED: CXPSpeed = CXPSpeed::CXP10;
#[cfg(max_cxp_speed = "CXP-12")]
const MAX_SPEED: CXPSpeed = CXPSpeed::CXP12;

pub enum Error {
    CameraNotDetected,
    ConnectionLost(u32),
    ConnectionIDMismatch(u32, u32),
    UnstableRX(u32),
    UnstableTX(u32),
    InsufficientChannels(u32),
    UnsupportedLinerateCode(u32),
    UnsupportedSpeed(CXPSpeed),
    UnsupportedVersion,
    CtrlPacketError(CtrlErr),
}

impl From<CtrlErr> for Error {
    fn from(value: CtrlErr) -> Error {
        Error::CtrlPacketError(value)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &Error::CameraNotDetected => write!(f, "CameraNotDetected"),
            &Error::ConnectionLost(channel) => write!(f, "ConnectionLost - Channel #{} needs to be up", channel),
            &Error::ConnectionIDMismatch(expected_ch, connected_ch) => {
                write!(
                    f,
                    "ConnectionIDMismatch - CXP grabber channel (#{}) is connected to incorrect camera channel (#{})",
                    expected_ch, connected_ch,
                )
            }
            &Error::UnstableRX(channel) => write!(f, "UnstableRX - RX connection test of channel #{} failed", channel),
            &Error::UnstableTX(channel) => write!(f, "UnstableTX - TX connection test of channel #{} failed", channel),
            &Error::InsufficientChannels(channel) => write!(
                f,
                "InsufficientChannels - CXP grabber provides {} channels but camera requests {} channels",
                CXP_LEN, channel,
            ),
            &Error::UnsupportedLinerateCode(linerate_code) => write!(
                f,
                "UnsupportedLinerateCode - {:#X} linerate code is not supported",
                linerate_code
            ),
            &Error::UnsupportedSpeed(speed) => write!(
                f,
                "UnsupportedSpeed - CXP grabber max speed is {} but camera requests {}",
                MAX_SPEED, speed,
            ),
            &Error::UnsupportedVersion => write!(
                f,
                "UnsupportedVersion - Incompatible CXP protocol between CXP grabber and camera"
            ),
            &Error::CtrlPacketError(ref err) => write!(f, "{}", err),
        }
    }
}

pub fn channel_ready(channel: u32) -> bool {
    unsafe { (CXP[channel as usize].rx_ready_read)() == 1 }
}

async fn monitor_channel_status_timeout(channel: u32) -> Result<(), Error> {
    let limit = timer::get_ms() + MONITOR_TIMEOUT_MS;
    while timer::get_ms() < limit {
        task::r#yield().await;
        if channel_ready(channel) {
            return Ok(());
        }
    }
    Err(Error::ConnectionLost(channel))
}

pub async fn discover_camera() -> Result<CXPSpeed, Error> {
    // Section 7.6 (CXP-001-2021)
    // 1.25Gbps (CXP_1) and 3.125Gbps (CXP_3) are the discovery rate
    // both linerate need to be checked as camera only support ONE of discovery rates
    for &speed in [CXPSpeed::CXP1, CXPSpeed::CXP3].iter() {
        // Section 12.1.2 (CXP-001-2021)
        // set tx linerate -> send ConnectionReset -> wait 200ms -> set rx linerate -> monitor connection status with a timeout
        tx::change_linerate(speed);
        write_bytes_no_ack(MASTER_CHANNEL, CONNECTION_RESET, &1_u32.to_be_bytes(), false)?;
        timer::async_delay_ms(CAMERA_CONNECTION_SETUP_TIME).await;
        rx::change_linerate(speed);

        // Check the camera is responsive in case the RX phy picks up noise as an IDLE word
        if monitor_channel_status_timeout(MASTER_CHANNEL).await.is_ok() {
            if let Ok(0xC0A79AE5) = async_read_u32(MASTER_CHANNEL, STANDARD, false).await {
                debug!("camera detected at linerate {:}", speed);
                return Ok(speed);
            }
        }
    }
    Err(Error::CameraNotDetected)
}

async fn check_connection_topology(discovery_speed: CXPSpeed) -> Result<u32, Error> {
    // Make sure master channel is connected correctly before any setup
    check_connection_id(MASTER_CHANNEL).await?;

    // Enable extension channel(s) on camera
    let recommended_channels = async_read_u32(MASTER_CHANNEL, CONNECTION_CFG_DEFAULT, false).await? >> 16;
    debug!("camera recommends using {} channels", recommended_channels);

    if recommended_channels > CXP_LEN as u32 {
        // From Section 12.1.3 (CXP-001-2021), Host shall maintain the master connection at the discovery speed,
        // even though the recommended channels > Host channels.
        // We will raise an discovery error instead since user cannot change the linerate in kernel.
        return Err(Error::InsufficientChannels(recommended_channels));
    }

    let current_cfg = async_read_u32(MASTER_CHANNEL, CONNECTION_CFG, false).await?;
    async_write_u32(
        MASTER_CHANNEL,
        CONNECTION_CFG,
        current_cfg & 0xFFFF | recommended_channels << 16,
        false,
    )
    .await?;

    // Some cameras (e.g. Hamamatsu C15550-20UP) reset all high speed links when change channels
    // We will wait 200 ms to allow for camera serdes setup before Forcing a RX serdes reset
    // to make sure the serdes can lock properly
    timer::async_delay_ms(CAMERA_CONNECTION_SETUP_TIME).await;
    rx::change_linerate(discovery_speed);

    // Verify all connection id
    for ch in MASTER_CHANNEL..recommended_channels {
        monitor_channel_status_timeout(ch).await?;
        check_connection_id(ch).await?;
    }
    Ok(recommended_channels)
}

async fn check_connection_id(channel: u32) -> Result<(), Error> {
    let connection_id = async_read_u32(channel, DEVICE_CONNECTION_ID, false).await?;
    (connection_id == channel)
        .then(|| ())
        .ok_or(Error::ConnectionIDMismatch(channel, connection_id))
}

async fn set_host_connection_id() -> Result<(), Error> {
    debug!("set host connection id to = {:#X}", HOST_CONNECTION_ID);
    async_write_u32(MASTER_CHANNEL, MASTER_HOST_CONNECTION_ID, HOST_CONNECTION_ID, false).await?;
    Ok(())
}

async fn negotiate_cxp_version() -> Result<bool, Error> {
    let rev = async_read_u32(MASTER_CHANNEL, REVISION, false).await?;

    let mut major_rev: u32 = rev >> 16;
    let mut minor_rev: u32 = rev & 0xFFFF;
    debug!("camera's CoaXPress revision is {}.{}", major_rev, minor_rev);

    // Section 12.1.4 (CXP-001-2021)
    // For CXP 2.0 and onward, Host need to check the VersionSupported register to determine
    // the highest common version that supported by both device & host
    if major_rev >= 2 {
        let reg = async_read_u32(MASTER_CHANNEL, VERSION_SUPPORTED, false).await?;

        // grabber support CXP 2.1, 2.0 and 1.1 only
        if ((reg >> 3) & 1) == 1 {
            major_rev = 2;
            minor_rev = 1;
        } else if ((reg >> 2) & 1) == 1 {
            major_rev = 2;
            minor_rev = 0;
        } else if ((reg >> 1) & 1) == 1 {
            major_rev = 1;
            minor_rev = 1;
        } else {
            return Err(Error::UnsupportedVersion);
        }

        async_write_u32(MASTER_CHANNEL, VERSION_USED, major_rev << 16 | minor_rev, false).await?;
    }
    debug!(
        "both camera and cxp grabber support CoaXPress {}.{}, switch to CoaXPress {}.{} protocol now",
        major_rev, minor_rev, major_rev, minor_rev
    );

    Ok(major_rev >= 2)
}

async fn negotiate_pak_max_size(with_tag: bool) -> Result<(), Error> {
    async_write_u32(MASTER_CHANNEL, STREAM_PACKET_SIZE_MAX, MAX_STREAM_PAK_SIZE, with_tag).await?;
    Ok(())
}

fn decode_cxp_speed(linerate_code: u32) -> Result<CXPSpeed, Error> {
    let max_speed_code = match MAX_SPEED {
        CXPSpeed::CXP1 => 0x28,
        CXPSpeed::CXP2 => 0x30,
        CXPSpeed::CXP3 => 0x38,
        CXPSpeed::CXP5 => 0x40,
        CXPSpeed::CXP6 => 0x48,
        CXPSpeed::CXP10 => 0x50,
        CXPSpeed::CXP12 => 0x58,
    };

    let speed = match linerate_code {
        0x28 => CXPSpeed::CXP1,
        0x30 => CXPSpeed::CXP2,
        0x38 => CXPSpeed::CXP3,
        0x40 => CXPSpeed::CXP5,
        0x48 => CXPSpeed::CXP6,
        0x50 => CXPSpeed::CXP10,
        0x58 => CXPSpeed::CXP12,
        _ => return Err(Error::UnsupportedLinerateCode(linerate_code)),
    };

    if linerate_code > max_speed_code {
        Err(Error::UnsupportedSpeed(speed))
    } else {
        Ok(speed)
    }
}

async fn set_operation_linerate(channels: u32, with_tag: bool) -> Result<(), Error> {
    let recommended_linerate_code = async_read_u32(MASTER_CHANNEL, CONNECTION_CFG_DEFAULT, with_tag).await? & 0xFFFF;

    let speed = decode_cxp_speed(recommended_linerate_code)?;
    debug!("changing linerate to {}", speed);
    // preserve the number of active channels
    let current_cfg = async_read_u32(MASTER_CHANNEL, CONNECTION_CFG, with_tag).await?;
    async_write_u32(
        MASTER_CHANNEL,
        CONNECTION_CFG,
        current_cfg & 0xFFFF0000 | recommended_linerate_code,
        with_tag,
    )
    .await?;

    tx::change_linerate(speed);
    // We will wait 200 ms to allow for camera serdes setup before changing line
    // to make sure the rx serdes can lock properly
    timer::async_delay_ms(CAMERA_CONNECTION_SETUP_TIME).await;
    rx::change_linerate(speed);
    for ch in MASTER_CHANNEL..channels {
        monitor_channel_status_timeout(ch).await?;
    }
    Ok(())
}

async fn test_counter_reset(channel: u32, with_tag: bool) -> Result<(), Error> {
    unsafe { (CXP[channel as usize].rx_test_counts_reset_write)(1) };
    async_write_u32(MASTER_CHANNEL, TEST_ERROR_COUNT_SELECTOR, channel as u32, with_tag).await?;
    async_write_u32(MASTER_CHANNEL, TEST_ERROR_COUNT, 0, with_tag).await?;
    async_write_u64(MASTER_CHANNEL, TEST_PACKET_COUNT_TX, 0, with_tag).await?;
    async_write_u64(MASTER_CHANNEL, TEST_PACKET_COUNT_RX, 0, with_tag).await?;
    Ok(())
}

async fn verify_test_result(channel: u32, with_tag: bool) -> Result<(), Error> {
    async_write_u32(MASTER_CHANNEL, TEST_ERROR_COUNT_SELECTOR, channel as u32, with_tag).await?;

    // Section 9.9.3 (CXP-001-2021)
    // verify grabber -> camera connection test result
    if async_read_u64(MASTER_CHANNEL, TEST_PACKET_COUNT_RX, with_tag).await? != TX_TEST_CNT as u64 {
        return Err(Error::UnstableTX(channel));
    };
    if async_read_u32(MASTER_CHANNEL, TEST_ERROR_COUNT, with_tag).await? > 0 {
        return Err(Error::UnstableTX(channel));
    };

    // Section 9.9.4 (CXP-001-2021)
    // verify camera -> grabber connection test result
    let camera_test_pak_cnt = async_read_u64(MASTER_CHANNEL, TEST_PACKET_COUNT_TX, with_tag).await?;
    unsafe {
        let rx_test_pak_cnt = (CXP[channel as usize].rx_test_packet_counter_read)();
        debug!(
            "camera channel #{} sent {} test packets, CXP grabber received {} packets",
            channel,
            camera_test_pak_cnt & 0xFFFF,
            rx_test_pak_cnt,
        );
        if rx_test_pak_cnt != (camera_test_pak_cnt & 0xFFFF) as u16 {
            return Err(Error::UnstableRX(channel));
        };
        if (CXP[channel as usize].rx_test_error_counter_read)() > 0 {
            return Err(Error::UnstableRX(channel));
        };
    };
    debug!("channel #{} passed connection test", channel);
    Ok(())
}

async fn test_channels_stability(channels: u32, with_tag: bool) -> Result<(), Error> {
    for ch in MASTER_CHANNEL..channels {
        test_counter_reset(ch, with_tag).await?;

        // CXP grabber -> camera connection test
        for _ in 0..TX_TEST_CNT {
            send_test_packet(ch)?;
            // sending the whole test sequence @ 20.833Mbps will take a minimum of 1.972ms
            // and leave some room to send IDLE word
            timer::async_delay_ms(2).await;
        }
    }

    // camera -> CXP grabber connection test
    async_write_u32(MASTER_CHANNEL, TESTMODE, 1, with_tag).await?;
    timer::async_delay_ms(100).await;
    async_write_u32(MASTER_CHANNEL, TESTMODE, 0, with_tag).await?;

    for ch in MASTER_CHANNEL..channels {
        verify_test_result(ch, with_tag).await?;
    }

    Ok(())
}

async fn get_camera_workarounds(with_tag: bool) -> Result<(bool, Option<u8>), Error> {
    // Table 54 (CXP-001-2021)
    let mut bytes: [u8; 32] = [0; 32];
    async_read_bytes(MASTER_CHANNEL, DEVICE_MODEL_NAME, &mut bytes, with_tag).await?;
    // String is null terminated with ascii encoding - Section 12.3.1 (CXP-001-2021)
    match unsafe { core::str::from_utf8_unchecked(&bytes) }.split("\0").next() {
        // List of non CoaXPress compliant cameras:
        // Hamamatsu C15550-20UP with G3.10 firmware version:
        // - StreamPacketSizeMax is not honored and the DsizeP is always 2035 (this will break grabber implementation with small FIFO size)
        // - StreamID in Stream data payload header is different each transmission and it should be matching StreamID in Rectangular image header
        // - StreamID in Rectangular image header is set to 0x01 instead of the recommanded 0x00 (Section 13.2.2.8 CXP-001-2021)
        // - Image1StreamID register is 0x00 instead of matching the 0x01 in the Rectangular image header
        // - In CXP6 stability test, TestPacketCountTx[3] reg is different from other TestPacketCountTx regs.
        // => bypass stability test and enable stream passthrough without stream id meeting
        Some(name @ "C15550-20UP") => {
            debug!(
                "{} is a non-compliant CoaXPress camera, implementing workaround...",
                name
            );
            Ok((true, None))
        }
        // Hamamatsu C15550-22UP:
        // - Stability test passes correctly on this model (unlike the 20UP).
        // - However, the Stream ID in the payload header is still non-compliant/mismatched.
        // => Enforce stability test (false), but enable stream passthrough by returning None for stream_id.
        Some(name @ "C15550-22UP") => {
            debug!(
                "{} is a non-compliant CoaXPress camera, implementing workaround...",
                name
            );
            Ok((false, None))
        }
        _ => {
            let addr = async_read_u32(MASTER_CHANNEL, IMAGE_1_STREAM_ID_ADDRESS, with_tag).await?;
            let stream_id = (async_read_u32(MASTER_CHANNEL, addr, with_tag).await? & 0xFF) as u8;
            debug!("image1 stream id = {:#x}", stream_id);
            Ok((false, Some(stream_id)))
        }
    }
}

pub async fn camera_setup(discovery_speed: CXPSpeed) -> Result<(u32, Option<u8>, bool), Error> {
    reset_tag();
    let active_channels = check_connection_topology(discovery_speed).await?;

    set_host_connection_id().await?;
    let with_tag = negotiate_cxp_version().await?;

    negotiate_pak_max_size(with_tag).await?;
    set_operation_linerate(active_channels, with_tag).await?;

    let (ignore_test_result, stream_id) = get_camera_workarounds(with_tag).await?;
    test_channels_stability(active_channels, with_tag)
        .await
        .or_else(|e| if ignore_test_result { Ok(()) } else { Err(e) })?;

    Ok((active_channels, stream_id, with_tag))
}
