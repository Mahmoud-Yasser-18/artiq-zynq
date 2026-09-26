use libboard_zynq::i2c;

use crate::cxp_compat as timer;
use libcortex_a9::mutex::Mutex;
use log::{error, info};

#[cfg(has_cxp_led)]
use crate::cxp_led::{LEDState, update_led};
#[cfg(has_cxp_led)]
use crate::pl::csr::CXP_LEN;
use crate::{cxp_camera_setup::{MASTER_CHANNEL, camera_setup, channel_ready, discover_camera},
            cxp_phys::CXPSpeed,
            pl::csr};

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    Connected,
    Detected,
    Disconnected,
}

static mut ACTIVE_CHANNELS: u32 = 0;
static mut DISCOVERY_SPEED: CXPSpeed = CXPSpeed::CXP1;
// Mutex as they are needed by core1 cxp api calls
static STATE: Mutex<State> = Mutex::new(State::Disconnected);
static WITH_TAG: Mutex<bool> = Mutex::new(false);

pub fn camera_connected() -> bool {
    *STATE.lock() == State::Connected
}

pub fn with_tag() -> bool {
    *WITH_TAG.lock()
}

pub async fn async_camera_connected() -> bool {
    *STATE.async_lock().await == State::Connected
}

pub async fn async_with_tag() -> bool {
    *WITH_TAG.async_lock().await
}

pub async fn thread(i2c: &mut i2c::I2c) {
    loop {
        tick(i2c).await;
        timer::async_delay_ms(200).await;
    }
}

async fn tick(_i2c: &mut i2c::I2c) {
    // Get the value and drop the mutexguard to prevent blocking other async task that need to use it
    let current_state = { *STATE.async_lock().await };
    let next_state = match current_state {
        State::Disconnected => {
            unsafe {
                ACTIVE_CHANNELS = 0;
                csr::cxp_grabber::stream_decoder_active_channels_write(0);
                csr::cxp_grabber::stream_decoder_stream_id_write(0x00);
                csr::cxp_grabber::stream_decoder_stream_passthrough_write(0);
            };
            *WITH_TAG.async_lock().await = false;

            #[cfg(has_cxp_led)]
            update_led(_i2c, [LEDState::RedFlash1Hz; CXP_LEN]);
            match discover_camera().await {
                Ok(speed) => {
                    info!("camera detected, setting up camera...");
                    unsafe { DISCOVERY_SPEED = speed }
                    State::Detected
                }
                Err(_) => State::Disconnected,
            }
        }
        State::Detected => {
            #[cfg(has_cxp_led)]
            update_led(_i2c, [LEDState::OrangeFlash12Hz5; CXP_LEN]);
            match camera_setup(unsafe { DISCOVERY_SPEED }).await {
                Ok((active_channels, stream_id, with_tag)) => {
                    info!("camera setup complete");
                    unsafe {
                        csr::cxp_grabber::stream_decoder_active_channels_write(active_channels as u8);
                        ACTIVE_CHANNELS = active_channels;
                        match stream_id {
                            Some(id) => {
                                csr::cxp_grabber::stream_decoder_stream_id_write(id);
                                csr::cxp_grabber::stream_decoder_stream_passthrough_write(0);
                            }
                            None => {
                                // Disable stream id checking in gateware and pass every stream data to image decoder
                                csr::cxp_grabber::stream_decoder_stream_id_write(0x00);
                                csr::cxp_grabber::stream_decoder_stream_passthrough_write(1);
                            }
                        }
                    };
                    *WITH_TAG.async_lock().await = with_tag;
                    State::Connected
                }
                Err(e) => {
                    error!("camera setup failure: {}", e);
                    State::Disconnected
                }
            }
        }
        State::Connected => {
            #[cfg(has_cxp_led)]
            {
                let mut led_states = [LEDState::RedFlash1Hz; CXP_LEN];
                led_states[0..unsafe { ACTIVE_CHANNELS as usize }].fill(LEDState::GreenSolid);
                update_led(_i2c, led_states);
            }

            if (0..unsafe { ACTIVE_CHANNELS }).all(|ch| channel_ready(ch)) {
                unsafe {
                    let crc_errors = csr::cxp_grabber::stream_decoder_crc_errors_read();
                    if crc_errors != 0 {
                        error!("frame packet has CRC error {:#b}", crc_errors);
                        csr::cxp_grabber::stream_decoder_crc_errors_write(1);
                    };

                    if (csr::CXP[MASTER_CHANNEL as usize].rx_trigger_ack_read)() == 1 {
                        info!("received CXP linktrigger ack");
                        (csr::CXP[MASTER_CHANNEL as usize].rx_trigger_ack_write)(1);
                    };

                    if csr::cxp_grabber::stream_decoder_new_frame_read() == 1 {
                        let width = csr::cxp_grabber::stream_decoder_x_size_read();
                        let height = csr::cxp_grabber::stream_decoder_y_size_read();
                        match csr::cxp_grabber::stream_decoder_pixel_format_code_read() {
                            0x0101 => info!("received frame: {}x{} with MONO8 format", width, height),
                            0x0102 => info!("received frame: {}x{} with MONO10 format", width, height),
                            0x0103 => info!("received frame: {}x{} with MONO12 format", width, height),
                            0x0104 => info!("received frame: {}x{} with MONO14 format", width, height),
                            0x0105 => info!("received frame: {}x{} with MONO16 format", width, height),
                            _ => info!("received frame: {}x{} with Unsupported pixel format", width, height),
                        };
                        csr::cxp_grabber::stream_decoder_new_frame_write(1);
                    };
                }
                State::Connected
            } else {
                info!("camera disconnected");
                State::Disconnected
            }
        }
    };
    {
        *STATE.async_lock().await = next_state
    };
}

pub fn roi_viewer_setup(x0: u16, y0: u16, x1: u16, y1: u16) {
    unsafe {
        // flush the fifo before arming
        while csr::cxp_grabber::roi_viewer_fifo_stb_read() == 1 {
            csr::cxp_grabber::roi_viewer_fifo_ack_write(1);
        }
        csr::cxp_grabber::roi_viewer_x0_write(x0);
        csr::cxp_grabber::roi_viewer_x1_write(x1);
        csr::cxp_grabber::roi_viewer_y0_write(y0);
        csr::cxp_grabber::roi_viewer_y1_write(y1);
        csr::cxp_grabber::roi_viewer_arm_write(1);
    }
}
