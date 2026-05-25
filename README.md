# PicoFlasher Client

PicoFlasher and ESPFlasher Client for windows, linux and macos

Supports [ESPFlasher](https://github.com/warp32767/espflasher) TCP server and [Hax360 PicoFlasher v4](https://codeberg.org/hax360/PicoFlasher) USB. 

## Usage

Default target is the ESP32 SoftAP IP/port (`192.168.4.1:3232`).

```bash
./target/release/picoclient read-nand --out nand.bin
./target/release/picoclient write-nand --input nand.bin
./target/release/picoclient read-emmc --out emmc.bin
./target/release/picoclient write-emmc --input emmc.bin
```

Read NAND (auto-detects flash size from flash config):

```bash
./target/release/picoclient read-nand --out nand.bin
```

Write NAND from a file (file must be 0x210-per-block layout):

```bash
./target/release/picoclient write-nand --start 0 --input nand.bin
```

Read eMMC (auto-detects size from EXT\_CSD SEC\_COUNT):

```bash
./target/release/picoclient read-emmc --out emmc.bin
```

Override address/timeout:

```bash
./target/release/picoclient --ip 192.168.4.1:3232 --timeout-ms 5000 read-nand --out nand.bin
```

Use USB serial (original PicoFlasher CDC COM port):

```bash
./target/release/picoclient --serial /dev/ttyACM0 read-nand --out nand.bin
./target/release/picoclient --serial COM3 write-nand --input nand.bin
```
