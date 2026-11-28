#!/usr/bin/env node
// 校验「签名私钥」与「tauri.conf.json 里的 pubkey」是不是同一对。
//
// 这是自动更新最容易静默失败的地方：私钥换了、pubkey 没换，CI 照常出包，
// 但客户端拿到 latest.json 后验签失败，用户只会看到「检查更新失败」。
// 所以发布前在这里直接判死。
//
// 用法：
//   TAURI_SIGNING_PRIVATE_KEY=... TAURI_SIGNING_PRIVATE_KEY_PASSWORD=... node scripts/verify-updater-key.mjs

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const root = path.resolve(import.meta.dirname, "..");
const conf = JSON.parse(fs.readFileSync(path.join(root, "src-tauri/tauri.conf.json"), "utf8"));
const pubkeyB64 = conf?.plugins?.updater?.pubkey;
if (!pubkeyB64) {
  console.error("tauri.conf.json 缺少 plugins.updater.pubkey");
  process.exit(1);
}

/** 从 minisign/rsign 的 base64 文本里取出 key id（8 字节）。 */
function keyIdFromPublicKey(b64) {
  const text = Buffer.from(b64, "base64").toString("utf8");
  const payload = text.split("\n").map((l) => l.trim()).filter(Boolean).pop();
  const bin = Buffer.from(payload, "base64");
  return bin.subarray(2, 10).toString("hex");
}

/** 从签名文件（base64 包裹的 minisign 文本）里取出 key id。 */
function keyIdFromSignature(sigFile) {
  const text = Buffer.from(fs.readFileSync(sigFile, "utf8").trim(), "base64").toString("utf8");
  const payload = text.split("\n").map((l) => l.trim()).filter(Boolean)[1];
  const bin = Buffer.from(payload, "base64");
  return bin.subarray(2, 10).toString("hex");
}

if (!process.env.TAURI_SIGNING_PRIVATE_KEY) {
  console.error("未设置 TAURI_SIGNING_PRIVATE_KEY，无法校验签名私钥");
  process.exit(1);
}

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "lkapi-key-"));
const probe = path.join(tmp, "probe.txt");
fs.writeFileSync(probe, "lkapi updater key probe\n");

const cli = path.join(root, "node_modules", "@tauri-apps", "cli", "tauri.js");
const run = (args) =>
  execFileSync(process.execPath, [cli, ...args], {
    cwd: root,
    stdio: "pipe",
    // 私钥是空密码时也必须显式给空串，否则 CLI 会交互式等待输入而挂住
    env: {
      ...process.env,
      CI: "true",
      TAURI_SIGNING_PRIVATE_KEY_PASSWORD: process.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD ?? "",
    },
  });

try {
  run(["signer", "sign", probe]);
} catch (error) {
  console.error("调用 tauri signer 失败:", error.stdout?.toString() || error.message);
  fs.rmSync(tmp, { recursive: true, force: true });
  process.exit(1);
}

const sigFile = `${probe}.sig`;
if (!fs.existsSync(sigFile)) {
  console.error("签名没有产出 .sig 文件");
  fs.rmSync(tmp, { recursive: true, force: true });
  process.exit(1);
}

const configId = keyIdFromPublicKey(pubkeyB64);
const signatureId = keyIdFromSignature(sigFile);
fs.rmSync(tmp, { recursive: true, force: true });

if (configId !== signatureId) {
  console.error(
    `签名私钥与 tauri.conf.json 的 pubkey 不匹配：\n  pubkey id      = ${configId}\n  私钥签名 id    = ${signatureId}\n` +
      "请把 TAURI_SIGNING_PRIVATE_KEY 换成与 pubkey 配套的私钥，或同步更新 pubkey。",
  );
  process.exit(1);
}

console.log(`✓ 签名私钥与 pubkey 匹配（key id ${configId}）`);
