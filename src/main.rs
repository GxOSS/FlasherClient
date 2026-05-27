mod cli;
mod pftcp;
mod pfc;
mod usb;

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;

use crate::cli::{Cli, Command};
use crate::pfc::{
	Client, CMD_EMMC_DETECT, CMD_EMMC_GET_EXT_CSD, CMD_EMMC_INIT, CMD_EMMC_READ, CMD_EMMC_READ_STREAM,
	CMD_EMMC_WRITE, CMD_EMMC_WRITE_MULTI, CMD_GET_FLASH_CONFIG, CMD_GET_VERSION, CMD_READ_FLASH,
	CMD_READ_FLASH_STREAM, CMD_SET_SMC_WORKAROUND, CMD_START_SMC, CMD_STOP_SMC, CMD_WRITE_FLASH,
	CMD_WRITE_FLASH_MULTI, EMMC_BLOCK_BYTES, NAND_BLOCK_BYTES,
};

fn main() -> Result<()> {
	let cli = Cli::parse();
	let timeout = Duration::from_millis(cli.timeout_ms);
	let (mut client, resolved) = if let Some(port) = &cli.serial {
		Client::connect_usb(port, timeout).with_context(|| format!("failed to open serial {port}"))?
	} else {
		Client::connect_tcp(&cli.addr, timeout)
			.with_context(|| format!("failed to connect to {}", cli.addr))?
	};

	eprintln!("connected to {resolved}");

	match cli.command {
		Command::ReadNand { out, start, count } => {
			let (flash_config, blocks_total) = prepare_nand(&mut client)?;
			let blocks = count.unwrap_or(blocks_total.saturating_sub(start));
			eprintln!("flash_config=0x{flash_config:08x} blocks={blocks} start={start}");
			read_nand(&mut client, out, start, blocks)?;
			println!("ok");
		}
		Command::WriteNand { input, start } => {
			let (flash_config, blocks_total) = prepare_nand(&mut client)?;
			eprintln!("flash_config=0x{flash_config:08x} start={start} max_blocks={blocks_total}");
			write_nand(&mut client, input, start)?;
			println!("ok");
		}
		Command::ReadEmmc { out, start, count } => {
			let blocks_total = prepare_emmc(&mut client)?;
			let blocks = count.unwrap_or(blocks_total.saturating_sub(start));
			eprintln!("emmc_blocks={blocks} start={start}");
			read_emmc(&mut client, out, start, blocks)?;
			println!("ok");
		}
		Command::WriteEmmc { input, start } => {
			let blocks_total = prepare_emmc(&mut client)?;
			eprintln!("start={start} max_blocks={blocks_total}");
			write_emmc(&mut client, input, start)?;
			println!("ok");
		}
	}

	Ok(())
}

fn prepare_nand(client: &mut Client) -> Result<(u32, u32)> {
	let _ver = client.cmd_u32(CMD_GET_VERSION, 0)?;
	let _ = client.cmd_u32(CMD_SET_SMC_WORKAROUND, 0)?;
	let _ = client.cmd_u32(CMD_STOP_SMC, 0)?;
	std::thread::sleep(Duration::from_millis(500));

	let flash_config = client.cmd_u32(CMD_GET_FLASH_CONFIG, 0)?;
	if flash_config == 0 || flash_config == 0xFFFF_FFFF {
		bail!("console not found (flash_config=0x{flash_config:08x})");
	}

	let flash_size_bytes = flash_size_from_config(flash_config)
		.ok_or_else(|| anyhow::anyhow!("unknown flash size for flash_config=0x{flash_config:08x}"))?;
	let blocks = (flash_size_bytes / 512) as u32;
	Ok((flash_config, blocks))
}

fn flash_size_from_config(flash_config: u32) -> Option<usize> {
	let major = (flash_config >> 17) & 3;
	let minor = (flash_config >> 4) & 3;

	let size_mb = if major >= 1 {
		match minor {
			0 => {
				if major != 1 {
					16
				} else {
					return None;
				}
			}
			1 => {
				if major != 1 {
					64
				} else {
					16
				}
			}
			2 | 3 => {
				let a = (flash_config >> 19) & 0x3;
				let b = (flash_config >> 21) & 0xF;
				8usize.checked_shl((a + b) as u32)?
			}
			_ => return None,
		}
	} else {
		8usize.checked_shl(minor as u32)?
	};

	Some(size_mb * 1024 * 1024)
}

fn read_nand(client: &mut Client, out: std::path::PathBuf, start: u32, count: u32) -> Result<()> {
	let f = File::create(out).context("open output")?;
	let mut f = BufWriter::with_capacity(1024 * 1024, f);

	if start == 0 {
		client.start_stream(CMD_READ_FLASH_STREAM, count)?;
		let mut rxbuf = vec![0u8; 4 + NAND_BLOCK_BYTES];
		for i in 0..count {
			let (ret, data) = client.recv_stream_block_into(&mut rxbuf, NAND_BLOCK_BYTES)?;
			if ret != 0 {
				bail!("read failed at block {i}: 0x{ret:08x}");
			}
			f.write_all(data.unwrap()).context("write output")?;

			if (i & 0xFF) == 0 {
				eprintln!("read {}/{} blocks", i + 1, count);
			}
		}
	} else {
		for i in 0..count {
			let lba = start + i;
			let (ret, data) = client.read_with_ret(CMD_READ_FLASH, lba, NAND_BLOCK_BYTES)?;
			if ret != 0 {
				bail!("read failed at lba {lba}: 0x{ret:08x}");
			}
			f.write_all(&data.unwrap()).context("write output")?;

			if (i & 0xFF) == 0 {
				eprintln!("read {}/{} blocks", i + 1, count);
			}
		}
	}

	Ok(())
}

fn write_nand(client: &mut Client, input: std::path::PathBuf, start: u32) -> Result<()> {
	let mut buf = vec![];
	File::open(input)
		.context("open input")?
		.read_to_end(&mut buf)
		.context("read input")?;

	if buf.len() % NAND_BLOCK_BYTES != 0 {
		bail!("input size must be a multiple of 0x210 (got 0x{:x})", buf.len());
	}

	let blocks = (buf.len() / NAND_BLOCK_BYTES) as u32;
	let mut i = 0u32;
	if client.supports_multi_write() {
		while i < blocks {
			let remaining = blocks - i;
			let chunk_blocks = remaining.min(32);
			let lba = start + i;

			let off = (i as usize) * NAND_BLOCK_BYTES;
			let end = off + (chunk_blocks as usize) * NAND_BLOCK_BYTES;
			let (ret, idx) = client.write_multi(CMD_WRITE_FLASH_MULTI, lba, NAND_BLOCK_BYTES, &buf[off..end])?;
			if ret != 0 {
				bail!("write failed at lba {}: 0x{ret:08x}", lba + idx);
			}

			i += chunk_blocks;
			eprintln!("written {}/{} blocks", i, blocks);
		}
	} else {
		while i < blocks {
			let lba = start + i;
			let off = (i as usize) * NAND_BLOCK_BYTES;
			let end = off + NAND_BLOCK_BYTES;
			let ret = client.write_single(CMD_WRITE_FLASH, lba, &buf[off..end])?;
			if ret != 0 {
				bail!("write failed at lba {}: 0x{ret:08x}", lba);
			}
			i += 1;
			if (i & 0xFF) == 0 || i == blocks {
				eprintln!("written {}/{} blocks", i, blocks);
			}
		}
	}

	Ok(())
}

fn prepare_emmc(client: &mut Client) -> Result<u32> {
	let _ver = client.cmd_u32(CMD_GET_VERSION, 0)?;
	let _ = client.cmd_u32(CMD_SET_SMC_WORKAROUND, 0)?;
	let _ = client.cmd_u32(CMD_STOP_SMC, 0)?;
	std::thread::sleep(Duration::from_millis(500));

	let detect = client.cmd_u8(CMD_EMMC_DETECT, 0)?;
	if detect == 0 {
		bail!("eMMC not detected");
	}

	let ret = client.cmd_u32(CMD_EMMC_INIT, 0)?;
	if ret != 0 {
		bail!("EMMC_INIT failed: {ret}");
	}

	let ext = client.cmd_exact_bytes(CMD_EMMC_GET_EXT_CSD, 0, 512)?;

	let sec_count = u32::from_le_bytes(ext[212..216].try_into().unwrap());
	if sec_count == 0 {
		bail!("invalid EXT_CSD SEC_COUNT=0");
	}
	Ok(sec_count)
}

fn read_emmc(client: &mut Client, out: std::path::PathBuf, start: u32, count: u32) -> Result<()> {
	let f = File::create(out).context("open output")?;
	let mut f = BufWriter::with_capacity(1024 * 1024, f);

	if start == 0 {
		client.start_stream(CMD_EMMC_READ_STREAM, count)?;
		let mut rxbuf = vec![0u8; 4 + EMMC_BLOCK_BYTES];
		for i in 0..count {
			let (ret, data) = client.recv_stream_block_into(&mut rxbuf, EMMC_BLOCK_BYTES)?;
			if ret != 0 {
				bail!("read failed at block {i}: {ret}");
			}
			f.write_all(data.unwrap()).context("write output")?;

			if (i & 0xFF) == 0 {
				eprintln!("read {}/{} blocks", i + 1, count);
			}
		}
	} else {
		for i in 0..count {
			let lba = start + i;
			let (ret, data) = client.read_with_ret(CMD_EMMC_READ, lba, EMMC_BLOCK_BYTES)?;
			if ret != 0 {
				bail!("read failed at lba {lba}: {ret}");
			}
			f.write_all(&data.unwrap()).context("write output")?;

			if (i & 0xFF) == 0 {
				eprintln!("read {}/{} blocks", i + 1, count);
			}
		}
	}

	Ok(())
}

fn write_emmc(client: &mut Client, input: std::path::PathBuf, start: u32) -> Result<()> {
	let mut buf = vec![];
	File::open(input)
		.context("open input")?
		.read_to_end(&mut buf)
		.context("read input")?;

	if buf.len() % EMMC_BLOCK_BYTES != 0 {
		bail!("input size must be a multiple of 0x200 (got 0x{:x})", buf.len());
	}

	let blocks = (buf.len() / EMMC_BLOCK_BYTES) as u32;
	let mut i = 0u32;
	if client.supports_multi_write() {
		while i < blocks {
			let remaining = blocks - i;
			let chunk_blocks = remaining.min(32);
			let lba = start + i;

			let off = (i as usize) * EMMC_BLOCK_BYTES;
			let end = off + (chunk_blocks as usize) * EMMC_BLOCK_BYTES;
			let (ret, idx) = client.write_multi(CMD_EMMC_WRITE_MULTI, lba, EMMC_BLOCK_BYTES, &buf[off..end])?;
			if ret != 0 {
				bail!("write failed at lba {}: {ret}", lba + idx);
			}

			i += chunk_blocks;
			eprintln!("written {}/{} blocks", i, blocks);
		}
	} else {
		while i < blocks {
			let lba = start + i;
			let off = (i as usize) * EMMC_BLOCK_BYTES;
			let end = off + EMMC_BLOCK_BYTES;
			let ret = client.write_single(CMD_EMMC_WRITE, lba, &buf[off..end])?;
			if ret != 0 {
				bail!("write failed at lba {}: {ret}", lba);
			}
			i += 1;
			if (i & 0x3FF) == 0 || i == blocks {
				eprintln!("written {}/{} blocks", i, blocks);
			}
		}
	}

	let _ = client.cmd_u32(CMD_START_SMC, 0);

	Ok(())
}
