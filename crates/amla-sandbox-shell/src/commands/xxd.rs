//! xxd - make a hex dump or do the reverse

use amla_scheduler::Exit;
use smallvec::SmallVec;

use crate::CmdContext;
use crate::io_handle::IoError;

use super::CommandResult;

/// xxd options
struct Options {
    /// Bytes per line (default 16)
    cols: usize,
    /// Grouping size in bytes (default 2)
    group: usize,
    /// Use uppercase hex
    uppercase: bool,
    /// Plain hex dump (no address or ASCII)
    plain: bool,
    /// C include style output
    c_include: bool,
    /// Reverse operation (hex to binary)
    reverse: bool,
    /// Limit output length
    length: Option<usize>,
    /// Seek offset
    seek: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            cols: 16,
            group: 2,
            uppercase: false,
            plain: false,
            c_include: false,
            reverse: false,
            length: None,
            seek: 0,
        }
    }
}

const HELP: &str = "\
xxd - make a hex dump or do the reverse

Usage: xxd [OPTIONS] [FILE]

Options:
  -h, --help     Show this help message
  -r             Reverse operation: convert hex dump to binary
  -p             Plain hex dump without addresses or ASCII
  -i             C include file style output
  -u             Use uppercase hex letters
  -c <cols>      Bytes per line (default 16)
  -g <bytes>     Group size in bytes (default 2, 0 for no grouping)
  -l <len>       Stop after <len> bytes
  -s <offset>    Start at byte offset

Examples:
  echo hello | xxd           Hex dump of 'hello'
  xxd -p file.bin            Plain hex output
  xxd -i data.bin            C include format
  xxd -r dump.txt            Convert hex dump back to binary
  xxd -c 8 file.bin          8 bytes per line
";

/// xxd [-r] [-p] [-i] [-u] [-c cols] [-g bytes] [-l len] [-s offset] [file]
///
/// Make a hex dump of a file or stdin, or reverse it.
pub async fn run(ctx: CmdContext) -> CommandResult {
    let mut opts = Options::default();
    let mut files: SmallVec<[String; 2]> = SmallVec::new();

    // Parse arguments
    let mut parser = ctx.arg_parser();
    loop {
        match parser.next() {
            Ok(Some(lexopt::Arg::Short('h'))) | Ok(Some(lexopt::Arg::Long("help"))) => {
                ctx.stdout_write_all(HELP.as_bytes()).await?;
                return Ok(Exit::success());
            }
            Ok(Some(lexopt::Arg::Short('r'))) => opts.reverse = true,
            Ok(Some(lexopt::Arg::Short('p'))) => opts.plain = true,
            Ok(Some(lexopt::Arg::Short('i'))) => opts.c_include = true,
            Ok(Some(lexopt::Arg::Short('u'))) => opts.uppercase = true,
            Ok(Some(lexopt::Arg::Short('c'))) => {
                if let Ok(val) = parser.value()
                    && let Ok(n) = val.to_string_lossy().parse::<usize>()
                {
                    opts.cols = n.max(1);
                }
            }
            Ok(Some(lexopt::Arg::Short('g'))) => {
                if let Ok(val) = parser.value()
                    && let Ok(n) = val.to_string_lossy().parse::<usize>()
                {
                    opts.group = n;
                }
            }
            Ok(Some(lexopt::Arg::Short('l'))) => {
                if let Ok(val) = parser.value()
                    && let Ok(n) = val.to_string_lossy().parse::<usize>()
                {
                    opts.length = Some(n);
                }
            }
            Ok(Some(lexopt::Arg::Short('s'))) => {
                if let Ok(val) = parser.value()
                    && let Ok(n) = val.to_string_lossy().parse::<usize>()
                {
                    opts.seek = n;
                }
            }
            Ok(Some(lexopt::Arg::Value(val))) => {
                files.push(val.to_string_lossy().into_owned());
            }
            Ok(Some(_)) => {} // Ignore unknown options
            Ok(None) => break,
            Err(_) => break,
        }
    }

    if opts.reverse {
        reverse_xxd(&ctx, &files, &opts).await
    } else {
        forward_xxd(&ctx, &files, &opts).await
    }
}

/// Forward xxd: binary to hex dump
async fn forward_xxd(ctx: &CmdContext, files: &[String], opts: &Options) -> CommandResult {
    let handle = if files.is_empty() {
        ctx.stdin.clone()
    } else {
        match ctx.open_or_stdin(&files[0]).await {
            Ok(h) => h,
            Err(e) => {
                ctx.eprintln(&format!("xxd: {}: {e}", files[0])).await?;
                return Ok(Exit::code(1));
            }
        }
    };

    // Get variable name for C include mode
    let var_name = if opts.c_include && !files.is_empty() {
        sanitize_c_name(&files[0])
    } else {
        "stdin".to_string()
    };

    let mut buffer = [0u8; 4096];
    let mut bytes_output: usize = 0;
    let max_bytes = opts.length.unwrap_or(usize::MAX);

    // Skip initial bytes if seeking
    let mut skip_remaining = opts.seek;
    while skip_remaining > 0 {
        let to_read = skip_remaining.min(buffer.len());
        let n = handle.read(&mut buffer[..to_read]).await?;
        if n == 0 {
            break;
        }
        skip_remaining -= n;
    }

    // Offset for display starts at seek position
    let mut offset: usize = opts.seek;

    // Collect data for processing
    let mut pending: Vec<u8> = Vec::new();

    // C include header
    if opts.c_include {
        ctx.println(&format!("unsigned char {var_name}[] = {{"))
            .await?;
    }

    loop {
        let n = handle.read(&mut buffer).await?;
        if n == 0 {
            break;
        }

        let remaining = max_bytes - bytes_output;
        let take = n.min(remaining);
        pending.extend_from_slice(&buffer[..take]);
        bytes_output += take;

        // Process complete lines
        while pending.len() >= opts.cols {
            let line: Vec<u8> = pending.drain(..opts.cols).collect();
            output_line(ctx, &line, offset, opts, &var_name).await?;
            offset += opts.cols;
        }

        if bytes_output >= max_bytes {
            break;
        }
    }

    // Output remaining bytes
    if !pending.is_empty() {
        output_line(ctx, &pending, offset, opts, &var_name).await?;
    }

    // C include footer
    if opts.c_include {
        ctx.println("};").await?;
        ctx.println(&format!("unsigned int {var_name}_len = {bytes_output};"))
            .await?;
    }

    // Flush stdout to ensure buffered output is emitted
    ctx.stdout.flush().await?;

    Ok(Exit::success())
}

/// Output a single line of hex dump
async fn output_line(
    ctx: &CmdContext,
    data: &[u8],
    offset: usize,
    opts: &Options,
    _var_name: &str,
) -> Result<(), IoError> {
    let hex_chars: &[u8] = if opts.uppercase {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };

    if opts.c_include {
        // C include style: "  0x48, 0x65, 0x6c, 0x6c, 0x6f,"
        let mut line = String::from("  ");
        for (i, &byte) in data.iter().enumerate() {
            if i > 0 {
                line.push_str(", ");
            }
            line.push_str(&format!("0x{byte:02x}"));
        }
        line.push(',');
        ctx.println(&line).await?;
    } else if opts.plain {
        // Plain hex: just hex bytes
        let mut hex = String::with_capacity(data.len() * 2);
        for &byte in data {
            hex.push(hex_chars[(byte >> 4) as usize] as char);
            hex.push(hex_chars[(byte & 0x0f) as usize] as char);
        }
        ctx.println(&hex).await?;
    } else {
        // Standard xxd format: "00000000: 4865 6c6c 6f20 576f 726c 640a            Hello World."
        let mut line = format!("{offset:08x}: ");

        // Hex part with grouping
        let mut hex_part = String::new();
        for (i, &byte) in data.iter().enumerate() {
            hex_part.push(hex_chars[(byte >> 4) as usize] as char);
            hex_part.push(hex_chars[(byte & 0x0f) as usize] as char);

            // Add space after group (if grouping enabled)
            if opts.group > 0 && (i + 1) % opts.group == 0 && i + 1 < opts.cols {
                hex_part.push(' ');
            }
        }

        // Pad hex part to full width
        let groups_per_line = if opts.group > 0 {
            opts.cols.div_ceil(opts.group)
        } else {
            1
        };
        let expected_hex_len = opts.cols * 2 + groups_per_line.saturating_sub(1);
        while hex_part.len() < expected_hex_len {
            hex_part.push(' ');
        }

        line.push_str(&hex_part);
        line.push_str("  ");

        // ASCII part
        for &byte in data {
            if (0x20..=0x7e).contains(&byte) {
                line.push(byte as char);
            } else {
                line.push('.');
            }
        }

        ctx.println(&line).await?;
    }

    Ok(())
}

/// Reverse xxd: hex dump to binary
async fn reverse_xxd(ctx: &CmdContext, files: &[String], opts: &Options) -> CommandResult {
    let handle = if files.is_empty() {
        ctx.stdin.clone()
    } else {
        match ctx.open_or_stdin(&files[0]).await {
            Ok(h) => h,
            Err(e) => {
                ctx.eprintln(&format!("xxd: {}: {e}", files[0])).await?;
                return Ok(Exit::code(1));
            }
        }
    };

    let mut buffer = [0u8; 4096];
    let mut pending = Vec::new();
    let mut output_buf = Vec::new();

    loop {
        let n = handle.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&buffer[..n]);
    }

    let input = String::from_utf8_lossy(&pending);

    if opts.plain {
        // Plain mode: just hex chars
        parse_plain_hex(&input, &mut output_buf);
    } else {
        // Standard mode: parse xxd output format
        parse_xxd_hex(&input, &mut output_buf);
    }

    ctx.stdout_write_all(&output_buf).await?;

    // Flush stdout to ensure buffered output is emitted
    ctx.stdout.flush().await?;

    Ok(Exit::success())
}

/// Parse plain hex (no addresses)
fn parse_plain_hex(input: &str, output: &mut Vec<u8>) {
    let mut chars = input.chars().filter(|c| c.is_ascii_hexdigit());
    loop {
        let high = chars.next();
        let low = chars.next();
        match (high, low) {
            (Some(h), Some(l)) => {
                if let (Some(hv), Some(lv)) = (h.to_digit(16), l.to_digit(16)) {
                    // Safe: hv and lv are 0-15, so (hv << 4) | lv fits in u8
                    #[allow(clippy::cast_possible_truncation)]
                    output.push(((hv << 4) | lv) as u8);
                }
            }
            _ => break,
        }
    }
}

/// Parse standard xxd format (with addresses)
fn parse_xxd_hex(input: &str, output: &mut Vec<u8>) {
    for line in input.lines() {
        // Skip empty lines
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Find the colon after the address
        let hex_part = if let Some(colon_pos) = line.find(':') {
            &line[colon_pos + 1..]
        } else {
            // No address, treat as plain hex
            line
        };

        // Find where ASCII part starts (two spaces before ASCII)
        let hex_only = if let Some(ascii_start) = hex_part.find("  ") {
            &hex_part[..ascii_start]
        } else {
            hex_part
        };

        // Parse hex pairs, skipping spaces
        parse_plain_hex(hex_only, output);
    }
}

/// Sanitize filename for C variable name
fn sanitize_c_name(path: &str) -> String {
    // Get basename
    let name = path.rsplit('/').next().unwrap_or(path);
    // Replace non-alphanumeric with underscore
    let sanitized: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    // Ensure doesn't start with digit
    if sanitized.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        format!("_{sanitized}")
    } else {
        sanitized
    }
}
