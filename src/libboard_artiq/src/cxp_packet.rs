use core::slice;

use byteorder::{ByteOrder, NetworkEndian};
use io::Cursor;
use libasync::task;
use crate::cxp_compat as timer;

use crate::{cxp_ctrl::{CTRL_PACKET_MAXSIZE, DATA_MAXSIZE, Error, RXCTRLPacket, TXCTRLPacket},
            mem::mem::CXP_MEM,
            pl::csr::CXP};

const TRANSMISSION_TIMEOUT: u64 = 200;

// Section 9.6.1.2 (CXP-001-2021)
// CTRL packet need to be tagged for CXP 2.0 or greater
static mut TAG: u8 = 0;

pub fn reset_tag() {
    unsafe { TAG = 0 };
}

fn increment_tag() {
    unsafe { TAG = TAG.wrapping_add(1) };
}

fn check_tag(tag: Option<u8>) -> Result<(), Error> {
    unsafe {
        if tag.is_some() && tag != Some(TAG) {
            Err(Error::TagMismatch)
        } else {
            Ok(())
        }
    }
}

fn receive_ctrl_packet(channel: u32) -> Result<Option<RXCTRLPacket>, Error> {
    if unsafe { (CXP[channel as usize].rx_pending_packet_read)() == 1 } {
        unsafe {
            let read_buffer_ptr = (CXP[channel as usize].rx_read_ptr_read)() as usize;
            let ptr = (CXP_MEM[channel as usize].base
                + CXP_MEM[channel as usize].size / 2
                + read_buffer_ptr * CTRL_PACKET_MAXSIZE) as *mut u32;

            let mut reader = Cursor::new(slice::from_raw_parts(ptr as *const u8, CTRL_PACKET_MAXSIZE));
            let packet = RXCTRLPacket::read_from(&mut reader);

            (CXP[channel as usize].rx_pending_packet_write)(1);
            Ok(Some(packet?))
        }
    } else {
        Ok(None)
    }
}

fn receive_ctrl_packet_timeout(channel: u32, timeout_ms: u64) -> Result<RXCTRLPacket, Error> {
    // assume timer was initialized successfully
    let limit = timer::get_ms() + timeout_ms;
    while timer::get_ms() < limit {
        match receive_ctrl_packet(channel)? {
            None => (),
            Some(packet) => return Ok(packet),
        }
    }
    Err(Error::TimedOut)
}

async fn async_receive_ctrl_packet_timeout(channel: u32, timeout_ms: u64) -> Result<RXCTRLPacket, Error> {
    // assume timer was initialized successfully
    let limit = timer::get_ms() + timeout_ms;
    while timer::get_ms() < limit {
        match receive_ctrl_packet(channel)? {
            None => (),
            Some(packet) => return Ok(packet),
        }
        task::r#yield().await;
    }
    Err(Error::TimedOut)
}

fn send_ctrl_packet(channel: u32, packet: &TXCTRLPacket) -> Result<(), Error> {
    unsafe {
        while (CXP[channel as usize].tx_writer_busy_read)() == 1 {}
        let ptr = CXP_MEM[channel as usize].base as *mut u32;
        let mut writer = Cursor::new(slice::from_raw_parts_mut(ptr as *mut u8, CTRL_PACKET_MAXSIZE));

        packet.write_to(&mut writer)?;

        (CXP[channel as usize].tx_writer_word_len_write)((writer.position() / 4) as u8);
        (CXP[channel as usize].tx_writer_stb_write)(1);
    }

    Ok(())
}

pub fn send_test_packet(channel: u32) -> Result<(), Error> {
    unsafe {
        while (CXP[channel as usize].tx_writer_busy_read)() == 1 {}
        (CXP[channel as usize].tx_writer_stb_testseq_write)(1);
    }
    Ok(())
}

fn get_ctrl_ack(packet: RXCTRLPacket, timeout_ms: &mut u64) -> Result<bool, Error> {
    match packet {
        RXCTRLPacket::CtrlDelay { tag, time } => {
            check_tag(tag)?;
            *timeout_ms = time.into();
            Ok(false)
        }
        RXCTRLPacket::CtrlAck { tag } => {
            check_tag(tag)?;
            Ok(true)
        }
        _ => Err(Error::UnexpectedReply),
    }
}

fn get_ctrl_reply(
    packet: RXCTRLPacket,
    timeout_ms: &mut u64,
    expected_length: u32,
) -> Result<Option<[u8; DATA_MAXSIZE]>, Error> {
    match packet {
        RXCTRLPacket::CtrlDelay { tag, time } => {
            check_tag(tag)?;
            *timeout_ms = time.into();
            Ok(None)
        }
        RXCTRLPacket::CtrlReply {
            tag,
            length: replied_length,
            data,
        } => {
            check_tag(tag)?;
            if replied_length != expected_length {
                return Err(Error::UnexpectedReply);
            };
            Ok(Some(data))
        }
        _ => Err(Error::UnexpectedReply),
    }
}

fn check_length(length: u32) -> Result<(), Error> {
    if length > DATA_MAXSIZE as u32 || length == 0 {
        Err(Error::LengthOutOfRange(length))
    } else {
        Ok(())
    }
}

pub fn write_bytes_no_ack(channel: u32, addr: u32, val: &[u8], with_tag: bool) -> Result<(), Error> {
    let length = val.len() as u32;
    check_length(length)?;

    let mut data: [u8; DATA_MAXSIZE] = [0; DATA_MAXSIZE];
    data[..length as usize].clone_from_slice(val);

    let tag: Option<u8> = if with_tag { Some(unsafe { TAG }) } else { None };
    send_ctrl_packet(
        channel,
        &TXCTRLPacket::CtrlWrite {
            tag,
            addr,
            length,
            data,
        },
    )
}

fn write_bytes(channel: u32, addr: u32, val: &[u8], with_tag: bool) -> Result<(), Error> {
    write_bytes_no_ack(channel, addr, val, with_tag)?;

    let mut timeout_ms = TRANSMISSION_TIMEOUT;
    loop {
        let packet = receive_ctrl_packet_timeout(channel, timeout_ms)?;
        if get_ctrl_ack(packet, &mut timeout_ms)? {
            break;
        }
    }

    if with_tag {
        increment_tag();
    };
    Ok(())
}

pub fn write_u32(channel: u32, addr: u32, val: u32, with_tag: bool) -> Result<(), Error> {
    write_bytes(channel, addr, &val.to_be_bytes(), with_tag)
}

async fn async_write_bytes(channel: u32, addr: u32, val: &[u8], with_tag: bool) -> Result<(), Error> {
    write_bytes_no_ack(channel, addr, val, with_tag)?;

    let mut timeout_ms = TRANSMISSION_TIMEOUT;
    loop {
        let packet = async_receive_ctrl_packet_timeout(channel, timeout_ms).await?;
        if get_ctrl_ack(packet, &mut timeout_ms)? {
            break;
        }
    }

    if with_tag {
        increment_tag();
    };
    Ok(())
}

pub async fn async_write_u32(channel: u32, addr: u32, val: u32, with_tag: bool) -> Result<(), Error> {
    async_write_bytes(channel, addr, &val.to_be_bytes(), with_tag).await
}

pub async fn async_write_u64(channel: u32, addr: u32, val: u64, with_tag: bool) -> Result<(), Error> {
    async_write_bytes(channel, addr, &val.to_be_bytes(), with_tag).await
}

pub fn read_bytes(channel: u32, addr: u32, bytes: &mut [u8], with_tag: bool) -> Result<(), Error> {
    let length = bytes.len() as u32;
    check_length(length)?;
    let tag: Option<u8> = if with_tag { Some(unsafe { TAG }) } else { None };
    send_ctrl_packet(channel, &TXCTRLPacket::CtrlRead { tag, addr, length })?;

    let mut timeout_ms = TRANSMISSION_TIMEOUT;
    loop {
        let packet = receive_ctrl_packet_timeout(channel, timeout_ms)?;
        if let Some(data) = get_ctrl_reply(packet, &mut timeout_ms, length)? {
            bytes.copy_from_slice(&data[..length as usize]);
            break;
        }
    }

    if with_tag {
        increment_tag();
    };
    Ok(())
}

pub fn read_u32(channel: u32, addr: u32, with_tag: bool) -> Result<u32, Error> {
    let mut bytes: [u8; 4] = [0; 4];
    read_bytes(channel, addr, &mut bytes, with_tag)?;
    let val = NetworkEndian::read_u32(&bytes);

    Ok(val)
}

pub async fn async_read_bytes(channel: u32, addr: u32, bytes: &mut [u8], with_tag: bool) -> Result<(), Error> {
    let length = bytes.len() as u32;
    check_length(length)?;
    let tag: Option<u8> = if with_tag { Some(unsafe { TAG }) } else { None };
    send_ctrl_packet(channel, &TXCTRLPacket::CtrlRead { tag, addr, length })?;

    let mut timeout_ms = TRANSMISSION_TIMEOUT;
    loop {
        let packet = async_receive_ctrl_packet_timeout(channel, timeout_ms).await?;
        if let Some(data) = get_ctrl_reply(packet, &mut timeout_ms, length)? {
            bytes.copy_from_slice(&data[..length as usize]);
            break;
        }
    }
    if with_tag {
        increment_tag();
    };
    Ok(())
}

pub async fn async_read_u32(channel: u32, addr: u32, with_tag: bool) -> Result<u32, Error> {
    let mut bytes: [u8; 4] = [0; 4];
    async_read_bytes(channel, addr, &mut bytes, with_tag).await?;
    let val = NetworkEndian::read_u32(&bytes);

    Ok(val)
}

pub async fn async_read_u64(channel: u32, addr: u32, with_tag: bool) -> Result<u64, Error> {
    let mut bytes: [u8; 8] = [0; 8];
    async_read_bytes(channel, addr, &mut bytes, with_tag).await?;
    let val = NetworkEndian::read_u64(&bytes);

    Ok(val)
}
