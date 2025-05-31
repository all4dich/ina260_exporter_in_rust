extern crate i2cdev;

use i2cdev::core::*;
#[cfg(any(target_os = "linux"))]
use i2cdev::linux::*;
use byteorder::{BigEndian, ByteOrder};
use std::thread;
use std::time::Duration;
use clap::Parser;
use prometheus::{Encoder, TextEncoder, gather, GaugeVec};
use std::net::SocketAddr;
use warp::Filter;
use anyhow::{Result, Context};
use log::{info, error, warn};

// INA260 I2C address
const INA260_ADDRESS: u16 = 0x40; // Default INA260 I2C address

// INA260 Register Addresses
const INA260_REG_CONFIG: u8      = 0x00; // Configuration Register
const INA260_REG_CURRENT: u8    = 0x01; // Current Register
const INA260_REG_BUS_VOLTAGE: u8 = 0x02; // Bus Voltage Register
const INA260_REG_POWER: u8       = 0x03; // Power Register
const INA260_REG_MANUF_ID: u8    = 0xFE; // Manufacturer ID Register
const INA260_REG_DEVICE_ID: u8   = 0xFF; // Device ID Register

// INA260 Scaling Factors
const VOLTAGE_LSB: f64 = 1.25; // mV/LSB for Bus Voltage Register
const CURRENT_LSB: f64 = 1.25; // mA/LSB for Current Register
const POWER_LSB: f64   = 10.0; // mW/LSB for Power Register

// Defines command-line arguments for the application.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value_t = String::from("0x70"), help = "TCA9548A I2C address (e.g., 0x70)")]
    tca_address: String,

    #[arg(short, long, default_value_t = 0, help = "TCA9548A channel number for INA260 (0-7)")]
    channel: u8,

    #[arg(short, long, default_value_t = String::from("/dev/i2c-1"), help = "I2C bus device path (e.g., /dev/i2c-1)")]
    i2c_bus: String,
}

// Prometheus gauges. `lazy_static` ensures they are initialized once.
lazy_static::lazy_static! {
    static ref INA260_CURRENT: GaugeVec =
        GaugeVec::new(
            prometheus::Opts::new("ina260_current", "Current measured by INA260 sensor in Amperes."),
            &["hostname", "device"]
        ).unwrap();
    static ref INA260_VOLTAGE: GaugeVec =
        GaugeVec::new(
            prometheus::Opts::new("ina260_voltage", "Bus voltage measured by INA260 sensor in Volts."),
            &["hostname", "device"]
        ).unwrap();
    static ref INA260_POWER: GaugeVec =
        GaugeVec::new(
            prometheus::Opts::new("ina260_power", "Power measured by INA260 sensor in Watts."),
            &["hostname", "device"]
        ).unwrap();
}

/// Reads a 16-bit value from the specified INA260 register.
/// The INA260 returns data in Big-Endian format.
fn read_ina260_reg(i2c: &mut LinuxI2CDevice, device_addr: u16, reg: u8) -> Result<u16> {
    let mut write_buf = [reg];
    let mut read_buf = [0; 2]; // 16-bit (2 bytes)

    // Create I2C messages for combined write-then-read transaction.
    // The address is specified per message, crucial for shared bus scenarios.
    //let msgs = &mut [
    //    LinuxI2CMessage::write { address: device_addr, data: &mut write_buf },
    //    LinuxI2CMessage::read { address: device_addr, data: &mut read_buf },
    //];
    // Cast device_addr to u8 for the I2C message.
    let device_addr = device_addr as u8;
    let msgs = &mut [
        LinuxI2CMessage::write(&[0x40]),
        LinuxI2CMessage::read(&mut read_buf),
    ];

    i2c.transfer(msgs)
        .context(format!("Failed to perform I2C transaction on register 0x{:02x} for device 0x{:02x}", reg, device_addr))?;

    Ok(BigEndian::read_u16(&read_buf))
}

#[tokio::main] // Enables asynchronous features for the HTTP server
async fn main() -> Result<()> {
    // Initialize standard logging.
    env_logger::init();

    // Register Prometheus gauges with the default registry.
    prometheus::default_registry().register(Box::new(INA260_CURRENT.clone()))?;
    prometheus::default_registry().register(Box::new(INA260_VOLTAGE.clone()))?;
    prometheus::default_registry().register(Box::new(INA260_POWER.clone()))?;

    // Parse command-line arguments.
    let args = Args::parse();

    // Retrieve the system hostname for Prometheus labels.
    let hostname = hostname::get()
        .context("Failed to get hostname")?
        .into_string()
        .map_err(|e| anyhow::anyhow!("Hostname is not valid UTF-8: {:?}", e))?;

    info!("Initializing I2C bus: {}", args.i2c_bus);
    // Open the I2C bus device (e.g., /dev/i2c-1).
    let mut i2c = LinuxI2CDevice::new(&args.i2c_bus, 0x70)
        .context(format!("Failed to open I2C bus at {}", args.i2c_bus))?;

    // --- TCA9548A Multiplexer Handling ---
    // Parse TCA address (supports "0x" prefix).
    let tca_address_str = if args.tca_address.starts_with("0x") {
        args.tca_address.trim_start_matches("0x")
    } else {
        &args.tca_address
    };
    let tca_address = u16::from_str_radix(tca_address_str, 16)
        .context(format!("Invalid TCA address format: {}", args.tca_address))?;

    info!("Using TCA9548A at address: 0x{:02X}", tca_address);

    // Validate and prepare the channel selection byte for TCA9548A.
    let channel_int = args.channel;
    if channel_int > 7 {
        anyhow::bail!("Channel number must be between 0 and 7, got {}", channel_int);
    }
    let ina260_channel = channel_int as u8;
    let channel_selection_byte = 1 << ina260_channel;

    // Set the I2C slave address to the TCA9548A and write the channel selection byte.
    // This routes subsequent communications on this bus to the selected channel.
    info!("TCA9548A: Setting slave address for TCA to 0x{:02X}", tca_address);
    i2c.set_slave_address(tca_address)?;
    info!("TCA9548A: Selected channel {}", ina260_channel);
    i2c.write(&[channel_selection_byte])
        .context(format!("Failed to select channel {} on TCA9548A", ina260_channel))?;

    // Define the device label for Prometheus metrics.
    let device_label = format!("tca9548a_0x{:02X}_ch{}_ina260", tca_address, ina260_channel);
    info!("Device label: {}", device_label);

    // --- INA260 Communication Verification ---
    // Set the I2C slave address to the INA260.
    info!("Setting slave address for INA260 to 0x{:02X}", INA260_ADDRESS);
    i2c.set_slave_address(INA260_ADDRESS)?;

    // Read Manufacturer ID and Device ID to verify communication.
    // Expected Manufacturer ID: 0x5449 (TI), Device ID: 0x2260 (INA260).
    let manuf_id = read_ina260_reg(&mut i2c, INA260_ADDRESS, INA260_REG_MANUF_ID)
        .context("Failed to read INA260 Manufacturer ID")?;
    let device_id = read_ina260_reg(&mut i2c, INA260_ADDRESS, INA260_REG_DEVICE_ID)
        .context("Failed to read INA260 Device ID")?;
    info!("INA260: Manufacturer ID: 0x{:04X}, Device ID: 0x{:04X}", manuf_id, device_id);
    if manuf_id != 0x5449 || device_id != 0x2260 {
        warn!(
            "Unexpected INA260 Manufacturer ID or Device ID. Expected 0x5449/0x2260, got 0x{:04X}/0x{:04X}",
            manuf_id, device_id
        );
    }

    // --- Start Prometheus HTTP Server ---
    // Define the `/metrics` endpoint to serve Prometheus metrics.
    let metrics_route = warp::path!("metrics").map(|| {
        let encoder = TextEncoder::new();
        let metric_families = gather(); // Collect all registered metrics.
        let mut buffer = vec![];
        encoder.encode(&metric_families, &mut buffer).unwrap(); // Encode metrics to text format.
        String::from_utf8(buffer).unwrap()
    });

    let addr = SocketAddr::from(([0, 0, 0, 0], 9090)); // Listen on all interfaces, port 9090.
    info!("Starting Prometheus metrics server on {}", addr);
    // Spawn the HTTP server in a separate Tokio task to run concurrently.
    tokio::spawn(async move {
        warp::serve(metrics_route).run(addr).await;
    });

    // --- Main Sensing Loop ---
    info!("Reading INA260 values (Voltage, Current, Power)...");
    loop {
        // Explicitly set the slave address for INA260 again before reading,
        // in case other I2C operations (if any were added) changed it.
        i2c.set_slave_address(INA260_ADDRESS)?;

        // Read Current (Register 0x01).
        let raw_current_res = read_ina260_reg(&mut i2c, INA260_ADDRESS, INA260_REG_CURRENT);
        let current = match raw_current_res {
            Ok(raw_current) => {
                // The Current Register (0x01) is a 16-bit two's complement signed integer.
                // Convert raw current (mA) to Amperes (A).
                (raw_current as i16) as f64 * CURRENT_LSB / 1000.0
            }
            Err(e) => {
                error!("Error reading current from INA260: {:?}", e);
                thread::sleep(Duration::from_secs(1)); // Wait before retrying.
                continue;
            }
        };

        // Read Voltage (Register 0x02).
        let raw_voltage_res = read_ina260_reg(&mut i2c, INA260_ADDRESS, INA260_REG_BUS_VOLTAGE);
        let voltage = match raw_voltage_res {
            Ok(raw_voltage) => {
                // Convert raw voltage (mV) to Volts (V).
                raw_voltage as f64 * VOLTAGE_LSB / 1000.0
            }
            Err(e) => {
                error!("Error reading Bus Voltage from INA260: {:?}", e);
                thread::sleep(Duration::from_secs(1));
                continue;
            }
        };

        // Read Power (Register 0x03).
        let raw_power_res = read_ina260_reg(&mut i2c, INA260_ADDRESS, INA260_REG_POWER);
        let power = match raw_power_res {
            Ok(raw_power) => {
                // Convert raw power (mW) to Watts (W).
                raw_power as f64 * POWER_LSB / 1000.0
            }
            Err(e) => {
                error!("Error reading power from INA260: {:?}", e);
                thread::sleep(Duration::from_secs(1));
                continue;
            }
        };

        info!("Voltage: {:.3} V, Current: {:.3} A, Power: {:.3} W", voltage, current, power);

        // Update Prometheus gauges with the collected data and labels.
        INA260_CURRENT.with_label_values(&[&hostname, &device_label]).set(current);
        INA260_VOLTAGE.with_label_values(&[&hostname, &device_label]).set(voltage);
        INA260_POWER.with_label_values(&[&hostname, &device_label]).set(power);

        thread::sleep(Duration::from_secs(1)); // Wait for 1 second before the next reading.
    }
}

