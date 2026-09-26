[package]
name = "libboard_artiq"
version = "0.0.0"
authors = ["M-Labs"]
edition = "2018"

[lib]
name = "libboard_artiq"

[features]
target_zc706 = ["libboard_zynq/target_zc706", "libconfig/target_zc706"]
target_kasli_soc = ["libboard_zynq/target_kasli_soc", "libconfig/target_kasli_soc"]
calibrate_wrpll_skew = []

[build-dependencies]
build_zynq = { path = "../libbuild_zynq" }

[dependencies]
log = "0.4"
log_buffer = { version = "1.2" }
crc = { version = "1.7", default-features = false }
core_io = { version = "0.1", features = ["collections"] }
embedded-hal = "0.2"
nb = "1.0"
void = { version = "1", default-features = false }
byteorder = { version = "1.3", default-features = false }

io = { path = "../libio", features = ["alloc", "byteorder"] }
libboard_zynq = { path = "@@ZYNQ_RS@@/libboard_zynq" }
libsupport_zynq = { path = "@@ZYNQ_RS@@/libsupport_zynq", default-features = false, features = ["alloc_core"] }
libregister = { path = "@@ZYNQ_RS@@/libregister" }
libconfig = { path = "@@ZYNQ_RS@@/libconfig", features = ["fat_lfn"] }
libcortex_a9 = { path = "@@ZYNQ_RS@@/libcortex_a9" }
libasync = { path = "@@ZYNQ_RS@@/libasync" }
