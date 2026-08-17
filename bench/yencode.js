// bench/yencode.js — node-yencode driver, shaped to match the Rust one.
//
// node-yencode is the C++ addon nyuu encodes with, so it is the only
// like-for-like comparison available for pesto's yEnc kernel. For the two
// numbers to mean the same thing the harness has to match:
//
//   * data generated in memory from the same kind of seeded PRNG (not zeroes:
//     NUL is the byte yEnc always has to escape, so a zero-filled buffer is
//     the worst case for both encoders and understates both),
//   * one warmup call before timing,
//   * iterate until a minimum wall time is reached rather than a fixed count,
//     so a 4 KiB input and an 8 MiB input are both sampled meaningfully,
//   * internal high-resolution timer, so node's startup cost is excluded on
//     this side exactly as Rust's is on the other.
//
// Prints one number: MiB/s.
//
// Usage: node yencode.js --size 768000 [--line-len 128] [--min-time 1.0]
//        node yencode.js <file> [line_len]        # legacy positional form

const fs = require('fs');

let yencode;
try {
    yencode = require('yencode');
} catch (e) {
    console.error('yencode module not installed (npm install yencode)');
    process.exit(1);
}

function parseArgs(argv) {
    const opts = { size: 768000, lineLen: 128, minTime: 1.0, file: null };
    for (let i = 0; i < argv.length; i++) {
        switch (argv[i]) {
            case '--size':     opts.size = parseInt(argv[++i], 10); break;
            case '--line-len': opts.lineLen = parseInt(argv[++i], 10); break;
            case '--min-time': opts.minTime = parseFloat(argv[++i]); break;
            default:
                if (!argv[i].startsWith('--') && opts.file === null) {
                    opts.file = argv[i];
                    if (argv[i + 1] && !argv[i + 1].startsWith('--')) {
                        opts.lineLen = parseInt(argv[++i], 10);
                    }
                }
        }
    }
    return opts;
}

// xorshift32 over 32-bit ints, deliberately not the Rust driver's xorshift64.
//
// What has to match between the two harnesses is the byte *distribution*, not
// the byte sequence: yEnc's cost is driven by how many bytes need escaping,
// and both generators are uniform over 0..255, so both encoders escape the
// same fraction of their input. Mirroring the 64-bit generator exactly would
// mean BigInt arithmetic per byte in JavaScript, which is slower than the
// encoder being measured — generating an 8 MiB buffer would then cost more
// than every timed iteration over it combined.
function makeData(len) {
    const out = Buffer.allocUnsafe(len);
    let x = 0x2545f491 >>> 0;
    for (let i = 0; i < len; i++) {
        x ^= x << 13; x >>>= 0;
        x ^= x >>> 17;
        x ^= x << 5;  x >>>= 0;
        out[i] = x & 0xff;
    }
    return out;
}

function main() {
    const opts = parseArgs(process.argv.slice(2));
    const data = opts.file ? fs.readFileSync(opts.file) : makeData(opts.size);
    const size = data.length;

    yencode.encode(data, opts.lineLen);   // warmup

    let iters = 16;
    for (;;) {
        const start = process.hrtime.bigint();
        for (let i = 0; i < iters; i++) {
            const out = yencode.encode(data, opts.lineLen);
            if (out.length === 0 && size > 0) throw new Error('encoding failed');
        }
        const elapsed = Number(process.hrtime.bigint() - start) / 1e9;
        if (elapsed >= opts.minTime || iters >= (1 << 30)) {
            const mib = (size * iters) / 1048576;
            console.log((mib / elapsed).toFixed(2));
            return;
        }
        iters *= 2;
    }
}

main();
