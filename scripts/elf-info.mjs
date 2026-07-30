#!/usr/bin/env node
// Report the ELF facts a release artifact is judged on: architecture, whether it needs an
// interpreter, and how many shared libraries it pulls in.
//
// This exists because readelf, objdump, nm and file are all absent from a stock NixOS host, and a
// missing tool behind a suppressed stderr reads exactly like a clean result. `ldd` is no substitute
// either: on a foreign architecture it cannot report a binary's real linkage.
//
// Usage:  node scripts/elf-info.mjs <binary>
// Output: key=value lines — class, endian, machine, interp, dt_needed
// Exits non-zero if the file is not a 64-bit little-endian ELF.

import { readFileSync } from "node:fs";

const path = process.argv[2];
if (!path) {
  console.error("usage: elf-info.mjs <binary>");
  process.exit(2);
}

const b = readFileSync(path);
const fail = (msg) => {
  console.error(`elf-info: ${msg}`);
  process.exit(1);
};

if (b.length < 64 || b[0] !== 0x7f || b[1] !== 0x45 || b[2] !== 0x4c || b[3] !== 0x46) {
  fail(`${path} is not an ELF file`);
}
if (b[4] !== 2) fail("not ELF64");
if (b[5] !== 1) fail("not little-endian");

// e_machine values we care about; anything else is reported as a raw number rather than guessed at.
const MACHINES = { 0x03: "x86", 0x28: "ARM", 0x3e: "x86-64", 0xb7: "AArch64", 0xf3: "RISC-V" };
const machine = b.readUInt16LE(0x12);

const phoff = Number(b.readBigUInt64LE(0x20));
const phentsize = b.readUInt16LE(0x36);
const phnum = b.readUInt16LE(0x38);

const PT_DYNAMIC = 2;
const PT_INTERP = 3;
const DT_NULL = 0;
const DT_NEEDED = 1;

let interp = "absent";
let dtNeeded = 0;

for (let i = 0; i < phnum; i++) {
  const ph = phoff + i * phentsize;
  if (ph + 56 > b.length) fail("program header table runs past end of file");
  const type = b.readUInt32LE(ph);

  if (type === PT_INTERP) {
    const off = Number(b.readBigUInt64LE(ph + 8));
    const size = Number(b.readBigUInt64LE(ph + 32));
    interp = b.subarray(off, off + size).toString("latin1").replace(/\0+$/, "");
  }

  if (type === PT_DYNAMIC) {
    const off = Number(b.readBigUInt64LE(ph + 8));
    const size = Number(b.readBigUInt64LE(ph + 32));
    // Elf64_Dyn is a pair of 64-bit words; DT_NULL terminates the array.
    for (let d = off; d + 16 <= off + size && d + 16 <= b.length; d += 16) {
      const tag = Number(b.readBigUInt64LE(d));
      if (tag === DT_NULL) break;
      if (tag === DT_NEEDED) dtNeeded++;
    }
  }
}

console.log(`class=ELF64`);
console.log(`endian=little`);
console.log(`machine=${MACHINES[machine] ?? `0x${machine.toString(16)}`}`);
console.log(`interp=${interp}`);
console.log(`dt_needed=${dtNeeded}`);
