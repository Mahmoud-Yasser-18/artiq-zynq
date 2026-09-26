#!/usr/bin/env python

import argparse
from operator import itemgetter

from migen import *
from migen.build.generic_platform import *
from migen.genlib.resetsync import AsyncResetSynchronizer
from migen.genlib.cdc import MultiReg
from migen_axi.integration.soc_core import SoCCore
from migen_axi.platforms import kasli_soc
from misoc.interconnect.csr import *
from misoc.cores import virtual_leds

from artiq.coredevice import jsondesc
from artiq.gateware import rtio, eem_7series
from artiq.gateware.cxp_grabber.core import CXPHostCore
from artiq.gateware.rtio.xilinx_clocking import fix_serdes_timing_path
from artiq.gateware.rtio.phy import ttl_simple, cxp_grabber
from artiq.gateware.drtio.transceiver import gtx_7series, eem_serdes
from artiq.gateware.drtio.siphaser import SiPhaser7Series
from artiq.gateware.drtio.rx_synchronizer import XilinxRXSynchronizer
from artiq.gateware.drtio import *
from artiq.gateware.wrpll import wrpll

import dma
import analyzer
import acpki as acpki_lib
import drtio_aux_controller
import zynq_clocking
from config import generate_ident, write_csr_file, write_mem_file, write_rustc_cfg_file

eem_iostandard_dict = {
     0: "LVDS_25",
     1: "LVDS_25",
     2: "LVDS",
     3: "LVDS",
     4: "LVDS",
     5: "LVDS",
     6: "LVDS",
     7: "LVDS",
     8: "LVDS_25",
     9: "LVDS_25",
    10: "LVDS",
    11: "LVDS",
}


def eem_iostandard(eem):
    return IOStandard(eem_iostandard_dict[eem])


class SMAClkinForward(Module):
    def __init__(self, platform):
        sma_clkin = platform.request("sma_clkin")
        sma_clkin_se = Signal()
        cdr_clk_se = Signal()
        cdr_clk = platform.request("cdr_clk")
        self.specials += [
            Instance("IBUFDS", i_I=sma_clkin.p, i_IB=sma_clkin.n, o_O=sma_clkin_se),
            Instance("ODDR", i_C=sma_clkin_se, i_CE=1, i_D1=1, i_D2=0, o_Q=cdr_clk_se),
            Instance("OBUFDS", i_I=cdr_clk_se, o_O=cdr_clk.p, o_OB=cdr_clk.n)
        ]


class GTPBootstrapClock(Module):
    def __init__(self, platform, freq=125e6):
        self.clock_domains.cd_bootstrap = ClockDomain(reset_less=True)
        self.cd_bootstrap.clk.attr.add("keep")

        bootstrap_125 = platform.request("clk125_gtp")
        bootstrap_se = Signal()
        clk_out = Signal()
        platform.add_period_constraint(bootstrap_125.p, 8.0)
        self.specials += [
            Instance("IBUFDS_GTE2",
                i_CEB=0,
                i_I=bootstrap_125.p, i_IB=bootstrap_125.n, 
                o_O=bootstrap_se,
                p_CLKCM_CFG="TRUE",
                p_CLKRCV_TRST="TRUE",
                p_CLKSWING_CFG=3),
            Instance("BUFG", i_I=bootstrap_se, o_O=clk_out)
        ]
        if freq == 125e6:
            self.comb += self.cd_bootstrap.clk.eq(clk_out)
        elif freq == 100e6:
            pll_fb = Signal()
            pll_out = Signal()
            self.specials += [
                Instance("PLLE2_BASE",
                    p_CLKIN1_PERIOD=8.0,
                    i_CLKIN1=clk_out,
                    i_CLKFBIN=pll_fb,
                    o_CLKFBOUT=pll_fb,

                    # VCO @ 1GHz
                    p_CLKFBOUT_MULT=8, p_DIVCLK_DIVIDE=1,

                    # 100MHz for bootstrap
                    p_CLKOUT1_DIVIDE=10, p_CLKOUT1_PHASE=0.0, o_CLKOUT1=pll_out,
                ),
                Instance("BUFG", i_I=pll_out, o_O=self.cd_bootstrap.clk)
            ]
        else:
            raise ValueError("Bootstrap frequency must be 100 or 125MHz")


def add_coaxpress_sfp(cls, sfp_slots, roi_engine_count, max_cxp_speed, refclk=None):
    # Ported from ARTIQ 10 artiq-zynq kasli_soc.py (CoaXPress-SFP grabber support).
    # `match` replaced with if/elif for Python-version safety.
    if refclk is None:
        refclk = Signal()
        gt_refclk_pad = cls.platform.request("clk_gtp")
        cls.platform.add_period_constraint(gt_refclk_pad.p, 8.0)
        cls.specials += Instance("IBUFDS_GTE2",
            i_CEB=0,
            i_I=gt_refclk_pad.p,
            i_IB=gt_refclk_pad.n,
            o_O=refclk,
            p_CLKCM_CFG="TRUE",
            p_CLKRCV_TRST="TRUE",
            p_CLKSWING_CFG=3
        )

    cls.config["HAS_CXP_LED"] = None

    stream_fifo_size = 0x10000
    # StreamPacketSizeMax is defined in bytes - Section 12.3.37 (CXP-001-2021)
    cls.config["MAX_CXP_STREAM_PAK_SIZE"] = stream_fifo_size // 8
    cls.submodules.cxp_grabber = cxp_grabber.CXPGrabber(
        refclk=refclk,
        gt_pads=[cls.platform.request("sfp", i) for i in sfp_slots],
        sys_clk_freq=cls.clk_freq,
        roi_engine_count=roi_engine_count,
        stream_fifo_size=stream_fifo_size
    )
    cls.csr_devices.append("cxp_grabber")

    cls.cxp_csr_group = []
    cls.cxp_mem_group = []
    for i, trx_phy in enumerate(cls.cxp_grabber.phy.phys):
        core_name = "cxp" + str(i)
        mem_name = "cxp" + str(i) + "_mem"
        core = CXPHostCore(trx_phy, trx_phy, cls.clk_freq)
        setattr(cls.submodules, core_name, core)

        cls.comb += core.rx.source.connect(cls.cxp_grabber.stream_decoder.sinks[i]),

        # Only master channel need to connect trigger
        if i == 0:
            cls.sync.rio += [
                core.tx.trig_extra_linktrig.eq(cls.cxp_grabber.trig_extra_linktrig_en),
                core.tx.trig_linktrig_mode.eq(cls.cxp_grabber.trig_linktrig_mode),
                core.tx.trig_stb.eq(cls.cxp_grabber.trig_stb),
            ]

        cls.csr_devices.append(core_name)
        cls.cxp_csr_group.append(core_name)
        cls.cxp_mem_group.append(mem_name)

        mem_size = core.get_mem_size()
        # upper half is tx while lower half is rx
        memory_address = cls.axi2csr.register_port(core.get_tx_port(), mem_size)
        cls.axi2csr.register_port(core.get_rx_port(), mem_size)
        cls.add_memory_region(
            mem_name, cls.mem_map["csr"] + memory_address, mem_size * 2
        )

    cls.add_csr_group("cxp", cls.cxp_csr_group)
    cls.add_memory_group("cxp_mem", cls.cxp_mem_group)

    print(
        "CoaXPress-SFP (SFP{}) at RTIO channel 0x{:06x}".format(
            str(sfp_slots)[1:-1], len(cls.rtio_channels)
        )
    )
    cls.rtio_channels += [
        rtio.Channel(cls.cxp_grabber.trigger),
        rtio.Channel(cls.cxp_grabber.config),
        rtio.Channel(cls.cxp_grabber.gate_data),
    ]

    cls.config["MAX_CXP_SPEED"] = max_cxp_speed
    if max_cxp_speed == "CXP-6":
        # max freq of cxp_gt_rx = linerate/internal_datawidth = 6.25Gbps/40 = 156.25 MHz
        rx_period = 6.4
    elif max_cxp_speed == "CXP-10":
        # max freq of cxp_gt_rx = 10Gbps/40 = 250 MHz
        rx_period = 4
    elif max_cxp_speed == "CXP-12":
        # max freq of cxp_gt_rx = 12.5Gbps/40 = 312.5 MHz
        rx_period = 3.2
    else:
        raise ValueError("Invalid CXP speed")

    rx = cls.cxp_grabber.phy.phys[0]
    cls.platform.add_period_constraint(rx.gtx.cd_cxp_gt_rx.clk, rx_period)
    # constraint the clk path
    cls.platform.add_false_path_constraints(cls.sys_crg.cd_sys.clk, rx.gtx.cd_cxp_gt_rx.clk)


class GenericStandalone(SoCCore):
    def __init__(self, description, acpki=False):
        self.acpki = acpki
        clk_freq = description["rtio_frequency"]
        self.clk_freq = clk_freq  # needed by add_coaxpress_sfp (CXP grabber)
        with_wrpll = description["enable_wrpll"]

        platform = kasli_soc.Platform()
        platform.toolchain.bitstream_commands.extend([
            "set_property BITSTREAM.GENERAL.COMPRESS True [current_design]",
        ])
        ident = generate_ident(description["variant"])
        if self.acpki:
            ident = "acpki_" + ident
        SoCCore.__init__(self, platform=platform, csr_data_width=32, ident=ident, ps_cd_sys=False)

        self.config["HW_REV"] = description["hw_rev"]
        clk_synth = platform.request("cdr_clk_clean_fabric")
        clk_synth_se = Signal()
        clk_synth_se_buf = Signal()
        platform.add_period_constraint(clk_synth.p, 8.0)

        self.specials += [ 
            Instance("IBUFGDS",
                p_DIFF_TERM="TRUE", p_IBUF_LOW_PWR="FALSE",
                i_I=clk_synth.p, i_IB=clk_synth.n, o_O=clk_synth_se
            ),
            Instance("BUFG", i_I=clk_synth_se, o_O=clk_synth_se_buf),
        ]
        fix_serdes_timing_path(platform)
        self.submodules.bootstrap = GTPBootstrapClock(self.platform, clk_freq)
        self.config["RTIO_FREQUENCY"] = str(clk_freq/1e6)
        self.config["CLOCK_FREQUENCY"] = int(clk_freq)

        self.submodules.sys_crg = zynq_clocking.SYSCRG(self.platform, self.ps7, clk_synth_se_buf)
        platform.add_false_path_constraints(
            self.bootstrap.cd_bootstrap.clk, self.sys_crg.cd_sys.clk)
        self.csr_devices.append("sys_crg")
        self.crg = self.ps7 # HACK for eem_7series to find the clock
        self.crg.cd_sys = self.sys_crg.cd_sys

        if with_wrpll:
            self.submodules.wrpll_refclk = wrpll.FrequencyMultiplier(platform.request("sma_clkin"))
            self.submodules.wrpll = wrpll.WRPLL(
                platform=self.platform,
                cd_ref=self.wrpll_refclk.cd_ref,
                main_clk_se=clk_synth_se)
            self.csr_devices.append("wrpll_refclk")
            self.csr_devices.append("wrpll")
            self.comb += self.ps7.core.core0.nfiq.eq(self.wrpll.ev.irq)
            self.config["HAS_SI549"] = None
            self.config["WRPLL_REF_CLK"] = "SMA_CLKIN"
        else:
            self.submodules += SMAClkinForward(self.platform)
            self.config["HAS_SI5324"] = None
            self.config["SI5324_SOFT_RESET"] = None


        self.rtio_channels = []
        has_grabber = False
        eem_peripherals = []
        for peripheral in description["peripherals"]:
            if peripheral["type"] == "coaxpress_sfp":
                add_coaxpress_sfp(self, list(range(peripheral["channels"])),
                                  peripheral["roi_engine_count"], peripheral["max_cxp_speed"])
            elif peripheral["type"] == "grabber":
                has_grabber = True
                self.grabber_csr_group = []
                eem_peripherals.append(peripheral)
            else:
                eem_peripherals.append(peripheral)
        eem_7series.add_peripherals(self, eem_peripherals, iostandard=eem_iostandard)
        for i in (0, 1):
            print("USER LED at RTIO channel 0x{:06x}".format(len(self.rtio_channels)))
            user_led = self.platform.request("user_led", i)
            phy = ttl_simple.Output(user_led)
            self.submodules += phy
            self.rtio_channels.append(rtio.Channel.from_phy(phy))
        self.config["RTIO_LOG_CHANNEL"] = len(self.rtio_channels)
        self.rtio_channels.append(rtio.LogChannel())

        self.submodules.rtio_tsc = rtio.TSC(glbl_fine_ts_width=3)
        self.submodules.rtio_core = rtio.Core(
            self.rtio_tsc, self.rtio_channels, lane_count=description["sed_lanes"]
        )
        self.csr_devices.append("rtio_core")

        if self.acpki:
            self.config["KI_IMPL"] = "acp"
            self.submodules.rtio = acpki_lib.KernelInitiator(self.rtio_tsc,
                                                             bus=self.ps7.s_axi_acp,
                                                             user=self.ps7.s_axi_acp_user,
                                                             evento=self.ps7.event.o)
            self.csr_devices.append("rtio")
        else:
            self.config["KI_IMPL"] = "csr"
            self.submodules.rtio = rtio.KernelInitiator(self.rtio_tsc, now64=True)
            self.csr_devices.append("rtio")

        self.submodules.rtio_dma = dma.DMA(self.ps7.s_axi_hp0)
        self.csr_devices.append("rtio_dma")

        self.submodules.cri_con = rtio.CRIInterconnectShared(
            [self.rtio.cri, self.rtio_dma.cri],
            [self.rtio_core.cri])
        self.csr_devices.append("cri_con")

        self.submodules.rtio_moninj = rtio.MonInj(self.rtio_channels)
        self.csr_devices.append("rtio_moninj")

        self.submodules.rtio_analyzer = analyzer.Analyzer(self.rtio_tsc, self.rtio_core.cri,
                                                          self.ps7.s_axi_hp1)
        self.csr_devices.append("rtio_analyzer")

        if has_grabber:
            self.config["HAS_GRABBER"] = None
            self.add_csr_group("grabber", self.grabber_csr_group)
            for grabber in self.grabber_csr_group:
                self.platform.add_false_path_constraints(
                    self.sys_crg.cd_sys.clk, getattr(self, grabber).deserializer.cd_cl.clk)


class GenericMaster(SoCCore):
    def __init__(self, description, acpki=False):
        clk_freq = description["rtio_frequency"]
        with_wrpll = description["enable_wrpll"]

        has_drtio_over_eem = any(peripheral["type"] == "shuttler" for peripheral in description["peripherals"])
        self.acpki = acpki

        platform = kasli_soc.Platform()
        platform.toolchain.bitstream_commands.extend([
            "set_property BITSTREAM.GENERAL.COMPRESS True [current_design]",
        ])
        ident = generate_ident(description["variant"])
        if self.acpki:
            ident = "acpki_" + ident
        SoCCore.__init__(self, platform=platform, csr_data_width=32, ident=ident, ps_cd_sys=False)

        self.config["HW_REV"] = description["hw_rev"]

        data_pads = [platform.request("sfp", i) for i in range(4)]

        self.submodules.gt_drtio = gtx_7series.GTX(
            clock_pads=platform.request("clk_gtp"),
            pads=data_pads,
            clk_freq=clk_freq)
        self.csr_devices.append("gt_drtio")
        self.config["RTIO_FREQUENCY"] = str(clk_freq/1e6)
        self.config["CLOCK_FREQUENCY"] = int(clk_freq)

        txout_buf = Signal()
        gtx0 = self.gt_drtio.gtxs[0]
        self.specials += Instance("BUFG", i_I=gtx0.txoutclk, o_O=txout_buf)

        ext_async_rst = Signal()

        self.submodules.bootstrap = GTPBootstrapClock(self.platform, clk_freq)
        self.submodules.sys_crg = zynq_clocking.SYSCRG(
            self.platform,
            self.ps7,
            txout_buf,
            clk_sw=self.gt_drtio.stable_clkin.storage,
            clk_sw_status=gtx0.tx_init.done,
            ext_async_rst=ext_async_rst)
        self.csr_devices.append("sys_crg")
        self.crg = self.ps7 # HACK for eem_7series to find the clock
        self.crg.cd_sys = self.sys_crg.cd_sys
        platform.add_false_path_constraints(
            self.bootstrap.cd_bootstrap.clk, self.sys_crg.cd_sys.clk)
        fix_serdes_timing_path(platform)

        self.comb += ext_async_rst.eq(self.sys_crg.clk_sw_fsm.o_clk_sw & ~gtx0.tx_init.done)
        self.specials += MultiReg(self.sys_crg.clk_sw_fsm.o_clk_sw & self.sys_crg.mmcm_locked, self.gt_drtio.clk_path_ready, odomain="bootstrap")

        if with_wrpll:
            clk_synth = platform.request("cdr_clk_clean_fabric")
            clk_synth_se = Signal()
            platform.add_period_constraint(clk_synth.p, 8.0)
            self.specials += Instance("IBUFGDS", p_DIFF_TERM="TRUE", p_IBUF_LOW_PWR="FALSE", i_I=clk_synth.p, i_IB=clk_synth.n, o_O=clk_synth_se)
            self.submodules.wrpll_refclk = wrpll.FrequencyMultiplier(platform.request("sma_clkin"))
            self.submodules.wrpll = wrpll.WRPLL(
                platform=self.platform,
                cd_ref=self.wrpll_refclk.cd_ref,
                main_clk_se=clk_synth_se)
            self.csr_devices.append("wrpll_refclk")
            self.csr_devices.append("wrpll")
            self.comb += self.ps7.core.core0.nfiq.eq(self.wrpll.ev.irq)
            self.config["HAS_SI549"] = None
            self.config["WRPLL_REF_CLK"] = "SMA_CLKIN"
        else:
            self.submodules += SMAClkinForward(self.platform)
            self.config["HAS_SI5324"] = None
            self.config["SI5324_SOFT_RESET"] = None

        self.rtio_channels = []
        has_grabber = any(peripheral["type"] == "grabber" for peripheral in description["peripherals"])
        if has_drtio_over_eem:
            self.eem_drtio_channels = []
        if has_grabber:
            self.grabber_csr_group = []
        eem_7series.add_peripherals(self, description["peripherals"], iostandard=eem_iostandard)
        for i in (0, 1):
            print("USER LED at RTIO channel 0x{:06x}".format(len(self.rtio_channels)))
            user_led = self.platform.request("user_led", i)
            phy = ttl_simple.Output(user_led)
            self.submodules += phy
            self.rtio_channels.append(rtio.Channel.from_phy(phy))
        self.config["RTIO_LOG_CHANNEL"] = len(self.rtio_channels)
        self.rtio_channels.append(rtio.LogChannel())

        self.submodules.rtio_tsc = rtio.TSC(glbl_fine_ts_width=3)

        self.drtio_csr_group = []
        self.drtioaux_csr_group = []
        self.drtioaux_memory_group = []
        self.drtio_cri = []
        for i in range(len(self.gt_drtio.channels)):
            core_name = "drtio" + str(i)
            coreaux_name = "drtioaux" + str(i)
            memory_name = "drtioaux" + str(i) + "_mem"
            self.drtio_csr_group.append(core_name)
            self.drtioaux_csr_group.append(coreaux_name)
            self.drtioaux_memory_group.append(memory_name)

            cdr = ClockDomainsRenamer({"rtio_rx": "rtio_rx" + str(i)})

            core = cdr(DRTIOMaster(self.rtio_tsc, self.gt_drtio.channels[i]))
            setattr(self.submodules, core_name, core)
            self.drtio_cri.append(core.cri)
            self.csr_devices.append(core_name)

            coreaux = cdr(drtio_aux_controller.DRTIOAuxControllerBare(core.link_layer))
            setattr(self.submodules, coreaux_name, coreaux)
            self.csr_devices.append(coreaux_name)

            size = coreaux.get_mem_size()
            memory_address = self.axi2csr.register_port(coreaux.get_tx_port(), size)
            self.axi2csr.register_port(coreaux.get_rx_port(), size)
            self.add_memory_region(memory_name, self.mem_map["csr"] + memory_address, size * 2)
        self.config["HAS_DRTIO"] = None
        self.config["HAS_DRTIO_ROUTING"] = None

        if has_drtio_over_eem:
            self.add_eem_drtio(self.eem_drtio_channels)
        self.add_drtio_cpuif_groups()

        self.submodules.rtio_core = rtio.Core(
            self.rtio_tsc, self.rtio_channels, lane_count=description["sed_lanes"]
        )
        self.csr_devices.append("rtio_core")

        if self.acpki:
            self.config["KI_IMPL"] = "acp"
            self.submodules.rtio = acpki_lib.KernelInitiator(self.rtio_tsc,
                                                             bus=self.ps7.s_axi_acp,
                                                             user=self.ps7.s_axi_acp_user,
                                                             evento=self.ps7.event.o)
            self.csr_devices.append("rtio")
        else:
            self.config["KI_IMPL"] = "csr"
            self.submodules.rtio = rtio.KernelInitiator(self.rtio_tsc, now64=True)
            self.csr_devices.append("rtio")

        self.submodules.rtio_dma = dma.DMA(self.ps7.s_axi_hp0)
        self.csr_devices.append("rtio_dma")

        self.submodules.cri_con = rtio.CRIInterconnectShared(
            [self.rtio.cri, self.rtio_dma.cri],
            [self.rtio_core.cri] + self.drtio_cri,
            enable_routing=True)
        self.csr_devices.append("cri_con")

        self.submodules.rtio_moninj = rtio.MonInj(self.rtio_channels)
        self.csr_devices.append("rtio_moninj")

        self.submodules.routing_table = rtio.RoutingTableAccess(self.cri_con)
        self.csr_devices.append("routing_table")

        self.submodules.rtio_analyzer = analyzer.Analyzer(self.rtio_tsc, self.rtio_core.cri,
                                                          self.ps7.s_axi_hp1)
        self.csr_devices.append("rtio_analyzer")

        if has_grabber:
            self.config["HAS_GRABBER"] = None
            self.add_csr_group("grabber", self.grabber_csr_group)
        

        self.submodules.virtual_leds = virtual_leds.VirtualLeds()
        self.csr_devices.append("virtual_leds")

        self.comb += [self.virtual_leds.get(i).eq(channel.rx_ready)
                for i, channel in enumerate(self.gt_drtio.channels)]

    def add_eem_drtio(self, eem_drtio_channels):
        # Must be called before invoking add_rtio() to construct the CRI
        # interconnect properly
        self.submodules.eem_transceiver = eem_serdes.EEMSerdes(self.platform, eem_drtio_channels)
        self.csr_devices.append("eem_transceiver")
        self.config["HAS_DRTIO_EEM"] = None
        self.config["EEM_DRTIO_COUNT"] = len(eem_drtio_channels)

        cdr = ClockDomainsRenamer({"rtio_rx": "sys"})
        for i in range(len(self.eem_transceiver.channels)):
            channel = i + len(self.gt_drtio.channels)
            core_name = "drtio" + str(channel)
            coreaux_name = "drtioaux" + str(channel)
            memory_name = "drtioaux" + str(channel) + "_mem"
            self.drtio_csr_group.append(core_name)
            self.drtioaux_csr_group.append(coreaux_name)
            self.drtioaux_memory_group.append(memory_name)

            core = cdr(DRTIOMaster(self.rtio_tsc, self.eem_transceiver.channels[i]))
            setattr(self.submodules, core_name, core)
            self.drtio_cri.append(core.cri)
            self.csr_devices.append(core_name)

            coreaux = cdr(drtio_aux_controller.DRTIOAuxControllerBare(core.link_layer))
            setattr(self.submodules, coreaux_name, coreaux)
            self.csr_devices.append(coreaux_name)

            size = coreaux.get_mem_size()
            memory_address = self.axi2csr.register_port(coreaux.get_tx_port(), size)
            self.axi2csr.register_port(coreaux.get_rx_port(), size)
            self.add_memory_region(memory_name, self.mem_map["csr"] + memory_address, size * 2)

    def add_drtio_cpuif_groups(self):
        self.add_csr_group("drtio", self.drtio_csr_group)
        self.add_csr_group("drtioaux", self.drtioaux_csr_group)
        self.add_memory_group("drtioaux_mem", self.drtioaux_memory_group)


class GenericSatellite(SoCCore):
    def __init__(self, description, acpki=False):
        clk_freq = description["rtio_frequency"]
        with_wrpll = description["enable_wrpll"]

        self.acpki = acpki

        platform = kasli_soc.Platform()
        platform.toolchain.bitstream_commands.extend([
            "set_property BITSTREAM.GENERAL.COMPRESS True [current_design]",
        ])
        ident = generate_ident(description["variant"])
        if self.acpki:
            ident = "acpki_" + ident
        SoCCore.__init__(self, platform=platform, csr_data_width=32, ident=ident, ps_cd_sys=False)

        self.config["HW_REV"] = description["hw_rev"]

        data_pads = [platform.request("sfp", i) for i in range(4)]
        
        self.submodules.gt_drtio = gtx_7series.GTX(
            clock_pads=platform.request("clk_gtp"),  
            pads=data_pads,
            clk_freq=clk_freq)
        self.csr_devices.append("gt_drtio")

        txout_buf = Signal()
        gtx0 = self.gt_drtio.gtxs[0]
        self.specials += Instance("BUFG", i_I=gtx0.txoutclk, o_O=txout_buf)

        ext_async_rst = Signal()

        self.submodules.bootstrap = GTPBootstrapClock(self.platform, clk_freq)
        self.submodules.sys_crg = zynq_clocking.SYSCRG(
            self.platform, 
            self.ps7,
            txout_buf,
            clk_sw=self.gt_drtio.stable_clkin.storage,
            clk_sw_status=gtx0.tx_init.done,
            ext_async_rst=ext_async_rst)
        platform.add_false_path_constraints(
            self.bootstrap.cd_bootstrap.clk, self.sys_crg.cd_sys.clk)
        self.csr_devices.append("sys_crg")
        self.crg = self.ps7 # HACK for eem_7series to find the clock
        self.crg.cd_sys = self.sys_crg.cd_sys

        fix_serdes_timing_path(platform)

        self.comb += ext_async_rst.eq(self.sys_crg.clk_sw_fsm.o_clk_sw & ~gtx0.tx_init.done)
        self.specials += MultiReg(self.sys_crg.clk_sw_fsm.o_clk_sw & self.sys_crg.mmcm_locked, self.gt_drtio.clk_path_ready, odomain="bootstrap")

        self.rtio_channels = []
        has_grabber = any(peripheral["type"] == "grabber" for peripheral in description["peripherals"])
        if has_grabber:
            self.grabber_csr_group = []
        eem_7series.add_peripherals(self, description["peripherals"], iostandard=eem_iostandard)
        for i in (0, 1):
            print("USER LED at RTIO channel 0x{:06x}".format(len(self.rtio_channels)))
            user_led = self.platform.request("user_led", i)
            phy = ttl_simple.Output(user_led)
            self.submodules += phy
            self.rtio_channels.append(rtio.Channel.from_phy(phy))
        self.config["RTIO_LOG_CHANNEL"] = len(self.rtio_channels)
        self.rtio_channels.append(rtio.LogChannel())

        self.submodules.rtio_tsc = rtio.TSC(glbl_fine_ts_width=3)

        drtioaux_csr_group = []
        drtioaux_memory_group = []
        drtiorep_csr_group = []
        self.drtio_cri = []
        for i in range(len(self.gt_drtio.channels)):
            coreaux_name = "drtioaux" + str(i)
            memory_name = "drtioaux" + str(i) + "_mem"
            drtioaux_csr_group.append(coreaux_name)
            drtioaux_memory_group.append(memory_name)

            cdr = ClockDomainsRenamer({"rtio_rx": "rtio_rx" + str(i)})

            if i == 0:
                self.submodules.rx_synchronizer = cdr(XilinxRXSynchronizer())
                core = cdr(DRTIOSatellite(
                    self.rtio_tsc, self.gt_drtio.channels[i],
                    self.rx_synchronizer))
                self.submodules.drtiosat = core
                self.csr_devices.append("drtiosat")
            else:
                corerep_name = "drtiorep" + str(i-1)
                drtiorep_csr_group.append(corerep_name)

                core = cdr(DRTIORepeater(
                    self.rtio_tsc, self.gt_drtio.channels[i]))
                setattr(self.submodules, corerep_name, core)
                self.drtio_cri.append(core.cri)
                self.csr_devices.append(corerep_name)

            coreaux = cdr(drtio_aux_controller.DRTIOAuxControllerBare(core.link_layer))
            setattr(self.submodules, coreaux_name, coreaux)
            self.csr_devices.append(coreaux_name)

            mem_size = coreaux.get_mem_size()
            tx_port = coreaux.get_tx_port()
            rx_port = coreaux.get_rx_port()
            memory_address = self.axi2csr.register_port(tx_port, mem_size)
            # rcv in upper half of the memory, thus added second
            self.axi2csr.register_port(rx_port, mem_size)
            # and registered in PS interface
            # manually, because software refers to rx/tx by halves of entire memory block, not names
            self.add_memory_region(memory_name, self.mem_map["csr"] + memory_address, mem_size * 2)
        self.config["HAS_DRTIO"] = None
        self.config["HAS_DRTIO_ROUTING"] = None
        self.add_csr_group("drtioaux", drtioaux_csr_group)
        self.add_memory_group("drtioaux_mem", drtioaux_memory_group)
        self.add_csr_group("drtiorep", drtiorep_csr_group)

        if self.acpki:
            self.config["KI_IMPL"] = "acp"
            self.submodules.rtio = acpki_lib.KernelInitiator(self.rtio_tsc,
                                                             bus=self.ps7.s_axi_acp,
                                                             user=self.ps7.s_axi_acp_user,
                                                             evento=self.ps7.event.o)
            self.csr_devices.append("rtio")
        else:
            self.config["KI_IMPL"] = "csr"
            self.submodules.rtio = rtio.KernelInitiator(self.rtio_tsc, now64=True)
            self.csr_devices.append("rtio")

        self.submodules.rtio_dma = dma.DMA(self.ps7.s_axi_hp0)
        self.csr_devices.append("rtio_dma")

        self.submodules.local_io = SyncRTIO(
            self.rtio_tsc, self.rtio_channels, lane_count=description["sed_lanes"]
        )
        self.comb += [
            self.drtiosat.async_errors.eq(self.local_io.async_errors),
            self.local_io.sed_spread_enable.eq(self.drtiosat.sed_spread_enable.storage)
        ]

        self.submodules.cri_con = rtio.CRIInterconnectShared(
            [self.drtiosat.cri, self.rtio_dma.cri, self.rtio.cri],
            [self.local_io.cri] + self.drtio_cri,
            enable_routing=True)
        self.csr_devices.append("cri_con")

        self.submodules.routing_table = rtio.RoutingTableAccess(self.cri_con)
        self.csr_devices.append("routing_table")     

        self.submodules.rtio_moninj = rtio.MonInj(self.rtio_channels)
        self.csr_devices.append("rtio_moninj")

        self.submodules.rtio_analyzer = analyzer.Analyzer(self.rtio_tsc, self.local_io.cri,
                                                          self.ps7.s_axi_hp1)
        self.csr_devices.append("rtio_analyzer")

        rtio_clk_period = 1e9/clk_freq
        self.config["RTIO_FREQUENCY"] = str(clk_freq/1e6)
        self.config["CLOCK_FREQUENCY"] = int(clk_freq)

        if with_wrpll:
            clk_synth = platform.request("cdr_clk_clean_fabric")
            clk_synth_se = Signal()
            platform.add_period_constraint(clk_synth.p, 8.0)
            self.specials += Instance("IBUFGDS", p_DIFF_TERM="TRUE", p_IBUF_LOW_PWR="FALSE", i_I=clk_synth.p, i_IB=clk_synth.n, o_O=clk_synth_se)
            self.submodules.wrpll = wrpll.WRPLL(
                platform=self.platform,
                cd_ref=self.gt_drtio.cd_rtio_rx0,
                main_clk_se=clk_synth_se)
            self.submodules.wrpll_skewtester = wrpll.SkewTester(self.rx_synchronizer)
            self.csr_devices.append("wrpll_skewtester")
            self.csr_devices.append("wrpll")
            self.comb += self.ps7.core.core0.nfiq.eq(self.wrpll.ev.irq)
            self.config["HAS_SI549"] = None
            self.config["WRPLL_REF_CLK"] = "GT_CDR"
        else:
            self.submodules.siphaser = SiPhaser7Series(
                si5324_clkin=platform.request("cdr_clk"),
                rx_synchronizer=self.rx_synchronizer,
                ultrascale=False,
                rtio_clk_freq=self.gt_drtio.rtio_clk_freq)
            self.csr_devices.append("siphaser")
            self.config["HAS_SI5324"] = None
            self.config["SI5324_SOFT_RESET"] = None

        gtx0 = self.gt_drtio.gtxs[0]
        platform.add_false_path_constraints(
            gtx0.txoutclk, gtx0.rxoutclk)

        if has_grabber:
            self.config["HAS_GRABBER"] = None
            self.add_csr_group("grabber", self.grabber_csr_group)
            # no RTIO CRG here
        
        self.submodules.virtual_leds = virtual_leds.VirtualLeds()
        self.csr_devices.append("virtual_leds")

        self.comb += [self.virtual_leds.get(i).eq(channel.rx_ready)
                for i, channel in enumerate(self.gt_drtio.channels)]

def main():
    parser = argparse.ArgumentParser(
        description="ARTIQ device binary builder for generic Kasli-SoC systems")
    parser.add_argument("-r", default=None,
        help="build Rust interface into the specified file")
    parser.add_argument("-c", default=None,
        help="build Rust compiler configuration into the specified file")
    parser.add_argument("-m", default=None,
        help="build Rust memory interface into the specified file")
    parser.add_argument("-g", default=None,
        help="build gateware into the specified directory")
    parser.add_argument("--acpki", default=False, action="store_true",
        help="enable ACPKI")
    parser.add_argument("description", metavar="DESCRIPTION",
                        help="JSON system description file")
    args = parser.parse_args()
    description = jsondesc.load(args.description)

    if description["target"] != "kasli_soc":
        raise ValueError("Description is for a different target")

    if description["drtio_role"] == "standalone":
        cls = GenericStandalone
    elif description["drtio_role"] == "master":
        cls = GenericMaster
    elif description["drtio_role"] == "satellite":
        cls = GenericSatellite
    else:
        raise ValueError("Invalid DRTIO role")

    soc = cls(description, acpki=args.acpki)
    soc.finalize()

    if args.r is not None:
        write_csr_file(soc, args.r)
    if args.m is not None:
        write_mem_file(soc, args.m)
    if args.c is not None:
        write_rustc_cfg_file(soc, args.c)
    if args.g is not None:
        soc.build(build_dir=args.g)


if __name__ == "__main__":
    main()
