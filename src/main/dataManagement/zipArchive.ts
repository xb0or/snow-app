import { deflateRawSync, inflateRawSync } from "node:zlib";

const LOCAL_FILE_SIGNATURE = 0x04034b50;
const CENTRAL_FILE_SIGNATURE = 0x02014b50;
const END_OF_CENTRAL_DIRECTORY_SIGNATURE = 0x06054b50;
const REDACTED_PATH_SEGMENTS = new Set(["", ".", ".."]);

export type ZipEntry = {
  name: string;
  data: Buffer;
};

export type ZipReadLimits = {
  maxEntries?: number;
  maxEntryBytes?: number;
  maxTotalBytes?: number;
};

const DEFAULT_LIMITS: Required<ZipReadLimits> = {
  maxEntries: 256,
  maxEntryBytes: 512 * 1024 * 1024,
  maxTotalBytes: 1024 * 1024 * 1024,
};

const crc32 = (value: Buffer): number => {
  let crc = 0xffffffff;
  for (const byte of value) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit += 1) {
      crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1));
    }
  }
  return (crc ^ 0xffffffff) >>> 0;
};

const assertSafeArchivePath = (name: string): string => {
  if (!name || name.includes("\\") || name.startsWith("/") || /^[A-Za-z]:/.test(name)) {
    throw new Error(`Unsafe ZIP entry path: ${name}`);
  }
  const segments = name.split("/");
  if (segments.some((segment) => REDACTED_PATH_SEGMENTS.has(segment))) {
    throw new Error(`Unsafe ZIP entry path: ${name}`);
  }
  return name;
};

const u16 = (buffer: Buffer, offset: number): number => buffer.readUInt16LE(offset);
const u32 = (buffer: Buffer, offset: number): number => buffer.readUInt32LE(offset);

export const createZipArchive = (entries: ZipEntry[]): Buffer => {
  if (entries.length === 0 || entries.length > DEFAULT_LIMITS.maxEntries) {
    throw new Error("ZIP archive must contain 1–256 entries");
  }
  const localParts: Buffer[] = [];
  const centralParts: Buffer[] = [];
  let offset = 0;
  const names = new Set<string>();

  for (const entry of entries) {
    const name = assertSafeArchivePath(entry.name);
    if (names.has(name)) {
      throw new Error(`Duplicate ZIP entry: ${name}`);
    }
    names.add(name);
    const nameBuffer = Buffer.from(name, "utf8");
    const data = Buffer.isBuffer(entry.data) ? entry.data : Buffer.from(entry.data);
    const compressed = deflateRawSync(data, { level: 6 });
    const checksum = crc32(data);
    const local = Buffer.alloc(30 + nameBuffer.length + compressed.length);
    local.writeUInt32LE(LOCAL_FILE_SIGNATURE, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(0, 6);
    local.writeUInt16LE(8, 8);
    local.writeUInt16LE(0, 10);
    local.writeUInt16LE(0, 12);
    local.writeUInt32LE(checksum, 14);
    local.writeUInt32LE(compressed.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(nameBuffer.length, 26);
    local.writeUInt16LE(0, 28);
    nameBuffer.copy(local, 30);
    compressed.copy(local, 30 + nameBuffer.length);
    localParts.push(local);

    const central = Buffer.alloc(46 + nameBuffer.length);
    central.writeUInt32LE(CENTRAL_FILE_SIGNATURE, 0);
    central.writeUInt16LE(20, 4);
    central.writeUInt16LE(20, 6);
    central.writeUInt16LE(0, 8);
    central.writeUInt16LE(8, 10);
    central.writeUInt16LE(0, 12);
    central.writeUInt16LE(0, 14);
    central.writeUInt32LE(checksum, 16);
    central.writeUInt32LE(compressed.length, 20);
    central.writeUInt32LE(data.length, 24);
    central.writeUInt16LE(nameBuffer.length, 28);
    central.writeUInt16LE(0, 30);
    central.writeUInt16LE(0, 32);
    central.writeUInt16LE(0, 34);
    central.writeUInt16LE(0, 36);
    central.writeUInt32LE(0, 38);
    central.writeUInt32LE(offset, 42);
    nameBuffer.copy(central, 46);
    centralParts.push(central);
    offset += local.length;
  }

  const centralDirectory = Buffer.concat(centralParts);
  const localDirectory = Buffer.concat(localParts);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(END_OF_CENTRAL_DIRECTORY_SIGNATURE, 0);
  end.writeUInt16LE(0, 4);
  end.writeUInt16LE(0, 6);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(centralDirectory.length, 12);
  end.writeUInt32LE(localDirectory.length, 16);
  end.writeUInt16LE(0, 20);
  return Buffer.concat([localDirectory, centralDirectory, end]);
};

const findEndOfCentralDirectory = (buffer: Buffer): number => {
  const minimum = Math.max(0, buffer.length - 22 - 0xffff);
  for (let offset = buffer.length - 22; offset >= minimum; offset -= 1) {
    if (u32(buffer, offset) === END_OF_CENTRAL_DIRECTORY_SIGNATURE) {
      return offset;
    }
  }
  throw new Error("ZIP end-of-central-directory record is missing");
};

export const readZipArchive = (
  archive: Buffer,
  suppliedLimits: ZipReadLimits = {}
): Map<string, Buffer> => {
  const limits = { ...DEFAULT_LIMITS, ...suppliedLimits };
  if (!Buffer.isBuffer(archive) || archive.length < 22) {
    throw new Error("ZIP archive is empty or truncated");
  }
  const endOffset = findEndOfCentralDirectory(archive);
  const entries = u16(archive, endOffset + 10);
  const centralSize = u32(archive, endOffset + 12);
  const centralOffset = u32(archive, endOffset + 16);
  if (entries === 0 || entries > limits.maxEntries) {
    throw new Error("ZIP archive contains too many entries");
  }
  if (centralOffset + centralSize > endOffset || centralOffset + centralSize > archive.length) {
    throw new Error("ZIP central directory is outside the archive");
  }

  const result = new Map<string, Buffer>();
  let cursor = centralOffset;
  let totalBytes = 0;
  for (let index = 0; index < entries; index += 1) {
    if (cursor + 46 > archive.length || u32(archive, cursor) !== CENTRAL_FILE_SIGNATURE) {
      throw new Error("ZIP central directory entry is invalid");
    }
    const flags = u16(archive, cursor + 8);
    const method = u16(archive, cursor + 10);
    const checksum = u32(archive, cursor + 16);
    const compressedSize = u32(archive, cursor + 20);
    const uncompressedSize = u32(archive, cursor + 24);
    const nameLength = u16(archive, cursor + 28);
    const extraLength = u16(archive, cursor + 30);
    const commentLength = u16(archive, cursor + 32);
    const localOffset = u32(archive, cursor + 42);
    const nameStart = cursor + 46;
    const name = assertSafeArchivePath(
      archive.subarray(nameStart, nameStart + nameLength).toString("utf8")
    );
    cursor = nameStart + nameLength + extraLength + commentLength;
    if (flags & 0x1) {
      throw new Error(`Encrypted ZIP entries are not supported: ${name}`);
    }
    if (uncompressedSize > limits.maxEntryBytes || totalBytes + uncompressedSize > limits.maxTotalBytes) {
      throw new Error("ZIP archive exceeds extraction limits");
    }
    if (localOffset + 30 > archive.length || u32(archive, localOffset) !== LOCAL_FILE_SIGNATURE) {
      throw new Error(`ZIP local entry is invalid: ${name}`);
    }
    const localNameLength = u16(archive, localOffset + 26);
    const localExtraLength = u16(archive, localOffset + 28);
    const dataStart = localOffset + 30 + localNameLength + localExtraLength;
    const dataEnd = dataStart + compressedSize;
    if (dataEnd > archive.length) {
      throw new Error(`ZIP entry is truncated: ${name}`);
    }
    const compressed = archive.subarray(dataStart, dataEnd);
    let data: Buffer;
    if (method === 0) {
      data = Buffer.from(compressed);
    } else if (method === 8) {
      data = inflateRawSync(compressed, {
        finishFlush: 2,
        maxOutputLength: Math.min(limits.maxEntryBytes, limits.maxTotalBytes - totalBytes),
      });
    } else {
      throw new Error(`Unsupported ZIP compression method ${method}: ${name}`);
    }
    if (data.length !== uncompressedSize || crc32(data) !== checksum) {
      throw new Error(`ZIP checksum mismatch: ${name}`);
    }
    if (result.has(name)) {
      throw new Error(`Duplicate ZIP entry: ${name}`);
    }
    result.set(name, data);
    totalBytes += data.length;
  }
  return result;
};
