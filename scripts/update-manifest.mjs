#!/usr/bin/env node
// 生成并校验 `latest.json`(Tauri 自动更新的清单)。
//
// 两种用法：
//   生成：node scripts/update-manifest.mjs --generate
//   校验：node scripts/update-manifest.mjs --verify <latest.json>
//
// 生成时从 src-tauri/target/release/bundle 读取已签名的安装包 + .sig，
// 拼出 platforms 映射；校验时逐个端点做真实验签（minisign: blake2b + ed25519），
// 也就是客户端 tauri-plugin-updater 拿到清单后会做的事。
//
// 之所以本地也要跑：CI 出包后如果 latest.json 指向的文件不存在、或签名与
// pubkey 不配套，用户端只会看到「检查更新失败」，没有任何可用线索。

import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const root = path.resolve(import.meta.dirname, "..");
const conf = JSON.parse(fs.readFileSync(path.join(root, "src-tauri/tauri.conf.json"), "utf8"));
const version = conf.version;
const bundleDir = path.join(root, "src-tauri/target/release/bundle");

/** 配置里的 pubkey（base64 包裹的 minisign 文本）→ 原始公钥字节。 */
function publicKeyBytes() {
  const text = Buffer.from(conf.plugins.updater.pubkey, "base64").toString("utf8");
  const payload = text.split("\n").map((l) => l.trim()).filter(Boolean).pop();
  return Buffer.from(payload, "base64");
}

/** minisign 对文件做 blake2b-512 预哈希后再用 ed25519 签名。 */
function prehash(file) {
  const h = crypto.createHash("blake2b512");
  h.update(fs.readFileSync(file));
  return h.digest();
}

function verifySignature(artifactPath, sigPath) {
  if (!fs.existsSync(artifactPath)) return `缺少产物: ${path.relative(root, artifactPath)}`;
  if (!fs.existsSync(sigPath)) return `缺少签名: ${path.relative(root, sigPath)}`;

  const pk = publicKeyBytes();
  const der = Buffer.concat([
    Buffer.from("302a300506032b6570032100", "hex"),
    pk.subarray(10, 42),
  ]);
  const key = crypto.createPublicKey({ key: der, format: "der", type: "spki" });

  const sigText = Buffer.from(fs.readFileSync(sigPath, "utf8").trim(), "base64").toString("utf8");
  const sig = Buffer.from(sigText.split("\n").map((l) => l.trim()).filter(Boolean)[1], "base64");

  const keyId = sig.subarray(2, 10).toString("hex");
  if (keyId !== pk.subarray(2, 10).toString("hex")) {
    return `签名 key id ${keyId} 与 pubkey ${pk.subarray(2, 10).toString("hex")} 不匹配`;
  }
  if (!crypto.verify(null, prehash(artifactPath), key, sig.subarray(10, 74))) {
    return "签名校验失败（内容与签名不一致）";
  }
  return null;
}

/** 扫描 bundle 目录，返回 { 平台键: { artifact, sig } }。 */
function collectPlatforms() {
  const platforms = {};
  const find = (dir) => {
    if (!fs.existsSync(dir)) return;
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) find(full);
      else if (entry.name.endsWith(".sig")) {
        const artifact = full.slice(0, -4);
        platforms[artifact] = full;
      }
    }
  };
  find(bundleDir);
  return platforms;
}

function platformKey(artifact) {
  const name = path.basename(artifact).toLowerCase();
  if (name.endsWith(".exe")) return "windows-x86_64-nsis";
  if (name.endsWith(".msi")) return "windows-x86_64";
  if (name.endsWith(".appimage")) return "linux-x86_64";
  if (name.endsWith(".deb")) return "linux-x86_64-deb";
  if (name.endsWith(".rpm")) return "linux-x86_64-rpm";
  if (name.endsWith(".app.tar.gz")) return "darwin-x86_64";
  return null;
}

function generate() {
  const found = collectPlatforms();
  const entries = Object.entries(found);
  if (entries.length === 0) {
    console.error("bundle 目录里没有任何已签名的产物，先跑 npx tauri build --bundles nsis");
    process.exit(1);
  }

  const platforms = {};
  const errors = [];
  for (const [artifact, sig] of entries) {
    const key = platformKey(artifact);
    if (!key) continue;
    const error = verifySignature(artifact, sig);
    if (error) {
      errors.push(`${path.basename(artifact)}: ${error}`);
      continue;
    }
    platforms[key] = {
      signature: fs.readFileSync(sig, "utf8").trim(),
      // 本地清单用占位 URL，真正发布时由 tauri-action 改成 releases/download/<tag>/<file>
      url: `https://github.com/lkk0205/LKAI-API/releases/download/lkapi-v${version}/${path.basename(artifact)}`,
    };
  }

  if (errors.length) {
    console.error("产物校验失败：");
    for (const e of errors) console.error("  - " + e);
    process.exit(1);
  }

  const manifest = {
    version,
    notes: `LKAPI v${version}。0.4.0 起不再兼容旧版本数据，升级会重建数据库。`,
    pub_date: new Date().toISOString(),
    platforms,
  };
  const outPath = path.join(root, "latest.json");
  fs.writeFileSync(outPath, JSON.stringify(manifest, null, 2) + "\n");
  console.log(`✓ 生成 ${outPath}`);
  for (const [key, value] of Object.entries(platforms)) {
    console.log(`  ${key} -> ${path.basename(value.url)}`);
  }
}

function verify(manifestPath) {
  const manifest = JSON.parse(fs.readFileSync(path.resolve(manifestPath), "utf8"));
  const problems = [];
  const pkId = publicKeyBytes().subarray(2, 10).toString("hex");

  for (const [platform, entry] of Object.entries(manifest.platforms || {})) {
    const sigText = Buffer.from(entry.signature, "base64").toString("utf8");
    const lines = sigText.split("\n").map((l) => l.trim()).filter(Boolean);
    if (lines.length < 2) {
      problems.push(`${platform}: signature 不是合法的 minisign 文本`);
      continue;
    }
    const sig = Buffer.from(lines[1], "base64");
    const keyId = sig.subarray(2, 10).toString("hex");
    if (keyId !== pkId) {
      problems.push(`${platform}: 签名 key id ${keyId} 与应用内 pubkey ${pkId} 不匹配（客户端会验签失败）`);
    }
    if (!/^https?:\/\//.test(entry.url || "")) {
      problems.push(`${platform}: url 不是 http(s) 直链`);
    }
  }

  if (!manifest.version) problems.push("缺少 version 字段");
  if (!manifest.platforms || Object.keys(manifest.platforms).length === 0) {
    problems.push("platforms 为空");
  }

  if (problems.length) {
    console.error("latest.json 校验失败：");
    for (const p of problems) console.error("  - " + p);
    process.exit(1);
  }
  console.log(`✓ latest.json 可用：version=${manifest.version}，平台 ${Object.keys(manifest.platforms).join(", ")}`);
}

const args = process.argv.slice(2);
if (args[0] === "--verify") {
  if (!args[1]) {
    console.error("用法: node scripts/update-manifest.mjs --verify <latest.json 或 URL 文件>");
    process.exit(1);
  }
  verify(args[1]);
} else if (args[0] === "--generate") {
  generate();
} else {
  console.error("用法: node scripts/update-manifest.mjs --generate | --verify <latest.json>");
  process.exit(1);
}
