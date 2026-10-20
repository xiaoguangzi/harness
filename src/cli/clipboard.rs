//! Reading the system clipboard for Ctrl+V in the REPL: an image, or the
//! paths of files copied in a file manager (whose clipboard entry also
//! carries their icons, which are not what was meant). macOS asks AppKit
//! through `osascript`; elsewhere `wl-paste` (Wayland) or `xclip` (X11) is
//! used. `/copy` writes text to it the same ways, or through the terminal.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::mentions::human_size;

/// Largest image sent: base64 makes it 5 MB, the API's limit.
const MAX_IMAGE_BYTES: usize = 3_932_160;
/// Longer edges are scaled down to this on macOS; the API would scale them
/// down anyway.
const MAX_EDGE: u32 = 2576;
/// How long the clipboard tool may take; an app that promised clipboard
/// data can stall it.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Image types the API takes, in order of preference.
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

pub struct Image {
    pub media_type: String,
    pub data: Vec<u8>,
}

pub enum Clip {
    Image(Image),
    Files(Vec<PathBuf>),
}

/// Puts `text` on the clipboard: with `pbcopy` on macOS, `wl-copy`, `xclip`
/// or `xsel` elsewhere, else by asking the terminal (OSC 52), which works
/// over SSH too where the terminal allows it. Returns how.
pub fn copy_text(text: &str) -> Result<&'static str, String> {
    use std::io::Write;
    let tools: &[(&'static str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"])]
    };
    for (program, args) in tools {
        let Ok(mut child) = Command::new(program).args(*args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() else {
            continue;
        };
        let written = child.stdin.take().is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
        if written && child.wait().is_ok_and(|status| status.success()) {
            return Ok(program);
        }
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return Err("no clipboard tool found (pbcopy, wl-copy, xclip or xsel)".into());
    }
    let mut stdout = std::io::stdout().lock();
    write!(stdout, "\x1b]52;c;{}\x07", BASE64.encode(text)).and_then(|()| stdout.flush()).map_err(|e| e.to_string())?;
    Ok("the terminal")
}

/// What the clipboard holds: `None` when it has neither an image nor files,
/// an error when it can't be read or its image is too large.
pub fn read() -> Result<Option<Clip>, String> {
    let clip = if cfg!(target_os = "macos") { read_macos()? } else { read_x11_or_wayland()? };
    if let Some(Clip::Image(image)) = &clip
        && image.data.len() > MAX_IMAGE_BYTES
    {
        let (size, max) = (human_size(image.data.len() as u64), human_size(MAX_IMAGE_BYTES as u64));
        return Err(format!("the image in the clipboard is {size}, over the {max} limit"));
    }
    Ok(clip)
}

/// JXA: prints `files` and one path per line, or an image's media type and
/// its base64, or nothing. An image is scaled down to `MAX_EDGE`, and
/// becomes a JPEG if it is still too large as a PNG.
const MACOS_SCRIPT: &str = r#"
ObjC.import('AppKit');
function run() {
  const pb = $.NSPasteboard.generalPasteboard;
  const urls = pb.readObjectsForClassesOptions($([$.NSURL]),
    $.NSDictionary.dictionaryWithObjectForKey(true, $.NSPasteboardURLReadingFileURLsOnlyKey));
  if (!urls.isNil() && urls.count > 0) {
    const paths = [];
    for (let i = 0; i < urls.count; i++) paths.push(urls.objectAtIndex(i).path.js);
    return 'files\n' + paths.join('\n');
  }
  let png = pb.dataForType('public.png');
  let rep;
  if (png.isNil()) {
    const image = $.NSImage.alloc.initWithPasteboard(pb);
    if (image.isNil()) return '';
    rep = $.NSBitmapImageRep.imageRepWithData(image.TIFFRepresentation);
  } else {
    rep = $.NSBitmapImageRep.imageRepWithData(png);
  }
  if (rep.isNil()) return '';
  const w = rep.pixelsWide, h = rep.pixelsHigh;
  const scale = Math.min(1, MAX_EDGE / Math.max(w, h));
  if (scale < 1) {
    const sw = Math.max(1, Math.round(w * scale)), sh = Math.max(1, Math.round(h * scale));
    const scaled = $.NSBitmapImageRep.alloc
      .initWithBitmapDataPlanesPixelsWidePixelsHighBitsPerSampleSamplesPerPixelHasAlphaIsPlanarColorSpaceNameBytesPerRowBitsPerPixel(
        null, sw, sh, 8, 4, true, false, $.NSDeviceRGBColorSpace, 0, 0);
    $.NSGraphicsContext.saveGraphicsState;
    const ctx = $.NSGraphicsContext.graphicsContextWithBitmapImageRep(scaled);
    ctx.imageInterpolation = $.NSImageInterpolationHigh;
    $.NSGraphicsContext.setCurrentContext(ctx);
    rep.drawInRect($.NSMakeRect(0, 0, sw, sh));
    $.NSGraphicsContext.restoreGraphicsState;
    rep = scaled;
    png = $();
  }
  let type = 'image/png';
  let data = png.isNil() ? rep.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $({})) : png;
  if (data.length > MAX_BYTES) {
    type = 'image/jpeg';
    data = rep.representationUsingTypeProperties($.NSBitmapImageFileTypeJPEG,
      $.NSDictionary.dictionaryWithObjectForKey(0.85, $.NSImageCompressionFactor));
  }
  return type + '\n' + data.base64EncodedStringWithOptions(0).js;
}
"#;

fn read_macos() -> Result<Option<Clip>, String> {
    let script = format!("const MAX_EDGE = {MAX_EDGE}, MAX_BYTES = {MAX_IMAGE_BYTES};{MACOS_SCRIPT}");
    let out = run("osascript", &["-l", "JavaScript", "-e", &script])?;
    if !out.status.success() {
        return Err(format!("osascript: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    parse_macos(&String::from_utf8_lossy(&out.stdout))
}

fn parse_macos(out: &str) -> Result<Option<Clip>, String> {
    let (head, body) = out.split_once('\n').unwrap_or((out, ""));
    match head.trim() {
        "" => Ok(None),
        "files" => Ok(Some(Clip::Files(body.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect()))),
        media_type => {
            let data = BASE64.decode(body.trim()).map_err(|e| format!("reading the clipboard image: {e}"))?;
            Ok(Some(Clip::Image(Image { media_type: media_type.to_string(), data })))
        }
    }
}

fn read_x11_or_wayland() -> Result<Option<Clip>, String> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    if !wayland && std::env::var_os("DISPLAY").is_none() {
        return Err("no clipboard to read: neither Wayland nor X11 is running".into());
    }
    // The tools fail when the clipboard has no such content.
    let get = |target: Option<&str>| {
        let (program, args) = clipboard_command(wayland, target);
        run(program, &args).map(|out| out.status.success().then_some(out.stdout))
    };
    let Some(targets) = get(None)? else { return Ok(None) };
    let targets = String::from_utf8_lossy(&targets);
    let targets: Vec<&str> = targets.lines().map(str::trim).collect();
    if targets.contains(&"text/uri-list")
        && let Some(list) = get(Some("text/uri-list"))?
    {
        let paths = file_paths(&String::from_utf8_lossy(&list));
        if !paths.is_empty() {
            return Ok(Some(Clip::Files(paths)));
        }
    }
    let Some(media_type) = IMAGE_TYPES.into_iter().find(|t| targets.contains(t)) else { return Ok(None) };
    Ok(get(Some(media_type))?.map(|data| Clip::Image(Image { media_type: media_type.to_string(), data })))
}

/// The program and arguments that list what the clipboard offers, or with
/// `target`, print it as that type.
fn clipboard_command(wayland: bool, target: Option<&str>) -> (&'static str, Vec<&str>) {
    match (wayland, target) {
        (true, None) => ("wl-paste", vec!["--list-types"]),
        (true, Some(target)) => ("wl-paste", vec!["--no-newline", "--type", target]),
        (false, target) => ("xclip", vec!["-selection", "clipboard", "-target", target.unwrap_or("TARGETS"), "-out"]),
    }
}

/// The local paths in a `text/uri-list`.
fn file_paths(list: &str) -> Vec<PathBuf> {
    list.lines()
        .filter_map(|line| line.trim().strip_prefix("file://"))
        // `file:///p` or `file://host/p`.
        .filter_map(|rest| rest.find('/').map(|i| &rest[i..]))
        .map(|path| PathBuf::from(percent_decode(path)))
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let hex = |i: usize| bytes.get(i).and_then(|&b| (b as char).to_digit(16));
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match (bytes[i], hex(i + 1), hex(i + 2)) {
            (b'%', Some(high), Some(low)) => {
                out.push((high * 16 + low) as u8);
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Runs a clipboard tool, stopping it after `TIMEOUT`.
fn run(program: &str, args: &[&str]) -> Result<Output, String> {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound if program == "osascript" => format!("{program} not found"),
            std::io::ErrorKind::NotFound => format!("{program} not found; install wl-clipboard (Wayland) or xclip (X11)"),
            _ => format!("{program}: {e}"),
        })?;
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(TIMEOUT) {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(e)) => Err(format!("{program}: {e}")),
        Err(_) => {
            // SAFETY: a plain signal; the pid stays the child's until it
            // exits and is reaped.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            Err(format!("{program} did not answer within {} seconds", TIMEOUT.as_secs()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_macos_output() {
        assert!(parse_macos("\n").unwrap().is_none());
        match parse_macos("files\n/a/b.png\n/c d/e.txt\n").unwrap() {
            Some(Clip::Files(paths)) => assert_eq!(paths, vec![PathBuf::from("/a/b.png"), PathBuf::from("/c d/e.txt")]),
            _ => panic!("expected files"),
        }
        match parse_macos("image/png\naGk=\n").unwrap() {
            Some(Clip::Image(image)) => assert_eq!((image.media_type.as_str(), image.data.as_slice()), ("image/png", &b"hi"[..])),
            _ => panic!("expected an image"),
        }
        assert!(parse_macos("image/png\n!!\n").is_err());
    }

    #[test]
    fn reads_file_uris() {
        let list = "# copied\r\nfile:///home/me/a%20b.png\r\nfile://host/tmp/%E4%B8%AD.txt\r\nhttps://example.com/x\r\n";
        assert_eq!(file_paths(list), vec![PathBuf::from("/home/me/a b.png"), PathBuf::from("/tmp/中.txt")]);
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%41"), "%zzA");
    }

    #[test]
    fn builds_clipboard_commands() {
        assert_eq!(clipboard_command(true, Some("image/png")), ("wl-paste", vec!["--no-newline", "--type", "image/png"]));
        assert_eq!(clipboard_command(false, None).1, vec!["-selection", "clipboard", "-target", "TARGETS", "-out"]);
    }

    #[test]
    fn runs_clipboard_tools() {
        assert_eq!(run("sh", &["-c", "printf hi"]).unwrap().stdout, b"hi");
        assert!(!run("sh", &["-c", "exit 1"]).unwrap().status.success());
        assert!(run("harness-no-such-program", &[]).unwrap_err().contains("not found"));
    }
}
