// ARTIQ 8 port note: the upstream (ARTIQ 10) version of this file used the
// `embedded-io` crate (Read/Write/ReadExactError) and the endianness-generic
// `io::proto` methods (`read_u32::<NetworkEndian>()`). Neither is available on the
// ARTIQ 8 firmware toolchain: `embedded-io` requires rustc >= 1.81 (v8 is ~1.51), and
// v8's in-repo `io::proto` is NativeEndian-only. This port instead uses the v8-native
// `core_io::{Read, Write}` traits (which `io::Cursor` implements) plus `byteorder` for
// the (network-endian) integer (de)serialization. Behaviour is unchanged.

use core::fmt;

use byteorder::{ByteOrder, NetworkEndian};
use crc::crc32::checksum_ieee;
use core_io::{Error as IoError, Read, Write};
use io::Cursor;

pub const CTRL_PACKET_MAXSIZE: usize = 128; // for compatibility with version1.x compliant Devices - Section 12.1.6 (CXP-001-2021)
pub const DATA_MAXSIZE: usize =
    CTRL_PACKET_MAXSIZE - /*packet start KCodes, data packet types, CMD, Tag, Addr, CRC, packet end KCode*/4*7;

pub enum Error {
    CorruptedPacket,
    CtrlAckError(u8),
    Io(IoError),
    LengthOutOfRange(u32),
    TagMismatch,
    TimedOut,
    UnexpectedReply,
    UnknownPacket(u8),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &Error::CorruptedPacket => write!(f, "CorruptedPacket - Received packet fail CRC test"),
            &Error::CtrlAckError(ref ack_code) => match ack_code {
                0x40 => write!(f, "CtrlAckError - Invalid Address"),
                0x41 => write!(f, "CtrlAckError - Invalid data for the address"),
                0x42 => write!(f, "CtrlAckError - Invalid operation code"),
                0x43 => write!(f, "CtrlAckError - Write attempted to a read-only address"),
                0x44 => write!(f, "CtrlAckError - Read attempted from a write-only address"),
                0x45 => write!(f, "CtrlAckError - Size field too large, exceed packet size limit"),
                0x46 => write!(f, "CtrlAckError - Message size is inconsistent with size field"),
                0x47 => write!(f, "CtrlAckError - Malformed packet"),
                0x80 => write!(f, "CtrlAckError - Failed CRC test in last received command"),
                _ => write!(f, "CtrlAckError - Unknown ack code {:#X}", ack_code),
            },
            &Error::Io(ref err) => write!(f, "IoError - {:?}", err),
            &Error::LengthOutOfRange(length) => write!(
                f,
                "LengthOutOfRange - Message length {} is not between 1 and {}",
                length, DATA_MAXSIZE
            ),
            &Error::TagMismatch => write!(f, "TagMismatch - Received tag is different from the transmitted tag"),
            &Error::TimedOut => write!(f, "MessageTimedOut"),
            &Error::UnexpectedReply => write!(f, "UnexpectedReply"),
            &Error::UnknownPacket(packet_type) => {
                write!(f, "UnknownPacket - Unknown packet type id {:#X} ", packet_type)
            }
        }
    }
}

impl From<IoError> for Error {
    fn from(err: IoError) -> Self {
        Self::Io(err)
    }
}

fn get_cxp_crc(bytes: &[u8]) -> u32 {
    // Section 9.2.2.2 (CXP-001-2021)
    // Only Control packet need CRC32 appended in the end of the packet
    // CoaXpress use the polynomial of IEEE-802.3 (Ethernet) CRC but the checksum calculation is different
    (!checksum_ieee(bytes)).swap_bytes()
}

trait CxpRead: Read {
    fn read_exact_4x(&mut self, buf: &mut [u8]) -> Result<(), IoError> {
        let mut temp = [0u8; 4];
        for byte in buf {
            // Section 9.2.2.1 (CXP-001-2021)
            // decoder should immune to single bit errors when handling 4x duplicated characters
            self.read_exact(&mut temp)?;
            let [a, b, c, d] = temp;
            // vote and return majority
            *byte = a & b & c | a & b & d | a & c & d | b & c & d;
        }
        Ok(())
    }

    fn read_4x_u8(&mut self) -> Result<u8, IoError> {
        let mut bytes = [0; 1];
        self.read_exact_4x(&mut bytes)?;
        Ok(bytes[0])
    }
}

impl<T: Read> CxpRead for T {}
impl<T: Write> CxpWrite for T {}

#[derive(Debug)]
pub enum RXCTRLPacket {
    CtrlReply {
        tag: Option<u8>,
        length: u32,
        data: [u8; DATA_MAXSIZE],
    },
    CtrlDelay {
        tag: Option<u8>,
        time: u32,
    },
    CtrlAck {
        tag: Option<u8>,
    },
}

impl RXCTRLPacket {
    pub fn read_from(reader: &mut Cursor<&[u8]>) -> Result<Self, Error> {
        match reader.read_4x_u8()? {
            0x03 => RXCTRLPacket::get_ctrl_packet(reader, false),
            0x06 => RXCTRLPacket::get_ctrl_packet(reader, true),
            ty => Err(Error::UnknownPacket(ty)),
        }
    }

    fn get_ctrl_packet(reader: &mut Cursor<&[u8]>, with_tag: bool) -> Result<Self, Error> {
        let mut tag: Option<u8> = None;
        if with_tag {
            tag = Some(reader.read_4x_u8()?);
        }

        let ackcode = reader.read_4x_u8()?;

        match ackcode {
            0x00 | 0x04 => {
                let mut len_bytes = [0u8; 4];
                reader.read_exact(&mut len_bytes)?;
                let length = NetworkEndian::read_u32(&len_bytes);
                let mut data: [u8; DATA_MAXSIZE] = [0; DATA_MAXSIZE];
                reader.read(&mut data[0..length as usize])?;

                // Section 9.6.3 (CXP-001-2021)
                // when length is not multiple of 4, dummy bits are padded to align to the word boundary
                // set position to next word boundary for CRC calculation
                let padding = (4 - (reader.position() % 4)) % 4;
                reader.set_position(reader.position() + padding);

                // Section 9.6.3 (CXP-001-2021)
                // only bytes after the first 4 are used in calculating the checksum
                let checksum = get_cxp_crc(&reader.get_ref()[4..reader.position()]);
                let mut crc_bytes = [0u8; 4];
                reader.read_exact(&mut crc_bytes)?;
                if NetworkEndian::read_u32(&crc_bytes) != checksum {
                    return Err(Error::CorruptedPacket);
                }

                if ackcode == 0x00 {
                    return Ok(RXCTRLPacket::CtrlReply { tag, length, data });
                } else {
                    return Ok(RXCTRLPacket::CtrlDelay {
                        tag,
                        time: NetworkEndian::read_u32(&data[..4]),
                    });
                }
            }
            0x01 => return Ok(RXCTRLPacket::CtrlAck { tag }),
            _ => return Err(Error::CtrlAckError(ackcode)),
        }
    }
}

trait CxpWrite: Write {
    fn write_all_4x(&mut self, buf: &[u8]) -> Result<(), IoError> {
        for byte in buf {
            self.write_all(&[*byte; 4])?;
        }
        Ok(())
    }

    fn write_4x_u8(&mut self, value: u8) -> Result<(), IoError> {
        self.write_all_4x(&[value])
    }
}

#[derive(Debug)]
pub enum TXCTRLPacket {
    CtrlRead {
        tag: Option<u8>,
        addr: u32,
        length: u32,
    },
    CtrlWrite {
        tag: Option<u8>,
        addr: u32,
        length: u32,
        data: [u8; DATA_MAXSIZE],
    },
}

impl TXCTRLPacket {
    pub fn write_to(&self, writer: &mut Cursor<&mut [u8]>) -> Result<(), Error> {
        match *self {
            TXCTRLPacket::CtrlRead { tag, addr, length } => {
                match tag {
                    Some(t) => {
                        writer.write_4x_u8(0x05)?;
                        writer.write_4x_u8(t)?;
                    }
                    None => {
                        writer.write_4x_u8(0x02)?;
                    }
                }

                let mut bytes = [0; 3];
                NetworkEndian::write_u24(&mut bytes, length);
                writer.write_all(&[0x00, bytes[0], bytes[1], bytes[2]])?;

                let mut addr_bytes = [0u8; 4];
                NetworkEndian::write_u32(&mut addr_bytes, addr);
                writer.write_all(&addr_bytes)?;

                // Section 9.6.2 (CXP-001-2021)
                // only bytes after the first 4 are used in calculating the checksum
                let checksum = get_cxp_crc(&writer.get_ref()[4..writer.position()]);
                let mut crc_bytes = [0u8; 4];
                NetworkEndian::write_u32(&mut crc_bytes, checksum);
                writer.write_all(&crc_bytes)?;
            }
            TXCTRLPacket::CtrlWrite {
                tag,
                addr,
                length,
                data,
            } => {
                match tag {
                    Some(t) => {
                        writer.write_4x_u8(0x05)?;
                        writer.write_4x_u8(t)?;
                    }
                    None => {
                        writer.write_4x_u8(0x02)?;
                    }
                }

                let mut bytes = [0; 3];
                NetworkEndian::write_u24(&mut bytes, length);
                writer.write_all(&[0x01, bytes[0], bytes[1], bytes[2]])?;

                let mut addr_bytes = [0u8; 4];
                NetworkEndian::write_u32(&mut addr_bytes, addr);
                writer.write_all(&addr_bytes)?;
                writer.write_all(&data[0..length as usize])?;

                // Section 9.6.2 (CXP-001-2021)
                // when length is not multiple of 4, dummy bites are padded to align to the word boundary
                let padding = (4 - (writer.position() % 4)) % 4;
                for _ in 0..padding {
                    writer.write_all(&[0])?;
                }

                // Section 9.6.2 (CXP-001-2021)
                // only bytes after the first 4 are used in calculating the checksum
                let checksum = get_cxp_crc(&writer.get_ref()[4..writer.position()]);
                let mut crc_bytes = [0u8; 4];
                NetworkEndian::write_u32(&mut crc_bytes, checksum);
                writer.write_all(&crc_bytes)?;
            }
        }
        Ok(())
    }
}
