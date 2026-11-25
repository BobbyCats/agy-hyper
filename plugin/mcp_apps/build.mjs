#!/usr/bin/env node
/**
 * Builds self-contained single-file HTML MCP Apps (`tx-diff.html` & `cockpit.html`)
 * bundling `@modelcontextprotocol/ext-apps` + inline CSS/JS with zero external requests.
 */

import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);

function resolveToolingNodeModules() {
  const candidates = [
    process.env.MCP_APPS_NODE_MODULES,
    path.join(__dirname, "node_modules"),
  ].filter(Boolean);
  for (const dir of candidates) {
    if (fs.existsSync(path.join(dir, "esbuild"))) {
      return dir;
    }
  }
  throw new Error(
    "esbuild and @modelcontextprotocol/ext-apps not found. Run `npm install` in plugin/mcp_apps/ or set MCP_APPS_NODE_MODULES."
  );
}

const nodeModulesDir = resolveToolingNodeModules();
const esbuild = require(path.join(nodeModulesDir, "esbuild"));

const themeCss = fs.readFileSync(path.join(__dirname, "src", "theme.css"), "utf8");

function minifyCss(css) {
  return esbuild.transformSync(css, { loader: "css", minify: true }).code.trim();
}

function escapeInlineScript(js) {
  return js.replace(/<\/script/gi, "<\\/script");
}

function buildApp({ name, title, entryJs, extraCss, outFile }) {
  const result = esbuild.buildSync({
    entryPoints: [path.join(__dirname, "src", entryJs)],
    bundle: true,
    minify: true,
    treeShaking: true,
    format: "iife",
    target: ["es2022"],
    nodePaths: [nodeModulesDir],
    write: false,
    legalComments: "none",
  });

  const jsCode = escapeInlineScript(result.outputFiles[0].text);
  const appCss = fs.readFileSync(path.join(__dirname, "src", extraCss), "utf8");
  const combinedCss = minifyCss(`${themeCss}\n${appCss}`);

  const html = `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>${title}</title>
<style>${combinedCss}</style>
</head>
<body>
<div id="app"></div>
<div id="toast" class="toast hidden" role="status" aria-live="polite"></div>
<script>${jsCode}</script>
</body>
</html>
`;

  const outPath = path.join(__dirname, outFile);
  fs.writeFileSync(outPath, html, "utf8");
  const bytes = Buffer.byteLength(html, "utf8");
  console.log(`built ${name} -> ${outFile} (${(bytes / 1024).toFixed(1)} KiB)`);
}

buildApp({
  name: "tx-diff",
  title: "AGY Hyper — WAL Transaction Diff",
  entryJs: "tx-diff.js",
  extraCss: "tx-diff.css",
  outFile: "tx-diff.html",
});

buildApp({
  name: "cockpit",
  title: "AGY Hyper — Telemetry Cockpit",
  entryJs: "cockpit.js",
  extraCss: "cockpit.css",
  outFile: "cockpit.html",
});
