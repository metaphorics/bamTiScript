export const TARGET_IDENTITIES = Object.freeze({
  "x86_64-unknown-linux-gnu": Object.freeze({
    format: "ELF",
    machine: 62,
    architecture: "x86-64",
  }),
  "aarch64-unknown-linux-gnu": Object.freeze({
    format: "ELF",
    machine: 183,
    architecture: "AArch64",
  }),
  "x86_64-apple-darwin": Object.freeze({
    format: "Mach-O",
    cpuType: 0x01000007,
    architecture: "x86-64",
  }),
  "aarch64-apple-darwin": Object.freeze({
    format: "Mach-O",
    cpuType: 0x0100000c,
    architecture: "arm64",
  }),
  "x86_64-pc-windows-msvc": Object.freeze({
    format: "PE",
    machine: 0x8664,
    architecture: "x86-64",
  }),
});

export class ArtifactIdentityError extends Error {
  constructor(target, detail) {
    super(`bamti CLI artifact: ${target} identity mismatch: ${detail}`);
    this.name = "ArtifactIdentityError";
    this.target = target;
  }
}

function identityError(target, detail) {
  throw new ArtifactIdentityError(target, detail);
}

function requireRange(bytes, target, offset, size, label) {
  if (
    !Number.isSafeInteger(offset) ||
    !Number.isSafeInteger(size) ||
    offset < 0 ||
    size < 0 ||
    offset > bytes.length - size
  ) {
    identityError(target, `${label} is truncated or out of bounds`);
  }
}

function assertElfIdentity(bytes, target, expected) {
  requireRange(bytes, target, 0, 4, "ELF magic");
  if (!bytes.subarray(0, 4).equals(Buffer.from([0x7f, 0x45, 0x4c, 0x46]))) {
    identityError(target, "expected ELF magic");
  }
  requireRange(bytes, target, 4, 2, "ELF class and endianness");
  if (bytes[4] !== 2) identityError(target, "expected a 64-bit ELF executable");
  if (bytes[5] !== 1) {
    identityError(target, "expected a little-endian ELF executable");
  }
  requireRange(bytes, target, 0, 64, "ELF64 header");

  const objectType = bytes.readUInt16LE(16);
  if (objectType !== 2 && objectType !== 3) {
    identityError(target, `expected ELF ET_EXEC or ET_DYN, got e_type ${objectType}`);
  }
  const machine = bytes.readUInt16LE(18);
  if (machine !== expected.machine) {
    identityError(
      target,
      `expected ${expected.architecture} ELF machine ${expected.machine}, got ${machine}`,
    );
  }
  const headerSize = bytes.readUInt16LE(52);
  if (headerSize !== 64) {
    identityError(target, `expected a 64-byte ELF64 header, got ${headerSize}`);
  }
  const programHeaderSize = bytes.readUInt16LE(54);
  const programHeaderCount = bytes.readUInt16LE(56);
  if (programHeaderSize < 56 || programHeaderCount === 0) {
    identityError(target, "ELF executable has no complete program header table");
  }
  const programHeaderOffset = bytes.readBigUInt64LE(32);
  if (programHeaderOffset > BigInt(Number.MAX_SAFE_INTEGER)) {
    identityError(target, "ELF program header offset is out of bounds");
  }
  const tableOffset = Number(programHeaderOffset);
  // overflow-safe table size: check multiplication does not overflow safe integer range
  const tableSize = programHeaderSize * programHeaderCount;
  if (!Number.isSafeInteger(tableSize)) {
    identityError(target, "ELF program header table size overflows");
  }
  requireRange(bytes, target, tableOffset, tableSize, "ELF program header table");

  let hasFileBackedLoad = false;
  for (let index = 0; index < programHeaderCount; index += 1) {
    const offset = tableOffset + index * programHeaderSize;
    requireRange(bytes, target, offset, 56, "ELF program header");
    const pType = bytes.readUInt32LE(offset);
    if (pType !== 1) continue; // PT_LOAD = 1
    const pOffset = bytes.readBigUInt64LE(offset + 8);
    const pFilesz = bytes.readBigUInt64LE(offset + 32);
    const pMemsz = bytes.readBigUInt64LE(offset + 40);
    if (pFilesz === 0n) continue;
    if (pMemsz < pFilesz) {
      identityError(target, `ELF PT_LOAD ${index} has p_memsz < p_filesz`);
    }
    if (pOffset > BigInt(Number.MAX_SAFE_INTEGER) || pFilesz > BigInt(Number.MAX_SAFE_INTEGER)) {
      identityError(target, `ELF PT_LOAD ${index} offset/size out of range`);
    }
    const end = pOffset + pFilesz;
    if (end > BigInt(bytes.length)) {
      identityError(target, `ELF PT_LOAD ${index} file-backed range is truncated or out of bounds`);
    }
    // also ensure the range itself is within buffer via requireRange for defense-in-depth when safe
    if (pOffset <= BigInt(Number.MAX_SAFE_INTEGER) && pFilesz <= BigInt(Number.MAX_SAFE_INTEGER)) {
      requireRange(bytes, target, Number(pOffset), Number(pFilesz), `ELF PT_LOAD ${index} segment bytes`);
    }
    hasFileBackedLoad = true;
  }
  if (!hasFileBackedLoad) {
    identityError(target, "ELF executable has no nonempty file-backed PT_LOAD segment");
  }
}

function assertMachOIdentity(bytes, target, expected) {
  requireRange(bytes, target, 0, 4, "Mach-O magic");
  if (bytes.readUInt32LE(0) !== 0xfeedfacf) {
    identityError(target, "expected thin little-endian 64-bit Mach-O magic");
  }
  requireRange(bytes, target, 0, 32, "Mach-O 64-bit header");
  const cpuType = bytes.readUInt32LE(4);
  if (cpuType !== expected.cpuType) {
    identityError(
      target,
      `expected ${expected.architecture} Mach-O CPU type 0x${expected.cpuType.toString(16)}, got 0x${cpuType.toString(16)}`,
    );
  }
  const fileType = bytes.readUInt32LE(12);
  if (fileType !== 2) {
    identityError(target, `expected Mach-O MH_EXECUTE, got filetype ${fileType}`);
  }
  const commandCount = bytes.readUInt32LE(16);
  const commandBytes = bytes.readUInt32LE(20);
  if (commandCount === 0 || commandBytes === 0) {
    identityError(target, "Mach-O executable has no load commands");
  }
  requireRange(bytes, target, 32, commandBytes, "Mach-O load command table");

  const commandEnd = 32 + commandBytes;
  if (commandEnd > bytes.length) {
    identityError(target, "Mach-O load command table exceeds file");
  }
  let cursor = 32;
  let hasFileBackedSegment = false;
  for (let index = 0; index < commandCount; index += 1) {
    requireRange(bytes, target, cursor, 8, "Mach-O load command");
    const command = bytes.readUInt32LE(cursor);
    const commandSize = bytes.readUInt32LE(cursor + 4);
    if (commandSize < 8) {
      identityError(target, `Mach-O load command ${index} has invalid size ${commandSize}`);
    }
    requireRange(bytes, target, cursor, commandSize, `Mach-O load command ${index}`);
    if (cursor + commandSize > commandEnd) {
      identityError(target, `Mach-O load command ${index} exceeds the command table`);
    }
    if (command === 0x19) {
      if (commandSize < 72) {
        identityError(target, "Mach-O LC_SEGMENT_64 command is truncated");
      }
      const vmsize = bytes.readBigUInt64LE(cursor + 32);
      const fileoff = bytes.readBigUInt64LE(cursor + 40);
      const filesize = bytes.readBigUInt64LE(cursor + 48);
      const nsects = bytes.readUInt32LE(cursor + 64);
      // validate cmdsize vs nsects
      const expectedMin = 72 + nsects * 80;
      if (nsects > 0 && commandSize < expectedMin) {
        identityError(
          target,
          `Mach-O LC_SEGMENT_64 ${index} section count ${nsects} exceeds command size ${commandSize}`,
        );
      }
      if (commandSize !== expectedMin) {
        // tolerate larger? strict check: if cmdsize != expected, ensure sections still fit; but require exact for determinism when nsects>0
        // we already ensured >=, now also ensure no extra trailing bytes that would imply malformation? keep permissive for now
      }
      // validate each section
      for (let s = 0; s < nsects; s += 1) {
        const sectOff = cursor + 72 + s * 80;
        requireRange(bytes, target, sectOff, 80, `Mach-O section ${index}.${s}`);
        const sectSize = bytes.readBigUInt64LE(sectOff + 40);
        const sectOffset = bytes.readUInt32LE(sectOff + 48);
        const sectFlags = bytes.readUInt32LE(sectOff + 64);
        const sectType = sectFlags & 0xff;
        const isZeroFill = sectType === 0x01 || sectType === 0x0c || sectType === 0x12;
        if (sectOff + 80 > cursor + commandSize) {
          identityError(target, `Mach-O section ${index}.${s} exceeds its segment command`);
        }
        if (isZeroFill) {
          // zerofill sections have no file bytes; skip bounds
          continue;
        }
        if (sectSize === 0n) continue;
        if (sectSize > BigInt(Number.MAX_SAFE_INTEGER)) {
          identityError(target, `Mach-O section ${index}.${s} size out of range`);
        }
        const sectEnd = BigInt(sectOffset) + sectSize;
        if (sectEnd > BigInt(bytes.length)) {
          identityError(target, `Mach-O section ${index}.${s} file range is truncated or out of bounds`);
        }
        // coherence within segment's file range when segment has file bytes
        // not strictly required, but ensure section lies within its segment file window if segment is file-backed
        if (filesize > 0n) {
          const segEnd = fileoff + filesize;
          if (BigInt(sectOffset) < fileoff || sectEnd > segEnd) {
            // still allow? For strict validation, require section within segment
            // Only enforce when sectSize>0 and not zerofill
            identityError(
              target,
              `Mach-O section ${index}.${s} file range outside its segment's file range`,
            );
          }
        }
      }
      // check segment file-backed coherence
      if (filesize > 0n) {
        if (filesize > vmsize) {
          identityError(target, `Mach-O LC_SEGMENT_64 ${index} filesize exceeds vmsize`);
        }
        if (fileoff > BigInt(Number.MAX_SAFE_INTEGER) || filesize > BigInt(Number.MAX_SAFE_INTEGER)) {
          identityError(target, `Mach-O LC_SEGMENT_64 ${index} fileoff/filesize out of range`);
        }
        const segEnd = fileoff + filesize;
        if (segEnd > BigInt(bytes.length)) {
          identityError(target, `Mach-O LC_SEGMENT_64 ${index} file range is truncated or out of bounds`);
        }
        // require the actual bytes are present
        requireRange(bytes, target, Number(fileoff), Number(filesize), `Mach-O LC_SEGMENT_64 ${index} segment bytes`);
        hasFileBackedSegment = true;
      }
    }
    cursor += commandSize;
  }
  if (cursor !== commandEnd) {
    identityError(target, "Mach-O load command sizes do not match sizeofcmds");
  }
  if (!hasFileBackedSegment) {
    identityError(target, "Mach-O executable has no nonempty file-backed LC_SEGMENT_64");
  }
}

function assertPeIdentity(bytes, target, expected) {
  requireRange(bytes, target, 0, 2, "PE DOS magic");
  if (bytes[0] !== 0x4d || bytes[1] !== 0x5a) {
    identityError(target, "expected DOS MZ magic");
  }
  requireRange(bytes, target, 0, 64, "PE DOS header");
  const peOffset = bytes.readUInt32LE(0x3c);
  requireRange(bytes, target, peOffset, 24, "PE signature and COFF header");
  if (!bytes.subarray(peOffset, peOffset + 4).equals(Buffer.from([0x50, 0x45, 0, 0]))) {
    identityError(target, "expected PE signature");
  }
  const machine = bytes.readUInt16LE(peOffset + 4);
  if (machine !== expected.machine) {
    identityError(
      target,
      `expected ${expected.architecture} PE machine 0x${expected.machine.toString(16)}, got 0x${machine.toString(16)}`,
    );
  }
  const sectionCount = bytes.readUInt16LE(peOffset + 6);
  if (sectionCount === 0) {
    identityError(target, "PE executable has no sections");
  }
  const optionalHeaderSize = bytes.readUInt16LE(peOffset + 20);
  if (optionalHeaderSize < 112) {
    identityError(target, `PE32+ optional header is too small: ${optionalHeaderSize}`);
  }
  const characteristics = bytes.readUInt16LE(peOffset + 22);
  if ((characteristics & 0x0002) === 0) {
    identityError(target, "PE COFF header lacks IMAGE_FILE_EXECUTABLE_IMAGE");
  }
  const optionalHeaderOffset = peOffset + 24;
  requireRange(
    bytes,
    target,
    optionalHeaderOffset,
    optionalHeaderSize,
    "PE optional header",
  );
  const optionalHeaderMagic = bytes.readUInt16LE(optionalHeaderOffset);
  if (optionalHeaderMagic !== 0x020b) {
    identityError(
      target,
      `expected PE32+ optional-header magic 0x20b, got 0x${optionalHeaderMagic.toString(16)}`,
    );
  }
  const sectionTableOffset = optionalHeaderOffset + optionalHeaderSize;
  const sectionTableSize = sectionCount * 40;
  if (!Number.isSafeInteger(sectionTableSize)) {
    identityError(target, "PE section table size overflows");
  }
  requireRange(bytes, target, sectionTableOffset, sectionTableSize, "PE section table");

  let hasFileBackedSection = false;
  for (let i = 0; i < sectionCount; i += 1) {
    const off = sectionTableOffset + i * 40;
    requireRange(bytes, target, off, 40, `PE section ${i}`);
    const sizeOfRawData = bytes.readUInt32LE(off + 16);
    const pointerToRawData = bytes.readUInt32LE(off + 20);
    if (sizeOfRawData === 0) continue;
    // pointer must be non-zero when size is non-zero; zero pointer with size is truncated
    if (pointerToRawData === 0) {
      identityError(target, `PE section ${i} has nonzero raw size but zero file pointer`);
    }
    if (pointerToRawData > bytes.length - sizeOfRawData) {
      identityError(target, `PE section ${i} raw data is truncated or out of bounds`);
    }
    requireRange(bytes, target, pointerToRawData, sizeOfRawData, `PE section ${i} raw data`);
    hasFileBackedSection = true;
  }
  if (!hasFileBackedSection) {
    identityError(target, "PE executable has no file-backed section with raw data");
  }
}

export function assertTargetBinaryIdentity(bytes, target) {
  const expected = TARGET_IDENTITIES[target];
  if (!expected) identityError(target, "unsupported target");
  if (!Buffer.isBuffer(bytes)) identityError(target, "payload is not a Buffer");

  if (expected.format === "ELF") {
    assertElfIdentity(bytes, target, expected);
  } else if (expected.format === "Mach-O") {
    assertMachOIdentity(bytes, target, expected);
  } else {
    assertPeIdentity(bytes, target, expected);
  }
  return Object.freeze({ format: expected.format, architecture: expected.architecture });
}
