use std::{
	net::{IpAddr, Ipv4Addr, SocketAddr},
	sync::Arc,
	time::Duration,
};

use chrono::Datelike;
use futures::lock::Mutex;
use hap::{
	Config as HapConfig, HapType, MacAddress, Pin, Result,
	accessory::{
		AccessoryCategory, AccessoryInformation, HapAccessory, bridge::BridgeAccessory, light_sensor::LightSensorAccessory,
	},
	server::{IpServer, Server},
	storage::{FileStorage, Storage},
};
use log::{error, info, warn};
use tokio_modbus::prelude::*;

#[derive(Debug, Clone)]
pub struct Reading {
	pub power_watts: i32,
	/// Grid meter watts (negative = export). Only present when read from the meter inverter.
	pub meter_watts: Option<i32>,
}

#[derive(Debug)]
pub struct Config {
	pub inverters: Vec<SocketAddr>,
	/// The inverter with the grid meter attached (for export readings via register 5600)
	pub meter_ip: SocketAddr,
	pub pin: [u8; 8],
	pub name: String,
	pub host: IpAddr,
	pub device_id: [u8; 6],
	pub interval: Duration,
}

impl Default for Config {
	fn default() -> Self {
		Config {
			inverters: Vec::new(),
			meter_ip: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 502),
			pin: [1, 1, 1, 2, 2, 3, 3, 3],
			name: String::from("Solar Monitor"),
			host: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
			device_id: [10, 20, 30, 40, 50, 60],
			interval: Duration::from_secs(10),
		}
	}
}

pub struct SolarMonitor {
	config: Config,
}

impl SolarMonitor {
	pub fn new(config: Config) -> Self {
		SolarMonitor { config }
	}

	pub async fn start(&self) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
		env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

		let bridge = BridgeAccessory::new(
			1,
			AccessoryInformation {
				name: self.config.name.clone(),
				manufacturer: String::from("DomSoft"),
				model: String::from("DomSoft Bridge"),
				serial_number: String::from("SOLARBRIDGE0"),
				..Default::default()
			},
		)?;

		let production_sensor = LightSensorAccessory::new(
			2,
			AccessoryInformation {
				name: String::from("Production Real-time"),
				manufacturer: String::from("DomSoft"),
				model: String::from("Solar production in real time in watts"),
				serial_number: String::from("SOLARSENSOR1"),
				..Default::default()
			},
		)?;

		let export_sensor = LightSensorAccessory::new(
			3,
			AccessoryInformation {
				name: String::from("Export Real-time"),
				manufacturer: String::from("DomSoft"),
				model: String::from("Solar export in real time in watts"),
				serial_number: String::from("SOLARSENSOR2"),
				..Default::default()
			},
		)?;

		let export_today_sensor = LightSensorAccessory::new(
			4,
			AccessoryInformation {
				name: String::from("Export Today"),
				manufacturer: String::from("DomSoft"),
				model: String::from("Solar export for all of today in kWh"),
				serial_number: String::from("SOLARSENSOR3"),
				..Default::default()
			},
		)?;

		let mut storage = FileStorage::current_dir().await?;

		let config = match storage.load_config().await {
			Ok(mut config) => {
				config.host = self.config.host;
				storage.save_config(&config).await?;
				config
			},
			Err(_) => {
				let config = HapConfig {
					host: self.config.host,
					pin: Pin::new(self.config.pin)?,
					name: self.config.name.clone(),
					device_id: MacAddress::from(self.config.device_id),
					category: AccessoryCategory::Bridge,
					..Default::default()
				};
				storage.save_config(&config).await?;
				config
			},
		};

		let server = IpServer::new(config, storage).await?;
		server.add_accessory(bridge).await?;
		let production_ptr = server.add_accessory(production_sensor).await?;
		let export_ptr = server.add_accessory(export_sensor).await?;
		let export_today_ptr = server.add_accessory(export_today_sensor).await?;

		let handle = server.run_handle();
		info!("HomeKit bridge started - pair with PIN {:?}", self.config.pin);

		let inverter_addrs = self.config.inverters.clone();
		info!("Starting polling inverters every {}s", self.config.interval.as_secs());
		let poller = async move {
			let mut interval = tokio::time::interval(self.config.interval);
			let mut export_kwh_today: f64 = 0.0;
			let mut last_export_watts: Option<i32> = None;
			let mut last_tick = tokio::time::Instant::now();
			let mut last_date = chrono::Local::now().ordinal();

			loop {
				interval.tick().await;
				let now = tokio::time::Instant::now();
				let elapsed_secs = (now - last_tick).as_secs_f64();
				last_tick = now;

				// Reset daily accumulator when the date changes (before accumulating)
				let today = chrono::Local::now().ordinal();
				if today != last_date {
					info!("Day changed: export today was {export_kwh_today:.2}kWh");
					export_kwh_today = 0.0;
					last_export_watts = None;
					last_date = today;
				}

				// Read all inverters for production watts.
				// The meter inverter also reads register 5600 in the same connection.
				let mut tasks = tokio::task::JoinSet::new();
				for (index, addr) in inverter_addrs.iter().enumerate() {
					let addr = *addr;
					let read_meter = addr == self.config.meter_ip;
					tasks.spawn(async move { (index, Self::read_inverter(addr, read_meter).await) });
				}

				let mut readings: Vec<Option<Reading>> = vec![None; inverter_addrs.len()];
				while let Some(result) = tasks.join_next().await {
					let (index, read_result) = result?;
					match read_result {
						Ok(reading) => {
							info!("Inverter {index}: {}W", reading.power_watts);
							readings[index] = Some(reading);
						},
						Err(error) => warn!("Inverter {index} read failed: {error}"),
					}
				}

				let total_watts = readings.iter().flatten().map(|reading| reading.power_watts.max(0)).sum::<i32>();

				// Extract meter reading from whichever inverter had it
				let meter_watts = readings.iter().flatten().find_map(|r| r.meter_watts);
				let export_watts = match meter_watts {
					Some(watts) => {
						// Negative = exporting, positive = importing. Negate so positive = export.
						let export = (-watts).max(0);
						info!("Grid meter: {watts}W (export: {export}W)");

						// Accumulate daily export kWh (trapezoidal integration using actual elapsed time)
						if let Some(prev) = last_export_watts {
							let avg_watts = (prev as f64 + export as f64) / 2.0;
							let kwh_increment = avg_watts * elapsed_secs / 3600.0;
							export_kwh_today += kwh_increment;
						}
						last_export_watts = Some(export);

						export
					},
					None => {
						warn!("Meter read unavailable this cycle");
						// Don't update last_export_watts — skip integration next tick rather
						// than integrating against a fake zero.
						last_export_watts = None;
						0
					},
				};

				info!(
					"REPORT: Production RL: {total_watts}W | Export RL: {export_watts}W | Export today: {export_kwh_today:.2}kWh"
				);

				if let Err(error) = Self::set_lux(&production_ptr, Self::clamp_lux(total_watts as f64)).await {
					error!("failed to update production: {error}");
				}
				if let Err(error) = Self::set_lux(&export_ptr, Self::clamp_lux(export_watts as f64)).await {
					error!("failed to update export: {error}");
				}
				// Export today in lux: kWh * 100 (so 5.2kWh = 520 lux)
				if let Err(error) = Self::set_lux(&export_today_ptr, Self::clamp_lux(export_kwh_today * 100.0)).await {
					error!("failed to update export today: {error}");
				}
			}

			// The loop never exits, but the async block needs a return type for try_join!
			#[allow(unreachable_code)]
			Ok(())
		};

		futures::try_join!(handle, poller)?;
		Ok(())
	}

	// HomeKit light sensors: lux range 0.0001–100000
	fn clamp_lux(value: f64) -> f64 {
		value.max(0.0001)
	}

	/// Decode a word-swapped int32 from two consecutive Modbus registers.
	/// Sungrow sends low-word first: value = (r1 << 16) | r0
	fn decode_i32_word_swap(low_word: u16, high_word: u16) -> i32 {
		let combined = ((high_word as u32) << 16) | (low_word as u32);
		combined as i32
	}

	/// Read inverter power via Modbus, and optionally the grid meter (register 5600)
	/// in the same connection when `read_meter` is true.
	async fn read_inverter(
		ip: SocketAddr,
		read_meter: bool,
	) -> std::result::Result<Reading, Box<dyn std::error::Error + Send + Sync>> {
		let mut modbus = tcp::connect_slave(ip, Slave(1)).await?;

		// Register 5008-5009: Total active power (I32, word-swapped, watts)
		// Works on SG string inverters.
		// Falls back to 5018 (U16, total active power, watts) for inverters that
		// don't support 5008 (e.g. some SH hybrid models).
		let power_watts = match modbus.read_input_registers(5008, 2).await? {
			Ok(registers) => {
				let watts = Self::decode_i32_word_swap(registers[0], registers[1]);
				if watts != 0 {
					watts
				} else {
					// 5008 returned 0, try 5018 (single U16 active power) as fallback
					match modbus.read_input_registers(5018, 1).await? {
						Ok(registers) => registers[0] as i32,
						Err(_) => 0,
					}
				}
			},
			Err(_) => {
				// 5008 not supported, try 5018 (single U16 active power)
				match modbus.read_input_registers(5018, 1).await? {
					Ok(registers) => registers[0] as i32,
					Err(_) => 0,
				}
			},
		};

		// Register 5600-5601: Meter active power (I32, word-swapped, watts)
		// Negative = exporting, positive = importing.
		let meter_watts = if read_meter {
			match modbus.read_input_registers(5600, 2).await? {
				Ok(registers) => Some(Self::decode_i32_word_swap(registers[0], registers[1])),
				Err(error) => {
					warn!("register 5600 error: {error}");
					None
				},
			}
		} else {
			None
		};

		modbus.disconnect().await?;

		Ok(Reading {
			power_watts,
			meter_watts,
		})
	}

	/// Set the KWH as lux for the sensor
	async fn set_lux(sensor_ptr: &Arc<Mutex<Box<dyn HapAccessory>>>, value: f64) -> Result<()> {
		let mut accessory = sensor_ptr.lock().await;
		let service = accessory.get_mut_service(HapType::LightSensor).expect("missing LightSensor service");
		let characteristic =
			service.get_mut_characteristic(HapType::CurrentLightLevel).expect("missing lux characteristic");
		characteristic.set_value(serde_json::Value::from(value)).await
	}
}
