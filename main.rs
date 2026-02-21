use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand,};
use colored::*;
use dialoguer::{Confirm, Select, theme::ColorfulTheme};
use indicatif::{ProgressBar, ProgressStyle};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

// ─────────────────────────────────────────────
//  CLI
// ─────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "burn",
    about = "🔥 BurnEngine USB — Real Linux ISO → USB Writer",
    long_about = r#"
╔══════════════════════════════════════════════════════════╗
║         🔥  B U R N E N G I N E  U S B  v3.0  🔥        ║
║         Real · Safe · Linux ISO to USB Writer            ║
╚══════════════════════════════════════════════════════════╝

Writes Linux ISO images directly to USB drives using dd.
Only detects REMOVABLE drives — never touches internal disks."#,
    version,
    propagate_version = true
)]
struct Cli {
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Dry-run: show what would happen without writing
    #[arg(short = 'n', long, global = true)]
    dry_run: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 🔥 Write ISO to USB drive
    Write {
        /// Path to ISO file
        #[arg(short, long)]
        input: Option<PathBuf>,

        /// Target USB device (e.g. /dev/sdb) — auto-detected if omitted
        #[arg(short, long)]
        device: Option<String>,

        /// Verify MD5 checksum after write
        #[arg(long)]
        verify: bool,
    },

    /// 📋 List removable USB drives only
    List,

    /// 🎛️  Interactive wizard
    Wizard,

    /// 📊 Show device info
    Info {
        #[arg(short, long)]
        device: Option<String>,
    },
}

// ─────────────────────────────────────────────
//  USB DEVICE
// ─────────────────────────────────────────────

#[derive(Debug, Clone)]
struct UsbDevice {
    /// e.g. "sdb"
    name: String,
    /// e.g. "/dev/sdb"
    path: String,
    /// Size in bytes
    size: u64,
    /// Vendor/Model from sysfs
    model: String,
    /// Is it actually removable?
    removable: bool,
    /// Transport: usb / ata / nvme etc.
    transport: String,
}

impl UsbDevice {
    fn size_human(&self) -> String {
        let gb = self.size as f64 / 1_000_000_000.0;
        if gb >= 1.0 {
            format!("{:.1} GB", gb)
        } else {
            format!("{:.0} MB", self.size as f64 / 1_000_000.0)
        }
    }

    fn label(&self) -> String {
        format!(
            "{}  {}  {}  [{}]",
            self.path.bright_cyan().bold(),
            self.size_human().bright_white(),
            self.model.bright_yellow(),
            self.transport.dimmed()
        )
    }
}

// ─────────────────────────────────────────────
//  DETECT USB DRIVES (SAFE)
// ─────────────────────────────────────────────

/// Read a sysfs file as trimmed string
fn sysfs_read(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Detect ONLY removable USB block devices (whole disks, not partitions)
fn detect_usb_drives() -> Vec<UsbDevice> {
    let mut devices = Vec::new();

    let block_dir = match fs::read_dir("/sys/block") {
        Ok(d) => d,
        Err(_) => return devices,
    };

    for entry in block_dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();

        // Skip loop devices, ram, zram, dm, md
        if name.starts_with("loop")
            || name.starts_with("ram")
            || name.starts_with("zram")
            || name.starts_with("dm-")
            || name.starts_with("md")
            || name.starts_with("sr")   // optical
        {
            continue;
        }

        let sys_path = format!("/sys/block/{}", name);

        // ── SAFETY CHECK 1: Must be removable ──
        let removable = sysfs_read(&format!("{}/removable", sys_path))
            .map(|s| s == "1")
            .unwrap_or(false);

        if !removable {
            continue; // skip internal disks!
        }

        // ── SAFETY CHECK 2: Transport must be usb ──
        // Follow the symlink chain to find the transport
        let transport = detect_transport(&sys_path);
        if transport != "usb" {
            continue; // skip eSATA, SD cards via wrong path, etc.
        }

        // ── SAFETY CHECK 3: Must have a /dev node ──
        let dev_path = format!("/dev/{}", name);
        if !std::path::Path::new(&dev_path).exists() {
            continue;
        }

        // Size in bytes (size file gives 512-byte sectors)
        let size_sectors: u64 = sysfs_read(&format!("{}/size", sys_path))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let size = size_sectors * 512;

        // Skip empty / tiny devices
        if size < 100_000_000 {
            continue;
        }

        // Model from sysfs
        let model = sysfs_read(&format!("{}/device/model", sys_path))
            .or_else(|| sysfs_read(&format!("{}/device/../product", sys_path)))
            .unwrap_or_else(|| "USB Drive".to_string());

        devices.push(UsbDevice {
            name: name.clone(),
            path: dev_path,
            size,
            model,
            removable,
            transport,
        });
    }

    devices
}

/// Walk sysfs to find transport type (usb / ata / nvme / mmc)
fn detect_transport(sys_path: &str) -> String {
    // Resolve the real path via /sys/block/sdX → device symlink
    let device_link = format!("{}/device", sys_path);
    if let Ok(real) = fs::canonicalize(&device_link) {
        let real_str = real.to_string_lossy().to_string();
        if real_str.contains("/usb") {
            return "usb".to_string();
        }
        if real_str.contains("nvme") {
            return "nvme".to_string();
        }
        if real_str.contains("mmc") {
            return "mmc".to_string();
        }
        if real_str.contains("ata") {
            return "ata".to_string();
        }
    }
    "unknown".to_string()
}

// ─────────────────────────────────────────────
//  UI HELPERS
// ─────────────────────────────────────────────

fn print_banner() {
    println!("{}", "╔══════════════════════════════════════════════════════════╗".bright_cyan());
    println!("{} {} {}",
        "║".bright_cyan(),
        "     🔥  B U R N E N G I N E  U S B  v3.0  🔥          ".bright_yellow().bold(),
        "║".bright_cyan()
    );
    println!("{} {} {}",
        "║".bright_cyan(),
        "       Real · Safe · Linux ISO to USB Writer          ".bright_white(),
        "  ║".bright_cyan()
    );
    println!("{}", "╚══════════════════════════════════════════════════════════╝".bright_cyan());
    println!();
}

fn info(msg: &str)    { println!("{} {}", "ℹ️ ".blue(),   msg.bright_white()); }
fn success(msg: &str) { println!("{} {}", "✅".green(),   msg.bright_green().bold()); }
fn warn(msg: &str)    { println!("{} {}", "⚠️ ".yellow(), msg.yellow()); }
fn err_msg(msg: &str) { println!("{} {}", "❌".red(),     msg.bright_red().bold()); }
fn step(n: u8, t: u8, msg: &str) {
    println!("{} {}",
        format!("[{}/{}]", n, t).bright_cyan().bold(),
        msg.white()
    );
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.cyan} {msg}")
        .unwrap()
        .tick_strings(&["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"])
}

fn write_bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.red} [{bar:50.red/dim}] {percent}%  ⚡ {bytes_per_sec}  🕐 ETA {eta}  {msg}"
    )
    .unwrap()
    .tick_strings(&["🔥","💥","🔥","💥"])
    .progress_chars("█▉▊▋▌▍▎▏ ")
}

fn verify_bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.green} [{bar:50.green/dim}] {percent}%  🔍 {bytes_per_sec}  🕐 ETA {eta}  {msg}"
    )
    .unwrap()
    .tick_strings(&["🔍","🔎","✅","🔍"])
    .progress_chars("█▉▊▋▌▍▎▏ ")
}

// ─────────────────────────────────────────────
//  SELECT DRIVE
// ─────────────────────────────────────────────

fn select_usb_device() -> Result<UsbDevice> {
    let devices = detect_usb_drives();
    if devices.is_empty() {
        return Err(anyhow!(
            "No USB drives detected!\n\
             • Make sure the USB is plugged in\n\
             • Try: lsblk -d -o NAME,TRAN,RM,SIZE,MODEL"
        ));
    }

    let theme = ColorfulTheme::default();
    let labels: Vec<String> = devices.iter().map(|d| {
        format!("{}  {}  {}",
            d.path.bright_cyan().bold(),
            d.size_human().bright_white(),
            d.model.yellow()
        )
    }).collect();

    // Plain strings for dialoguer
    let plain_labels: Vec<String> = devices.iter().map(|d| {
        format!("{}  {}  {}", d.path, d.size_human(), d.model)
    }).collect();

    let idx = Select::with_theme(&theme)
        .with_prompt("🔌 Select USB drive")
        .items(&plain_labels)
        .default(0)
        .interact()?;

    Ok(devices[idx].clone())
}

fn pick_file() -> Result<PathBuf> {
    let theme = ColorfulTheme::default();

    if std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok() {
        let use_gui = Confirm::with_theme(&theme)
            .with_prompt("📂 Open graphical file picker?")
            .default(true)
            .interact()?;
        if use_gui {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("ISO Images", &["iso"])
                .add_filter("All Files", &["*"])
                .set_title("Select Linux ISO")
                .pick_file()
            {
                return Ok(path);
            }
        }
    }

    let s: String = dialoguer::Input::with_theme(&theme)
        .with_prompt("📁 Path to ISO file")
        .interact_text()?;
    let p = PathBuf::from(s.trim());
    if !p.exists() {
        return Err(anyhow!("File not found: {}", p.display()));
    }
    Ok(p)
}

fn iso_size(path: &PathBuf) -> Result<u64> {
    Ok(fs::metadata(path)
        .with_context(|| format!("Cannot read ISO: {}", path.display()))?
        .len())
}

// ─────────────────────────────────────────────
//  SAFETY CONFIRMATION
// ─────────────────────────────────────────────

fn safety_confirm(iso: &PathBuf, device: &UsbDevice) -> Result<bool> {
    let iso_bytes = iso_size(iso)?;
    let theme = ColorfulTheme::default();

    println!();
    println!("{}", "┌─────────────────────────────────────────────────────┐".bright_red());
    println!("{} {} {}",
        "│".bright_red(),
        "           ⚠️   WARNING — DATA WILL BE LOST!  ⚠️           ".bright_red().bold(),
        "│".bright_red()
    );
    println!("{}", "├─────────────────────────────────────────────────────┤".bright_red());
    println!("{}  {:20} {}  {}",
        "│".bright_red(),
        "ISO:".bright_white(),
        iso.file_name().unwrap_or_default().to_string_lossy().bright_yellow(),
        "│".bright_red()
    );
    println!("{}  {:20} {}  {}",
        "│".bright_red(),
        "ISO size:".bright_white(),
        format!("{:.1} GB", iso_bytes as f64 / 1e9).bright_yellow(),
        "│".bright_red()
    );
    println!("{}  {:20} {}  {}",
        "│".bright_red(),
        "Target device:".bright_white(),
        device.path.bright_red().bold(),
        "│".bright_red()
    );
    println!("{}  {:20} {}  {}",
        "│".bright_red(),
        "Device model:".bright_white(),
        device.model.bright_yellow(),
        "│".bright_red()
    );
    println!("{}  {:20} {}  {}",
        "│".bright_red(),
        "Device size:".bright_white(),
        device.size_human().bright_yellow(),
        "│".bright_red()
    );
    println!("{}", "│                                                     │".bright_red());
    println!("{} {} {}",
        "│".bright_red(),
        "  ALL DATA ON THIS USB WILL BE PERMANENTLY ERASED!   ".bright_red().bold(),
        "│".bright_red()
    );
    println!("{}", "└─────────────────────────────────────────────────────┘".bright_red());
    println!();

    // Check ISO fits on device
    if iso_bytes > device.size {
        err_msg(&format!(
            "ISO ({:.1} GB) is LARGER than the USB ({})!",
            iso_bytes as f64 / 1e9,
            device.size_human()
        ));
        return Ok(false);
    }

    // Double confirmation
    let first = Confirm::with_theme(&theme)
        .with_prompt(format!("Write to {}? ({})", device.path, device.model))
        .default(false)
        .interact()?;

    if !first {
        warn("Cancelled.");
        return Ok(false);
    }

    let second = Confirm::with_theme(&theme)
        .with_prompt("⚠️  FINAL WARNING — Are you absolutely sure? This CANNOT be undone!")
        .default(false)
        .interact()?;

    Ok(second)
}

// ─────────────────────────────────────────────
//  UNMOUNT PARTITIONS
// ─────────────────────────────────────────────

fn unmount_device(device: &UsbDevice) {
    info(&format!("Unmounting all partitions on {}…", device.path));

    // Find all partitions (sdb1, sdb2, ...)
    if let Ok(entries) = fs::read_dir("/sys/block") {
        for entry in entries.flatten() {
            let bname = entry.file_name().to_string_lossy().to_string();
            if bname == device.name { continue; }
        }
    }

    // Try to unmount via /proc/mounts
    if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
        for line in mounts.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let mount_dev = parts[0];
                let mount_point = parts[1];
                if mount_dev.starts_with(&device.path) {
                    info(&format!("  Unmounting {}…", mount_point));
                    let _ = Command::new("umount")
                        .arg(mount_point)
                        .status();
                }
            }
        }
    }
}

// ─────────────────────────────────────────────
//  WRITE — real dd
// ─────────────────────────────────────────────

fn do_write(
    input: &PathBuf,
    device: &UsbDevice,
    verify: bool,
    dry_run: bool,
    verbose: bool,
    running: Arc<AtomicBool>,
) -> Result<()> {
    let iso_bytes = iso_size(input)?;
    let total_steps: u8 = if verify { 3 } else { 2 };

    println!();
    step(1, total_steps, "Preparing…");
    info(&format!("ISO  : {}  ({:.1} GB)",
        input.display().to_string().bright_yellow(),
        iso_bytes as f64 / 1e9
    ));
    info(&format!("USB  : {}  {}  {}",
        device.path.bright_cyan(),
        device.size_human().bright_white(),
        device.model.yellow()
    ));
    if dry_run { warn("DRY-RUN — nothing will be written"); }
    println!();

    // ── Unmount ───────────────────────────────
    unmount_device(device);
    println!();

    if dry_run {
        success("DRY-RUN complete — would run:");
        info(&format!(
            "dd if={} of={} bs=4M status=progress oflag=sync",
            input.display(), device.path
        ));
        return Ok(());
    }

    // ── Write with dd ─────────────────────────
    step(2, total_steps, "Writing ISO to USB…");

    let pb = ProgressBar::new(iso_bytes);
    pb.set_style(write_bar_style());
    pb.enable_steady_tick(Duration::from_millis(120));
    pb.set_message("Starting dd…");

    if verbose {
        info(&format!(
            "Running: dd if={} of={} bs=4M status=progress oflag=sync",
            input.display(), device.path
        ));
    }

    // dd writes progress to stderr with status=progress
    let mut child = Command::new("dd")
        .args([
            format!("if={}", input.display()),
            format!("of={}", device.path),
            "bs=4M".into(),
            "status=progress".into(),
            //"oflag=sync".into(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to launch dd — is it installed?")?;

    let stderr = child.stderr.take().unwrap();
    let pb2 = pb.clone();
    let run2 = running.clone();

    // dd with status=progress writes to stderr lines like:
    // "1234567168 bytes (1.2 GB, 1.1 GiB) copied, 5.1 s, 242 MB/s"
    let parse_thread = thread::spawn(move || {
        // dd status=progress uses \r not \n — read byte by byte
        use std::io::Read;
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();

        loop {
            if !run2.load(Ordering::SeqCst) { break; }
            let mut byte = [0u8; 1];
            match reader.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let ch = byte[0] as char;
                    if ch == '\r' || ch == '\n' {
                        let trimmed = line.trim().to_string();
                        if trimmed.contains("bytes") && trimmed.contains("copied") {
                            if let Some(b) = parse_dd_bytes(&trimmed) {
                                pb2.set_position(b);
                                pb2.set_message(format!("{:.1} GB written", b as f64 / 1e9));
                            }
                        }
                        line.clear();
                    } else {
                        line.push(ch);
                    }
                }
            }
        }
    });

    let status = child.wait().context("dd process error")?;
    parse_thread.join().ok();

    if !status.success() {
        pb.abandon_with_message("❌ dd failed".red().to_string());
        println!();
        return Err(anyhow!(
            "dd failed (exit code {}).\n\
             \nTroubleshooting:\n\
             • Run with sudo or as root\n\
             • Make sure USB is properly connected\n\
             • Try: sudo burn write -i ubuntu.iso",
            status.code().unwrap_or(-1)
        ));
    }

    pb.set_position(iso_bytes);
    pb.finish_with_message(format!("{}", "🔥 Write complete!".red().bold()));
    println!();

    // ── Sync ──────────────────────────────────
    let sp = ProgressBar::new_spinner();
    sp.set_style(spinner_style());
    sp.set_message("Flushing buffers to USB (sync)…");
    sp.enable_steady_tick(Duration::from_millis(80));
    let _ = Command::new("sync").status();
    sp.finish_with_message(format!("{}", "✅ Sync complete".green()));
    println!();

    // ── Verify ────────────────────────────────
    if verify {
        do_verify(input, device, running.clone())?;
    }

    println!();
    println!("{}", "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━".bright_cyan());
    println!("{}", "  🎉  ALL DONE — USB is ready to boot!               ".bright_green().bold());
    println!("{}", "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━".bright_cyan());

    if verbose {
        println!();
        println!("{}", "📊 Summary:".bright_white().bold());
        println!("  ISO    : {}", input.display().to_string().bright_yellow());
        println!("  Device : {}  {}", device.path.bright_cyan(), device.model.dimmed());
        println!("  Written: {}", format!("{:.1} GB", iso_bytes as f64 / 1e9).bright_green());
        println!("  Verify : {}", if verify { "✅ PASSED".green().to_string() } else { "skipped".dimmed().to_string() });
    }

    Ok(())
}

/// Parse "1234567168 bytes (1.2 GB...) copied" → 1234567168
fn parse_dd_bytes(line: &str) -> Option<u64> {
    line.split_whitespace()
        .next()
        .and_then(|s| s.replace(',', "").parse::<u64>().ok())
}

// ─────────────────────────────────────────────
//  VERIFY — md5sum ISO vs USB
// ─────────────────────────────────────────────

fn do_verify(input: &PathBuf, device: &UsbDevice, running: Arc<AtomicBool>) -> Result<()> {
    println!();
    step(3, 3, &format!(
        "Verifying {}  vs  {}",
        input.file_name().unwrap_or_default().to_string_lossy().bright_yellow(),
        device.path.bright_cyan()
    ));

    let iso_bytes = iso_size(input)?;
    let sectors = (iso_bytes + 511) / 512;

    // ── MD5 of ISO ────────────────────────────
    let sp = ProgressBar::new_spinner();
    sp.set_style(spinner_style());
    sp.set_message("Computing ISO MD5…");
    sp.enable_steady_tick(Duration::from_millis(80));
    let iso_md5 = md5sum_file(input)?;
    sp.finish_with_message(format!("ISO MD5: {}", iso_md5.bright_yellow()));
    println!();

    // ── MD5 of USB (read exact ISO size) ──────
    info("Reading back from USB…");
    let pb = ProgressBar::new(iso_bytes);
    pb.set_style(verify_bar_style());
    pb.enable_steady_tick(Duration::from_millis(120));
    pb.set_message("Reading…");

    // dd if=/dev/sdb bs=512 count=<sectors> | md5sum
    let mut dd = Command::new("dd")
        .args([
            format!("if={}", device.path),
            "bs=512".into(),
            format!("count={}", sectors),
            "status=progress".into(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to run dd for verify")?;

    let dd_stdout = dd.stdout.take().unwrap();
    let mut md5proc = Command::new("md5sum")
        .stdin(dd_stdout)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("Failed to run md5sum")?;

    // Parse dd stderr for progress
    let dd_stderr = dd.stderr.take().unwrap();
    let pb2 = pb.clone();
    let run2 = running.clone();
    thread::spawn(move || {
        use std::io::Read;
        let mut reader = BufReader::new(dd_stderr);
        let mut line = String::new();
        loop {
            if !run2.load(Ordering::SeqCst) { break; }
            let mut byte = [0u8; 1];
            match reader.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let ch = byte[0] as char;
                    if ch == '\r' || ch == '\n' {
                        let t = line.trim().to_string();
                        if t.contains("bytes") && t.contains("copied") {
                            if let Some(b) = parse_dd_bytes(&t) {
                                pb2.set_position(b.min(iso_bytes));
                            }
                        }
                        line.clear();
                    } else {
                        line.push(ch);
                    }
                }
            }
        }
    });

    dd.wait().context("dd verify failed")?;
    let md5out = md5proc.wait_with_output()?;
    let usb_md5 = String::from_utf8_lossy(&md5out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("error")
        .to_string();

    pb.set_position(iso_bytes);
    pb.finish_with_message("Done");

    println!();

    println!("  🔐 ISO  MD5 : {}", iso_md5.bright_yellow());
    println!("  🔐 USB  MD5 : {}", usb_md5.bright_cyan());
    println!();

    if iso_md5 == usb_md5 {
        success("✅ Verification PASSED — USB is a perfect copy of the ISO!");
        Ok(())
    } else {
        err_msg("❌ Verification FAILED — checksums do NOT match!");
        Err(anyhow!("MD5 mismatch — write may have failed or USB is faulty"))
    }
}

fn md5sum_file(path: &PathBuf) -> Result<String> {
    let out = Command::new("md5sum")
        .arg(path)
        .output()
        .context("md5sum not found")?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string())
}

// ─────────────────────────────────────────────
//  LIST
// ─────────────────────────────────────────────

fn do_list() {
    println!();
    println!("{}", "📋 Removable USB drives:".bright_white().bold());
    println!("{}", "──────────────────────────────────────────────────────".dimmed());

    let devices = detect_usb_drives();
    if devices.is_empty() {
        warn("No USB drives detected.");
        info("Plug in a USB drive and try again.");
        info("Debug: lsblk -d -o NAME,TRAN,RM,SIZE,MODEL");
        return;
    }

    for d in &devices {
        println!("  🟢  {}  {}  {}  [transport: {}]",
            d.path.bright_cyan().bold(),
            d.size_human().bright_white(),
            d.model.bright_yellow(),
            d.transport.dimmed()
        );
    }
    println!();
    warn("⚠️  Writing to any of these will ERASE all data on it!");
    println!();
}

// ─────────────────────────────────────────────
//  INFO
// ─────────────────────────────────────────────

fn do_info(device: &UsbDevice) {
    println!();
    println!("{}", format!("📊 Device Info — {}", device.path).bright_white().bold());
    println!("{}", "──────────────────────────────────────────────────────".dimmed());

    let fields = vec![
        ("🔌 Device",     device.path.clone()),
        ("📦 Model",      device.model.clone()),
        ("💾 Size",       device.size_human()),
        ("🔄 Removable",  device.removable.to_string()),
        ("🚌 Transport",  device.transport.clone()),
    ];

    for (label, value) in &fields {
        println!("  {:20} {}", label.bright_cyan(), value.bright_white());
    }

    // lsblk for partitions
    println!();
    println!("{}", "  Partitions:".bright_white().bold());
    let _ = Command::new("lsblk")
        .args(["-o", "NAME,SIZE,FSTYPE,LABEL,MOUNTPOINT", &device.path])
        .status();
    println!();
}

// ─────────────────────────────────────────────
//  WIZARD
// ─────────────────────────────────────────────

fn do_wizard(dry_run: bool, verbose: bool, running: Arc<AtomicBool>) -> Result<()> {
    let theme = ColorfulTheme::default();

    println!();
    println!("{}", "🎛️  BurnEngine USB — Interactive Wizard".bright_cyan().bold());
    println!("{}", "──────────────────────────────────────────".dimmed());
    println!();

    let ops = vec![
        "🔥  Write Linux ISO to USB",
        "🔍  Verify USB against ISO",
        "📋  List USB drives",
        "📊  Show device info",
    ];

    let op = Select::with_theme(&theme)
        .with_prompt("What do you want to do?")
        .items(&ops)
        .default(0)
        .interact()?;

    match op {
        0 => {
            let input = pick_file()?;
            let device = select_usb_device()?;

            if !safety_confirm(&input, &device)? {
                return Ok(());
            }

            let extra = vec!["✅ Verify MD5 after write"];
            let selected = dialoguer::MultiSelect::with_theme(&theme)
                .with_prompt("⚙️  Options")
                .items(&extra)
                .defaults(&[true])
                .interact()?;
            let verify = selected.contains(&0);

            println!();
            do_write(&input, &device, verify, dry_run, verbose, running)?;
        }
        1 => {
            let input = pick_file()?;
            let device = select_usb_device()?;
            do_verify(&input, &device, running)?;
        }
        2 => do_list(),
        3 => {
            let device = select_usb_device()?;
            do_info(&device);
        }
        _ => {}
    }

    Ok(())
}

// ─────────────────────────────────────────────
//  CTRL-C
// ─────────────────────────────────────────────

fn setup_ctrlc(running: Arc<AtomicBool>) {
    ctrlc::set_handler(move || {
        println!("\n\n{} {}", "⚠️ ".yellow(), "Interrupt! Stopping…".red().bold());
        running.store(false, Ordering::SeqCst);
        std::process::exit(1);
    })
    .expect("Failed to set Ctrl-C handler");
}

// ─────────────────────────────────────────────
//  MAIN
// ─────────────────────────────────────────────

fn main() -> Result<()> {
    let cli = Cli::parse();
    let running = Arc::new(AtomicBool::new(true));
    setup_ctrlc(running.clone());

    print_banner();

    if cli.dry_run {
        warn("DRY-RUN mode — nothing will be written.");
        println!();
    }

    match cli.command {
        Commands::Write { input, device, verify } => {
            let input = match input {
                Some(p) => {
                    if !p.exists() { return Err(anyhow!("ISO not found: {}", p.display())); }
                    p
                }
                None => pick_file()?,
            };

            let device = match device {
                Some(d) => {
                    // Validate manually specified device
                    let devices = detect_usb_drives();
                    devices.into_iter().find(|dev| dev.path == d)
                        .ok_or_else(|| anyhow!(
                            "'{}' is not a detected USB drive.\n\
                             Use 'burn list' to see available USB devices.",
                            d
                        ))?
                }
                None => select_usb_device()?,
            };

            if !safety_confirm(&input, &device)? {
                return Ok(());
            }

            do_write(&input, &device, verify, cli.dry_run, cli.verbose, running)?;
        }

        Commands::List => do_list(),

        Commands::Info { device } => {
            let device = match device {
                Some(d) => {
                    let devices = detect_usb_drives();
                    devices.into_iter().find(|dev| dev.path == d)
                        .ok_or_else(|| anyhow!("'{}' not found as USB device", d))?
                }
                None => select_usb_device()?,
            };
            do_info(&device);
        }

        Commands::Wizard => {
            do_wizard(cli.dry_run, cli.verbose, running)?;
        }
    }

    Ok(())
}
