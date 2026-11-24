import { createHash } from "node:crypto";

export function sha256Hex(bytes: Buffer): string {
  return createHash("sha256").update(bytes).digest("hex");
}

export function b64url(bytes: Buffer): string {
  return bytes.toString("base64url");
}
