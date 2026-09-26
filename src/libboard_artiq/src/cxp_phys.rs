use core::fmt;

use crate::pl::csr;

#[derive(Clone, Copy)]
pub enum CXPSpeed {
    CXP1,
    CXP2,
    CXP3,
    CXP5,
    CXP6,
    CXP10,
    CXP12,
}

impl fmt::Display for CXPSpeed {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &CXPSpeed::CXP1 => write!(f, "1.25 Gbps"),
            &CXPSpeed::CXP2 => write!(f, "2.5 Gbps"),
            &CXPSpeed::CXP3 => write!(f, "3.125 Gbps"),
            &CXPSpeed::CXP5 => write!(f, "5 Gbps"),
            &CXPSpeed::CXP6 => write!(f, "6.25 Gbps"),
            &CXPSpeed::CXP10 => write!(f, "10 Gbps"),
            &CXPSpeed::CXP12 => write!(f, "12.5 Gbps"),
        }
    }
}

pub fn setup() {
    let init_speed = CXPSpeed::CXP1;
    tx::setup();
    tx::change_linerate(init_speed);
    rx::setup();
    rx::change_linerate(init_speed);
}

pub mod tx {
    use super::*;

    pub fn setup() {
        unsafe {
            csr::cxp_grabber::phy_tx_enable_write(1);
        }
    }

    pub fn change_linerate(speed: CXPSpeed) {
        unsafe {
            match speed {
                CXPSpeed::CXP1 | CXPSpeed::CXP2 | CXPSpeed::CXP3 | CXPSpeed::CXP5 | CXPSpeed::CXP6 => {
                    csr::cxp_grabber::phy_tx_bitrate2x_enable_write(0);
                }
                CXPSpeed::CXP10 | CXPSpeed::CXP12 => {
                    csr::cxp_grabber::phy_tx_bitrate2x_enable_write(1);
                }
            };
            csr::cxp_grabber::phy_tx_clk_reset_write(1);
        }
    }
}

pub mod rx {
    use super::*;

    pub fn setup() {
        unsafe {
            csr::cxp_grabber::phy_rx_gtx_refclk_stable_write(1);
        }
    }

    pub fn change_linerate(speed: CXPSpeed) {
        change_qpll_fb_divider(speed);
        change_gtx_divider(speed);
        change_cdr_cfg(speed);
        change_eq_cfg(speed);

        unsafe {
            csr::cxp_grabber::phy_rx_qpll_reset_write(1);
            while csr::cxp_grabber::phy_rx_qpll_locked_read() != 1 {}
            // Changing RXOUT_DIV via DRP requires a manual reset
            // https://adaptivesupport.amd.com/s/question/0D52E00006hplwnSAA/re-gtx-line-rate-change
            csr::cxp_grabber::phy_rx_gtx_restart_write(1);
        }
    }

    fn change_qpll_fb_divider(speed: CXPSpeed) {
        let qpll_div_reg = match speed {
            CXPSpeed::CXP1 | CXPSpeed::CXP2 | CXPSpeed::CXP5 | CXPSpeed::CXP10 => 0x0120, // FB_Divider = 80, QPLL VCO @ 10GHz
            CXPSpeed::CXP3 | CXPSpeed::CXP6 | CXPSpeed::CXP12 => 0x0170, // FB_Divider = 100, QPLL VCO @ 12.5GHz
        };

        qpll_write(0x36, qpll_div_reg);
    }

    fn change_gtx_divider(speed: CXPSpeed) {
        let div_reg = match speed {
            CXPSpeed::CXP1 => 0x03,                    // TXOUT_DIV = 1, RXOUT_DIV = 8
            CXPSpeed::CXP2 | CXPSpeed::CXP3 => 0x02,   // TXOUT_DIV = 1, RXOUT_DIV = 4
            CXPSpeed::CXP5 | CXPSpeed::CXP6 => 0x01,   // TXOUT_DIV = 1, RXOUT_DIV = 2
            CXPSpeed::CXP10 | CXPSpeed::CXP12 => 0x00, // TXOUT_DIV = 1, RXOUT_DIV = 1
        };

        gtx_write(0x88, div_reg);
    }

    fn change_cdr_cfg(speed: CXPSpeed) {
        struct CdrConfig {
            pub cfg_reg0: u16, // addr = 0xA8
            pub cfg_reg1: u16, // addr = 0xA9
            pub cfg_reg2: u16, // addr = 0xAA
            pub cfg_reg3: u16, // addr = 0xAB
            pub cfg_reg4: u16, // addr = 0xAC
        }

        let cdr_cfg = match speed {
            // when RXOUT_DIV = 8
            CXPSpeed::CXP1 => CdrConfig {
                cfg_reg0: 0x0020,
                cfg_reg1: 0x1008,
                cfg_reg2: 0x23FF,
                cfg_reg3: 0x0000,
                cfg_reg4: 0x0003,
            },
            // when RXOUT_DIV = 4
            CXPSpeed::CXP2 | CXPSpeed::CXP5 => CdrConfig {
                cfg_reg0: 0x0020,
                cfg_reg1: 0x1010,
                cfg_reg2: 0x23FF,
                cfg_reg3: 0x0000,
                cfg_reg4: 0x0003,
            },
            // when RXOUT_DIV= 2
            CXPSpeed::CXP3 | CXPSpeed::CXP6 => CdrConfig {
                cfg_reg0: 0x0020,
                cfg_reg1: 0x1020,
                cfg_reg2: 0x23FF,
                cfg_reg3: 0x0000,
                cfg_reg4: 0x0003,
            },
            // when RXOUT_DIV= 1
            CXPSpeed::CXP10 | CXPSpeed::CXP12 => CdrConfig {
                cfg_reg0: 0x0020,
                cfg_reg1: 0x1040,
                cfg_reg2: 0x23FF,
                cfg_reg3: 0x0000,
                cfg_reg4: 0x000B,
            },
        };

        gtx_write(0x0A8, cdr_cfg.cfg_reg0);
        gtx_write(0x0A9, cdr_cfg.cfg_reg1);
        gtx_write(0x0AA, cdr_cfg.cfg_reg2);
        gtx_write(0x0AB, cdr_cfg.cfg_reg3);
        gtx_write(0x0AC, cdr_cfg.cfg_reg4);
    }

    fn change_eq_cfg(speed: CXPSpeed) {
        let eq_cfg = match speed {
            CXPSpeed::CXP1 | CXPSpeed::CXP2 | CXPSpeed::CXP3 | CXPSpeed::CXP5 | CXPSpeed::CXP6 => 0x0904,
            CXPSpeed::CXP10 | CXPSpeed::CXP12 => 0x0104,
        };

        gtx_write(0x029, eq_cfg);
    }

    #[allow(dead_code)]
    fn gtx_read(address: u16) -> u16 {
        unsafe {
            csr::cxp_grabber::phy_rx_gtx_daddr_write(address);
            csr::cxp_grabber::phy_rx_gtx_dread_write(1);
            while csr::cxp_grabber::phy_rx_gtx_dready_read() != 1 {}
            csr::cxp_grabber::phy_rx_gtx_dout_read()
        }
    }

    fn gtx_write(address: u16, value: u16) {
        unsafe {
            csr::cxp_grabber::phy_rx_gtx_daddr_write(address);
            csr::cxp_grabber::phy_rx_gtx_din_write(value);
            csr::cxp_grabber::phy_rx_gtx_din_stb_write(1);
            while csr::cxp_grabber::phy_rx_gtx_dready_read() != 1 {}
        }
    }

    #[allow(dead_code)]
    fn qpll_read(address: u8) -> u16 {
        unsafe {
            csr::cxp_grabber::phy_rx_qpll_daddr_write(address);
            csr::cxp_grabber::phy_rx_qpll_dread_write(1);
            while csr::cxp_grabber::phy_rx_qpll_dready_read() != 1 {}
            csr::cxp_grabber::phy_rx_qpll_dout_read()
        }
    }

    fn qpll_write(address: u8, value: u16) {
        unsafe {
            csr::cxp_grabber::phy_rx_qpll_daddr_write(address);
            csr::cxp_grabber::phy_rx_qpll_din_write(value);
            csr::cxp_grabber::phy_rx_qpll_din_stb_write(1);
            while csr::cxp_grabber::phy_rx_qpll_dready_read() != 1 {}
        }
    }
}
