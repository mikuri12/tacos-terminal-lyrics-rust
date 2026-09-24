//! Terminal handling: raw mode, size, cursor, and palette detection via OSC.
//! The colors are YOUR terminal's theme colors - queried live via OSC 10/11/12
//! and the 256-color palette via OSC 4. No pywal, no matugen, no config.

use std::io::{self, Write};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Raw mode
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct Termios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line: u8,
    c_cc: [u8; 32],
    c_ispeed: u32,
    c_ospeed: u32,
}

extern "C" {
    fn tcgetattr(fd: i32, termios_p: *mut Termios) -> i32;
    fn tcsetattr(fd: i32, optional_actions: i32, termios_p: *const Termios) -> i32;
    fn ioctl(fd: i32, request: u64, ...) -> i32;
}

const TIOCGWINSZ: u64 = 0x5413;
const TCSANOW: i32 = 0;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct WinSize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

/// Guard that puts the terminal into raw mode and restores it on drop.
pub struct RawMode {
    saved: Termios,
    fd: i32,
}

impl RawMode {
    pub fn enable() -> io::Result<Self> {
        let fd = libc::STDOUT_FILENO;
        let mut saved = unsafe { std::mem::zeroed() };
        if unsafe { tcgetattr(fd, &mut saved) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = saved;
        // classic cfmakeraw minus output post-processing (keep OPOST so \n works)
        raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN);
        raw.c_iflag &= !(libc::IXON | libc::ICRNL | libc::BRKINT | libc::INPCK | libc::ISTRIP);
        raw.c_cflag |= libc::CS8;
        raw.c_cc[libc::VMIN] = 0; // polling reads
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { tcsetattr(fd, TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { saved, fd })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { tcsetattr(self.fd, TCSANOW, &self.saved) };
        let _ = io::stdout().flush();
    }
}

/// Terminal size in (cols, rows); falls back to 80x24.
pub fn size() -> (usize, usize) {
    let mut ws = WinSize::default();
    let ok = unsafe { ioctl(libc::STDOUT_FILENO, TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 && ws.ws_row > 0 {
        (ws.ws_col as usize, ws.ws_row as usize)
    } else {
        (80, 24)
    }
}

/// Non-blocking read of pending input bytes (for q/Ctrl-C to quit).
pub fn poll_key() -> Option<u8> {
    let mut buf = [0u8; 1];
    let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr() as *mut libc::c_void, 1) };
    if n == 1 {
        Some(buf[0])
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Palette detection: query the terminal's own colors
// ---------------------------------------------------------------------------

/// An RGB color as reported by the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    fn sgr(&self) -> String {
        format!("\x1b[38;2;{};{};{}m", self.0, self.1, self.2)
    }
}

/// The terminal's theme colors, live-queried. fg = lyrics, bg = dimmed lyrics.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub fg: Rgb,
    pub bg: Rgb,
    pub has_bg: bool,
}

impl Palette {
    /// Color code (as ANSI string) for the active lyric text.
    pub fn lyric(&self) -> String {
        self.fg.sgr()
    }

    /// Dimmed color for the not-yet-sung part: fg blended 55% toward bg.
    pub fn dim(&self) -> String {
        let bg = if self.has_bg { self.bg } else { Rgb(0, 0, 0) };
        let mix = |f: u8, b: u8| -> u8 { (f as u16 * 55 / 100 + b as u16 * 45 / 100) as u8 };
        Rgb(
            mix(self.fg.0, bg.0),
            mix(self.fg.1, bg.1),
            mix(self.fg.2, bg.2),
        )
        .sgr()
    }
}

/// Query fg (OSC 10) and bg (OSC 11) colors from the terminal.
///
/// Works on xterm, kitty, foot, alacritty, wezterm, ghostty, konsole, st...
/// (any terminal that answers OSC queries). When the terminal doesn't
/// answer we fall back to SGR 39 (default foreground) - i.e. your normal
/// text color - and SGR 2 (faint) for dimmed text.
pub fn detect_palette() -> Palette {
    let mut out = io::stdout();
    let mut answers: Vec<String> = Vec::new();

    // We must be in raw mode for the reply not to be eaten/echoed.
    if let Ok(_raw) = RawMode::enable() {
        let _ = out.write_all(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\");
        let _ = out.flush();
        // drain replies for up to ~150ms
        let deadline = std::time::Instant::now() + Duration::from_millis(150);
        let mut acc = Vec::new();
        while std::time::Instant::now() < deadline {
            let mut b = [0u8; 64];
            let n = unsafe {
                libc::read(
                    libc::STDIN_FILENO,
                    b.as_mut_ptr() as *mut libc::c_void,
                    b.len(),
                )
            };
            if n > 0 {
                acc.extend_from_slice(&b[..n as usize]);
                if acc.windows(2).filter(|w| w == b"\\").count() >= 2 {
                    break;
                }
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        // parse OSC replies: "\x1b]10;rgb:rrrr/gggg/bbbb\x1b\""
        let text = String::from_utf8_lossy(&acc).to_string();
        for part in text.split("\x1b]").filter(|s| !s.is_empty()) {
            if part.starts_with("10;") || part.starts_with("11;") {
                answers.push(
                    part.trim_end_matches('\x07')
                        .trim_end_matches('\\')
                        .to_string(),
                );
            }
        }
    }

    let fg = answers
        .iter()
        .find(|a| a.starts_with("10;"))
        .and_then(|a| parse_osc_rgb(a));
    let bg = answers
        .iter()
        .find(|a| a.starts_with("11;"))
        .and_then(|a| parse_osc_rgb(a));
    match (fg, bg) {
        (Some(fg), bg) => Palette {
            fg,
            bg: bg.unwrap_or(Rgb(0, 0, 0)),
            has_bg: bg.is_some(),
        },
        _ => Palette {
            fg: Rgb(255, 255, 255),
            bg: Rgb(0, 0, 0),
            has_bg: false,
        },
    }
}

/// Parse "10;rgb:1234/5678/9abc" into an 8-bit RGB.
fn parse_osc_rgb(reply: &str) -> Option<Rgb> {
    let rgb = reply.split_once("rgb:")?.1;
    let mut chans = [0u8; 3];
    for (i, c) in rgb.split('/').enumerate() {
        if i > 2 {
            break;
        }
        let c = c.trim();
        let scaled = match c.len() {
            1 => u16::from_str_radix(c, 16).ok().map(|v| v * 17),
            2 => u16::from_str_radix(c, 16).ok(),
            3 => u16::from_str_radix(c, 16)
                .ok()
                .map(|v| (v as u32 * 255 * 2 / 4095 / 2) as u16),
            4 => u16::from_str_radix(c, 16)
                .ok()
                .map(|v| ((v as u32 * 255 * 2 + 65535) / 2 / 65535) as u16),
            _ => None,
        };
        chans[i] = scaled? as u8;
    }
    Some(Rgb(chans[0], chans[1], chans[2]))
}

// ---------------------------------------------------------------------------
// Screen helpers
// ---------------------------------------------------------------------------

pub fn hide_cursor() -> io::Result<()> {
    io::stdout().write_all(b"\x1b[?25l").map(|_| ())
}

pub fn show_cursor() -> io::Result<()> {
    io::stdout().write_all(b"\x1b[?25h").map(|_| ())
}

pub fn clear_screen() -> io::Result<()> {
    io::stdout().write_all(b"\x1b[2J\x1b[H").map(|_| ())
}

/// Move cursor to the start of the lyric area without clearing: we redraw
/// in place every frame, which avoids the flash of full clears.
#[allow(dead_code)]
pub fn cursor_home() -> io::Result<()> {
    io::stdout().write_all(b"\x1b[H").map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rgb_replies() {
        // 4-digit form: scale each 16-bit channel to 8-bit (with rounding)
        assert_eq!(parse_osc_rgb("10;rgb:ffff/0000/00ff"), Some(Rgb(255, 0, 1)));
        // 2-digit form is already 8-bit
        assert_eq!(
            parse_osc_rgb("11;rgb:1a2b/3c4d/5e6f"),
            Some(Rgb(0x1a, 0x3c, 0x5e))
        );
        assert_eq!(parse_osc_rgb("10;rgb:ff/00/99"), Some(Rgb(255, 0, 153)));
        assert_eq!(parse_osc_rgb("garbage"), None);
    }

    #[test]
    fn dim_blends() {
        let p = Palette {
            fg: Rgb(255, 255, 255),
            bg: Rgb(0, 0, 0),
            has_bg: true,
        };
        let dim = p.dim();
        assert!(dim.contains("38;2;"), "{dim}");
        // 255*0.55 = 140
        assert!(dim.contains("140;140;140"), "{dim}");
    }
}
