#!/usr/bin/env node
// 统一改版本号：package.json / src-tauri/tauri.conf.json / src-tauri/Cargo.toml / Cargo.lock（lkapi 包）
// 用法：node scripts/set-version.mjs 0.4.1
//
// 发布流程依赖这四处版本号一致（CI 会校验），所以别再手改单个文件。

import fs from "node:fs";
import path from "node:path";

const version = process.argv[2];
if (!version || !/^\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$/.test(version)) {
  console.error("用法: node scripts/set-version.mjs <版本号>，例如 0.4.1");
  process.exit(1);
}

const root = path.resolve(import.meta.dirname, "..");
const read = (p) => fs.readFileSync(path.join(root, p), "utf8");
const write = (p, s) => fs.writeFileSync(path.join(root, p), s);
let changed = 0;

// package.json
{
  const file = "package.json";
  const json = JSON.parse(read(file));
  if (json.version !== version) {
    json.version = version;
    write(file, JSON.stringify(json, null, 2) + "\n");
    changed++;
    console.log(`✓ ${file} -> ${version}`);
  }
}

// tauri.conf.json
{
  const file = "src-tauri/tauri.conf.json";
  const json = JSON.parse(read(file));
  if (json.version !== version) {
    json.version = version;
    write(file, JSON.stringify(json, null, 2) + "\n");
    changed++;
    console.log(`✓ ${file} -> ${version}`);
  }
}

// Cargo.toml（只改 [package] 段的第一个 version）
{
  const file = "src-tauri/Cargo.toml";
  const text = read(file);
  const next = text.replace(/^version = "([^"]+)"$/m, `version = "${version}"`);
  if (next !== text) {
    write(file, next);
    changed++;
    console.log(`✓ ${file} -> ${version}`);
  }
}

// Cargo.lock（lkapi 包自己的条目；不跑 cargo 也能保持一致）
{
  const file = "src-tauri/Cargo.lock";
  const text = read(file);
  const next = text.replace(
    /(\[\[package\]\]\nname = "lkapi"\nversion = ")([^"]+)(")/,
    `$1${version}$3`,
  );
  if (next !== text) {
    write(file, next);
    changed++;
    console.log(`✓ ${file} -> ${version}`);
  }
}

if (changed === 0) console.log("版本号已经是 " + version + "，无需修改");
