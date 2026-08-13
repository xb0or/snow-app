import {
  createCipheriv,
  createDecipheriv,
  createHash,
  randomBytes,
} from "node:crypto";
import { argon2idAsync } from "@noble/hashes/argon2.js";

const ENVELOPE_VERSION = 1;
const KDF_MEMORY_KIB = 64 * 1024;
const KDF_TIME_COST = 3;
const KDF_PARALLELISM = 1;
const KEY_BYTES = 32;
const NONCE_BYTES = 12;
const SALT_BYTES = 16;
const MAX_PASSWORD_BYTES = 1024 * 1024;

type EncryptedEnvelope = {
  version: 1;
  algorithm: "aes-256-gcm";
  kdf: "argon2id";
  kdfParams: {
    memoryKiB: number;
    timeCost: number;
    parallelism: number;
  };
  salt: string;
  nonce: string;
  tag: string;
  ciphertext: string;
};

const passwordBytes = (password: string): Buffer => {
  const value = Buffer.from(password, "utf8");
  if (value.length === 0 || value.length > MAX_PASSWORD_BYTES) {
    throw new Error("Encryption password must contain 1–1,048,576 bytes");
  }
  return value;
};

const deriveKey = async (password: string, salt: Buffer): Promise<Buffer> =>
  Buffer.from(await argon2idAsync(passwordBytes(password), salt, {
    t: KDF_TIME_COST,
    m: KDF_MEMORY_KIB,
    p: KDF_PARALLELISM,
    dkLen: KEY_BYTES,
    asyncTick: 10,
    maxmem: 128 * 1024 * 1024,
  }));

export const encryptBundlePayload = async (payload: Buffer, password: string): Promise<Buffer> => {
  const salt = randomBytes(SALT_BYTES);
  const nonce = randomBytes(NONCE_BYTES);
  const cipher = createCipheriv("aes-256-gcm", await deriveKey(password, salt), nonce);
  const ciphertext = Buffer.concat([cipher.update(payload), cipher.final()]);
  const envelope: EncryptedEnvelope = {
    version: ENVELOPE_VERSION,
    algorithm: "aes-256-gcm",
    kdf: "argon2id",
    kdfParams: {
      memoryKiB: KDF_MEMORY_KIB,
      timeCost: KDF_TIME_COST,
      parallelism: KDF_PARALLELISM,
    },
    salt: salt.toString("base64"),
    nonce: nonce.toString("base64"),
    tag: cipher.getAuthTag().toString("base64"),
    ciphertext: ciphertext.toString("base64"),
  };
  return Buffer.from(JSON.stringify(envelope), "utf8");
};

export const decryptBundlePayload = async (payload: Buffer, password: string): Promise<Buffer> => {
  let envelope: Partial<EncryptedEnvelope>;
  try {
    envelope = JSON.parse(payload.toString("utf8")) as Partial<EncryptedEnvelope>;
  } catch {
    throw new Error("Encrypted package header is invalid");
  }
  if (
    envelope.version !== ENVELOPE_VERSION ||
    envelope.algorithm !== "aes-256-gcm" ||
    envelope.kdf !== "argon2id" ||
    !envelope.kdfParams ||
    envelope.kdfParams.memoryKiB !== KDF_MEMORY_KIB ||
    envelope.kdfParams.timeCost !== KDF_TIME_COST ||
    envelope.kdfParams.parallelism !== KDF_PARALLELISM ||
    typeof envelope.salt !== "string" ||
    typeof envelope.nonce !== "string" ||
    typeof envelope.tag !== "string" ||
    typeof envelope.ciphertext !== "string"
  ) {
    throw new Error("Unsupported encrypted package format");
  }
  try {
    const salt = Buffer.from(envelope.salt, "base64");
    const nonce = Buffer.from(envelope.nonce, "base64");
    const tag = Buffer.from(envelope.tag, "base64");
    const ciphertext = Buffer.from(envelope.ciphertext, "base64");
    if (salt.length !== SALT_BYTES || nonce.length !== NONCE_BYTES || tag.length !== 16) {
      throw new Error("Encrypted package header has invalid lengths");
    }
    const decipher = createDecipheriv("aes-256-gcm", await deriveKey(password, salt), nonce);
    decipher.setAuthTag(tag);
    return Buffer.concat([decipher.update(ciphertext), decipher.final()]);
  } catch {
    throw new Error("Unable to decrypt package; check the password or package integrity");
  }
};

export const sha256Buffer = (value: Buffer | string): string =>
  createHash("sha256").update(value).digest("hex");
