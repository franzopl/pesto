const yencode = require('yencode');
const fs = require('fs');
const path = require('path');

async function main() {
    const args = process.argv.slice(2);
    if (args.length < 1) {
        console.error('Usage: node bench_yencode.js <file> [line_len]');
        process.exit(1);
    }

    const filePath = args[0];
    const lineLen = parseInt(args[1] || '128', 10);

    const data = fs.readFileSync(filePath);
    const size = data.length;

    // Warm up
    yencode.encode(data, lineLen);

    const iterations = size < 1024 * 1024 ? 1000 : (size < 100 * 1024 * 1024 ? 10 : 1);

    const start = process.hrtime.bigint();
    for (let i = 0; i < iterations; i++) {
        const out = yencode.encode(data, lineLen);
        if (out.length === 0 && size > 0) {
            throw new Error('Encoding failed');
        }
    }
    const end = process.hrtime.bigint();

    const elapsedNanosecs = Number(end - start);
    const elapsedSecs = elapsedNanosecs / 1_000_000_000;
    
    const totalBytes = size * iterations;
    const mbps = (totalBytes / 1024 / 1024) / elapsedSecs;

    console.log(mbps.toFixed(2));
}

main().catch(err => {
    console.error(err);
    process.exit(1);
});
