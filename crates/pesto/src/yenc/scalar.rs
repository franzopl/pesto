/// Portable yEnc encoder — reference implementation, no SIMD.
///
/// Each byte is shifted by 42 (mod 256). The four critical output values —
/// NUL, LF, CR and `=` — are always escaped as `=` followed by the value
/// shifted by a further 64. TAB and space are escaped only at the start or end
/// of a line (where transports may strip them), and `.` is escaped at the
/// start of a line (to keep clear of NNTP dot-stuffing).
pub fn encode_scalar(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    let line_len = line_len.max(1);
    // Upper bound: every byte could escape (×2) + one CRLF per line.
    out.reserve(data.len() * 2 + (data.len() / line_len + 1) * 2);
    let last = data.len().saturating_sub(1);
    let mut col = 0usize;

    unsafe {
        let out_base = out.as_mut_ptr();
        let mut out_ptr = out_base.add(out.len());

        for (i, &b) in data.iter().enumerate() {
            let e = b.wrapping_add(42);
            let at_line_start = col == 0;
            let at_line_end = col + 1 == line_len || i == last;

            let critical = matches!(e, 0x00 | 0x0A | 0x0D | 0x3D);
            let positional = ((e == 0x09 || e == 0x20) && (at_line_start || at_line_end))
                || (e == 0x2E && at_line_start);

            if critical || positional {
                *out_ptr = b'=';
                *out_ptr.add(1) = e.wrapping_add(64);
                out_ptr = out_ptr.add(2);
            } else {
                *out_ptr = e;
                out_ptr = out_ptr.add(1);
            }

            col += 1;
            if col == line_len {
                *out_ptr = b'\r';
                *out_ptr.add(1) = b'\n';
                out_ptr = out_ptr.add(2);
                col = 0;
            }
        }

        if col != 0 {
            *out_ptr = b'\r';
            *out_ptr.add(1) = b'\n';
            out_ptr = out_ptr.add(2);
        }

        out.set_len(out_ptr.offset_from(out_base) as usize);
    }
}
