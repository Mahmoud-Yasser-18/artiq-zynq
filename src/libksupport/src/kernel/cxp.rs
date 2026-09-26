use alloc::{format,
            string::{String, ToString},
            vec::Vec};
use core::fmt;

use byteorder::{ByteOrder, NetworkEndian};
use libboard_artiq::drtioaux_proto::CXP_PAYLOAD_MAX_SIZE;
#[cfg(has_cxp_grabber)]
use libboard_artiq::{cxp_camera_setup::MASTER_CHANNEL,
                     cxp_ctrl::DATA_MAXSIZE,
                     cxp_grabber::{camera_connected, roi_viewer_setup, with_tag},
                     cxp_packet::{read_bytes, read_u32, write_u32}};
use log::info;

#[cfg(has_drtio)]
use super::{KERNEL_CHANNEL_0TO1, KERNEL_CHANNEL_1TO0, Message};
#[cfg(has_cxp_grabber)]
use crate::pl::csr::cxp_grabber;
// ARTIQ 8: kernel `list` args arrive as `cslice::CSlice<T>` (v10 used
// `kernel::refcounting::List<T>`, which does not exist here).
use cslice::CSlice;
use crate::artiq_raise;

const ROI_MAX_SIZE: usize = 4096;

#[repr(C)]
pub struct ROIViewerFrame {
    width: i32,
    height: i32,
    pixel_width: i32,
}

enum Error {
    BufferSizeTooSmall(usize, usize),
    ROISizeTooBig(usize, usize),
    InvalidLocalUrl(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &Error::BufferSizeTooSmall(required_size, buffer_size) => {
                write!(
                    f,
                    "BufferSizeTooSmall - The required size is {} bytes but the buffer size is {} bytes",
                    required_size, buffer_size
                )
            }
            &Error::ROISizeTooBig(width, height) => {
                write!(
                    f,
                    "ROISizeTooBig - The maximum ROIViewer height and total size are {} and {} pixels respectively \
                     but the ROI is set to {} ({}x{}) pixels",
                    ROI_MAX_SIZE / 4,
                    ROI_MAX_SIZE,
                    width * height,
                    width,
                    height
                )
            }
            &Error::InvalidLocalUrl(ref s) => {
                write!(f, "InvalidLocalUrl - Cannot download xml file locally from {}", s)
            }
        }
    }
}

fn read_xml_url<F>(read_bytes_f: F) -> Result<String, Error>
where F: Fn(u32, &mut [u8]) {
    let mut bytes: [u8; 4] = [0; 4];
    read_bytes_f(0x0018, &mut bytes);
    let mut addr = NetworkEndian::read_u32(&bytes);
    let mut buffer = Vec::new();

    // Strings stored in the bootstrap and manufacturer-specific registers space shall be NULL-terminated, encoded ASCII - Section 12.3.1 (CXP-001-2021)
    // String length is not known during runtime, grabber must read 4 bytes at a time until NULL-terminated
    loop {
        let mut bytes: [u8; 4] = [0; 4];
        read_bytes_f(addr, &mut bytes);
        addr += 4;

        // NOTE(ARTIQ 8): iterate by reference — by-value array IntoIterator is not
        // available on the v8 rustc/edition.
        for &b in bytes.iter() {
            if b == 0 {
                // UTF-8 is compatible with ASCII encoding
                // use U+FFFD REPLACEMENT_CHARACTER to represent decoding error
                return Ok(String::from_utf8_lossy(&buffer).to_string());
            } else {
                buffer.push(b);
            }
        }
    }
}

fn read_xml_location(url: String) -> Result<(String, u32, u32), Error> {
    // url example - Section 13.2.3 (CXP-001-2021)
    // Available on camera - "Local:MyFilename.zip;B8000;33A?SchemaVersion=1.0.0"
    // => ZIP file starting at address 0xB8000 in the Device with a length of 0x33A bytes
    //
    // Available online - "Web:http://www.example.com/xml/MyFilename.xml"
    // => xml is available at http://www.example.com/xml/MyFilename.xml
    let mut splitter = url.split(|c| c == ':' || c == ';' || c == '?');
    let scheme = splitter.next().unwrap();
    if scheme.eq_ignore_ascii_case("local") {
        if let (Some(file_name), Some(addr_str), Some(size_str)) = (splitter.next(), splitter.next(), splitter.next()) {
            let addr = u32::from_str_radix(addr_str, 16).map_err(|_| Error::InvalidLocalUrl(url.to_string()))?;
            let size = u32::from_str_radix(size_str, 16).map_err(|_| Error::InvalidLocalUrl(url.to_string()))?;
            return Ok((file_name.to_string(), addr, size));
        }
    }
    Err(Error::InvalidLocalUrl(url.to_string()))
}

fn read_xml_file<F>(buffer: &mut [i32], read_bytes_f: F, max_read_length: usize) -> Result<u32, Error>
where F: Fn(u32, &mut [u8]) {
    let url = read_xml_url(&read_bytes_f)?;
    let (file_name, base_addr, size) = read_xml_location(url)?;

    if buffer.len() * 4 < size as usize {
        return Err(Error::BufferSizeTooSmall(size as usize, buffer.len() * 4).into());
    };

    info!("downloading xml file {} with {} bytes...", file_name, size);
    let mut v: Vec<u8> = Vec::new();
    let mut addr = base_addr;
    let mut bytesleft = size;
    let mut bytes: [u8; CXP_PAYLOAD_MAX_SIZE] = [0; CXP_PAYLOAD_MAX_SIZE];

    while bytesleft > 0 {
        let read_len = max_read_length.min(bytesleft as usize);
        read_bytes_f(addr, &mut bytes[..read_len]);
        v.extend(&bytes[..read_len]);
        addr += read_len as u32;
        bytesleft -= read_len as u32;
    }
    info!("download successful");

    // pad to 32 bit boundary
    let padding = (4 - (size % 4)) % 4;
    for _ in 0..padding {
        v.push(0);
    }

    NetworkEndian::read_i32_into(&v, &mut buffer[..((size + padding) / 4) as usize]);
    Ok((size + padding) / 4)
}

#[cfg(has_drtio)]
fn kernel_channel_transact(content: Message) -> Message {
    unsafe {
        KERNEL_CHANNEL_1TO0.as_mut().unwrap().send(content);
        KERNEL_CHANNEL_0TO1.as_mut().unwrap().recv()
    }
}
#[cfg(has_drtio)]
fn drtio_read_bytes(dest: u8, addr: u32, bytes: &mut [u8]) {
    let length = bytes.len() as u16;
    if length as usize > CXP_PAYLOAD_MAX_SIZE {
        panic!("CXPReadRequest length is too long")
    }

    match kernel_channel_transact(Message::CXPReadRequest {
        destination: dest,
        address: addr,
        length,
    }) {
        Message::CXPReadReply { length, data } => {
            bytes.copy_from_slice(&data[..length as usize]);
        }
        Message::CXPError(err_msg) => artiq_raise!("CXPError", err_msg),
        _ => unreachable!(),
    };
}

pub extern "C" fn download_xml_file(dest: i32, buffer: &mut CSlice<i32>) -> i32 {
    let buf = unsafe {
        let s = buffer.as_ref();
        core::slice::from_raw_parts_mut(s.as_ptr() as *mut i32, s.len())
    };
    match dest {
        0 => {
            #[cfg(has_cxp_grabber)]
            {
                if !camera_connected() {
                    artiq_raise!("CXPError", "Camera is not connected");
                };
                match read_xml_file(
                    buf,
                    |addr, bytes| {
                        if let Err(e) = read_bytes(MASTER_CHANNEL, addr, bytes, with_tag()) {
                            artiq_raise!("CXPError", format!("{}", e));
                        };
                    },
                    DATA_MAXSIZE,
                ) {
                    Ok(size_read) => size_read as i32,
                    Err(e) => artiq_raise!("CXPError", format!("{}", e)),
                }
            }
            #[cfg(not(has_cxp_grabber))]
            artiq_raise!("CXPError", "CXP Grabber is not available on destination 0");
        }
        _ => {
            #[cfg(has_drtio)]
            {
                match read_xml_file(
                    buf,
                    |addr, bytes| drtio_read_bytes(dest as u8, addr, bytes),
                    CXP_PAYLOAD_MAX_SIZE,
                ) {
                    Ok(size_read) => size_read as i32,
                    Err(e) => artiq_raise!("CXPError", format!("{}", e)),
                }
            }
            #[cfg(not(has_drtio))]
            artiq_raise!("CXPError", "Destination cannot be reached");
        }
    }
}

pub extern "C" fn read32(dest: i32, addr: i32) -> i32 {
    match dest {
        0 => {
            #[cfg(has_cxp_grabber)]
            {
                if !camera_connected() {
                    artiq_raise!("CXPError", "Camera is not connected");
                };
                match read_u32(MASTER_CHANNEL, addr as u32, with_tag()) {
                    Ok(result) => result as i32,
                    Err(e) => artiq_raise!("CXPError", format!("{}", e)),
                }
            }
            #[cfg(not(has_cxp_grabber))]
            artiq_raise!("CXPError", "CXP Grabber is not available on destination 0");
        }
        _ => {
            #[cfg(has_drtio)]
            {
                let mut bytes: [u8; 4] = [0; 4];
                drtio_read_bytes(dest as u8, addr as u32, &mut bytes);
                NetworkEndian::read_i32(&bytes)
            }
            #[cfg(not(has_drtio))]
            artiq_raise!(
                "CXPError",
                format!("DRTIO is not avaiable, destination {} cannot be reached", dest)
            );
        }
    }
}

pub extern "C" fn write32(dest: i32, addr: i32, val: i32) {
    match dest {
        0 => {
            #[cfg(has_cxp_grabber)]
            {
                if !camera_connected() {
                    artiq_raise!("CXPError", "Camera is not connected");
                };
                match write_u32(MASTER_CHANNEL, addr as u32, val as u32, with_tag()) {
                    Ok(_) => {}
                    Err(e) => artiq_raise!("CXPError", format!("{}", e)),
                }
            }
            #[cfg(not(has_cxp_grabber))]
            artiq_raise!("CXPError", "CXP Grabber is not available on destination 0");
        }
        _ => {
            #[cfg(has_drtio)]
            {
                match kernel_channel_transact(Message::CXPWrite32Request {
                    destination: dest as u8,
                    address: addr as u32,
                    value: val as u32,
                }) {
                    Message::CXPWrite32Reply => return,
                    Message::CXPError(err_msg) => artiq_raise!("CXPError", err_msg),
                    _ => unreachable!(),
                }
            }
            #[cfg(not(has_drtio))]
            artiq_raise!(
                "CXPError",
                format!("DRTIO is not avaiable, destination {} cannot be reached", dest)
            );
        }
    }
}

pub extern "C" fn start_roi_viewer(dest: i32, x0: i32, y0: i32, x1: i32, y1: i32) {
    let (width, height) = ((x1 - x0) as usize, (y1 - y0) as usize);
    if width * height > ROI_MAX_SIZE || height > ROI_MAX_SIZE / 4 {
        artiq_raise!("CXPError", format!("{}", Error::ROISizeTooBig(width, height)));
    }

    match dest {
        0 => {
            #[cfg(has_cxp_grabber)]
            {
                roi_viewer_setup(x0 as u16, y0 as u16, x1 as u16, y1 as u16)
            }
            #[cfg(not(has_cxp_grabber))]
            artiq_raise!("CXPError", "CXP Grabber is not available on destination 0");
        }
        _ => {
            #[cfg(has_drtio)]
            {
                match kernel_channel_transact(Message::CXPROIViewerSetupRequest {
                    destination: dest as u8,
                    x0: x0 as u16,
                    y0: y0 as u16,
                    x1: x1 as u16,
                    y1: y1 as u16,
                }) {
                    Message::CXPROIViewerSetupReply => return,
                    _ => unreachable!(),
                }
            }
            #[cfg(not(has_drtio))]
            artiq_raise!(
                "CXPError",
                format!("DRTIO is not avaiable, destination {} cannot be reached", dest)
            );
        }
    }
}

pub extern "C" fn download_roi_viewer_frame(dest: i32, buffer: &mut CSlice<i64>) -> ROIViewerFrame {
    let buf = unsafe {
        let s = buffer.as_ref();
        core::slice::from_raw_parts_mut(s.as_ptr() as *mut i64, s.len())
    };
    if buf.len() * 4 < ROI_MAX_SIZE {
        // each pixel is 16 bits
        artiq_raise!(
            "CXPError",
            format!("{}", Error::BufferSizeTooSmall(ROI_MAX_SIZE * 2, buf.len() * 8))
        );
    };

    let (width, height, pixel_code);
    match dest {
        0 => {
            #[cfg(has_cxp_grabber)]
            unsafe {
                while cxp_grabber::roi_viewer_ready_read() == 0 {}
                let mut i = 0;
                while cxp_grabber::roi_viewer_fifo_stb_read() == 1 {
                    buf[i] = cxp_grabber::roi_viewer_fifo_data_read() as i64;
                    i += 1;
                    cxp_grabber::roi_viewer_fifo_ack_write(1);
                }
                cxp_grabber::roi_viewer_ready_write(1);

                width = cxp_grabber::roi_viewer_x1_read() - cxp_grabber::roi_viewer_x0_read();
                height = cxp_grabber::roi_viewer_y1_read() - cxp_grabber::roi_viewer_y0_read();
                pixel_code = cxp_grabber::stream_decoder_pixel_format_code_read();
            }
            #[cfg(not(has_cxp_grabber))]
            artiq_raise!("CXPError", "CXP Grabber is not available on destination 0");
        }
        _ => {
            #[cfg(has_drtio)]
            {
                let mut i = 0;
                loop {
                    match kernel_channel_transact(Message::CXPROIViewerDataRequest {
                        destination: dest as u8,
                    }) {
                        Message::CXPROIVIewerPixelDataReply { length, data } => {
                            for d in &data[..length as usize] {
                                buf[i] = *d as i64;
                                i += 1;
                            }
                        }
                        Message::CXPROIVIewerFrameDataReply {
                            width: w,
                            height: h,
                            pixel_code: p,
                        } => {
                            (width, height, pixel_code) = (w, h, p);
                            break;
                        }
                        _ => unreachable!(),
                    }
                }
            }
            #[cfg(not(has_drtio))]
            artiq_raise!(
                "CXPError",
                format!("DRTIO is not avaiable, destination {} cannot be reached", dest)
            );
        }
    };
    let pixel_width = match pixel_code {
        0x0101 => 8,
        0x0102 => 10,
        0x0103 => 12,
        0x0104 => 14,
        0x0105 => 16,
        _ => artiq_raise!("CXPError", "UnsupportedPixelFormat"),
    };
    ROIViewerFrame {
        width: width as i32,
        height: height as i32,
        pixel_width: pixel_width as i32,
    }
}
