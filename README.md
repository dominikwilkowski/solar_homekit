Solar HomeKit
=============

> This is my personal HomeKit sensor that reports what my solar produced to HomeKit

It uses my Synology DS224+ to host the binary and then queries my Sungrow inverters (one hybrid) and registers a HomeKit
bridge with two sensors plus one per inverter to make daily/total kWh and current kWh per inverter available as lux
color temperature (as wattage isn't supported at this time).
With this setup I get to add HomeKit automations that will trigger when my solar produces above or below a certain
amount.

## Deploy

### Dependencies

```sh
rustup target add x86_64-unknown-linux-musl
brew install messense/macos-cross-toolchains/x86_64-unknown-linux-musl
```

Cross compiling solar_homekit to Synology DS224+

```sh
cargo build --release --target x86_64-unknown-linux-musl
```

Copy the binary over

```sh
ssh dominik@10.0.0.2 "cat > /volume1/docker/solar_homekit/solar_homekit && chmod +x /volume1/docker/solar_homekit/solar_homekit" < /Users/dominik/Sites/solar_homekit/target/x86_64-unknown-linux-musl/release/solar_homekit

ssh dominik@10.0.0.2
cd /volume1/docker/solar_homekit
./solar_homekit >> solar_homekit.log 2>&1 &

# check if running
ps -ef | grep solar_homekit
tail -f solar_homekit.log

# kill running
ps -ef | grep solar_homekit
kill <pid>
```

### Auto-start on boot

DSM > Control Panel > Task Scheduler<br>
Create > Triggered Task > User-defined script<br>
General:
	- name: `Solar HomeKit`
	- user: `root`
	- event: `Boot-up`
Task Settings script:

```sh
#!/bin/bash
cd /volume1/docker/solar_homekit
./solar_homekit >> /volume1/docker/solar_homekit/solar_homekit.log 2>&1 &
```
